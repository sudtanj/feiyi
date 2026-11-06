//! Git worktree isolation, as upstream's: a workspace created with
//! `source.kind: "worktree"` (or an agent created with a `worktree` target)
//! gets its own checkout of the repository on a new branch. Upstream keeps
//! worktrees in `~/.paseo/worktrees/<repo hash>/<slug>`; feiyi uses the same
//! layout under its data directory (`<home>/worktrees/<repo hash>/<slug>`), and
//! only paths of that shape count as feiyi's own.
//!
//! Archiving the last workspace of a worktree removes the directory, but never
//! with work in it that git can't restore (see `cleanup::unsaved_work`). The
//! branch always stays, so commits are never lost with the directory.

use crate::cleanup::{unsaved_work, Policy};
use crate::git_actions::{branch_slug_error, resolve_branch, safe_ref};
use crate::manager::{reply, Cmd, Manager, Out};
use crate::projects::run;
use crate::rpc::{normalize_dir, s};
use crate::terminal;
use crate::util::{new_id, now_iso};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SLUG: usize = 50;

/// An error code (upstream's wire code) and a message.
pub(crate) type Fail = (&'static str, String);
/// When the workspaces were archived and the agents archived with them.
pub(crate) type Archived = Result<(String, Vec<String>), Fail>;

fn fail(code: &'static str, message: impl Into<String>) -> Fail {
    (code, message.into())
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    run("git", args, Some(dir), GIT_TIMEOUT).await
}

/// What to create: the fields shared by `workspace.create` (worktree source),
/// `create_paseo_worktree_request` and an agent's `worktree` / `git` options.
pub(crate) struct Spec {
    cwd: String,
    project_id: Option<String>,
    checkout: bool,
    ref_name: Option<String>,
    branch: Option<String>,
    slug: Option<String>,
    pull_request: Option<Result<u64, &'static str>>,
}

struct Created {
    path: String,
    branch: String,
    repo_root: String,
}

struct Plan {
    new_branch: Option<String>,
    start: String,
    branch: String,
}

fn text(v: &Value, key: &str) -> Option<String> {
    s(v, key).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The pull request asked for, from `checkoutSource` (`{ kind: "change_request", forge, number }`)
/// or the legacy `githubPrNumber` / `prNumber`; None when the request names none.
fn pull_request(v: &Value) -> Option<Result<u64, &'static str>> {
    let source = v.get("checkoutSource").filter(|c| !c.is_null());
    if source.is_some_and(|c| s(c, "forge").is_some_and(|f| f != "github")) {
        return Some(Err("feiyi can only check out GitHub pull requests"));
    }
    let number = source.and_then(|c| c.get("number")).or_else(|| v.get("githubPrNumber")).or_else(|| v.get("prNumber")).filter(|n| !n.is_null());
    match number {
        Some(n) => Some(n.as_u64().filter(|n| *n > 0).ok_or("Pull request number must be a positive integer")),
        None => source.map(|_| Err("checkoutSource needs a pull request number")),
    }
}

impl Spec {
    fn from_request(v: &Value) -> Spec {
        Spec {
            cwd: s(v, "cwd").or_else(|| s(v, "projectId")).unwrap_or_default(),
            project_id: s(v, "projectId"),
            checkout: s(v, "action").as_deref() == Some("checkout"),
            ref_name: text(v, "refName").or_else(|| text(v, "baseBranch")),
            branch: text(v, "branchName"),
            slug: text(v, "worktreeSlug"),
            pull_request: pull_request(v),
        }
    }
}

/// The worktree an agent creation asks for (`worktree` target, or the older `git.createWorktree`).
pub(crate) fn agent_spec(msg: &Value, cwd: &str) -> Option<Spec> {
    let name = text(msg, "worktreeName");
    if let Some(target) = msg.get("worktree").filter(|t| !t.is_null()) {
        let mode = s(target, "mode").unwrap_or_default();
        return Some(Spec {
            cwd: cwd.to_string(),
            project_id: None,
            checkout: mode == "checkout-branch",
            ref_name: text(target, "base").or_else(|| text(target, "branch")),
            branch: text(target, "newBranch"),
            slug: name,
            pull_request: (mode == "checkout-pr").then(|| pull_request(target).unwrap_or(Err("checkout-pr needs a pull request number"))),
        });
    }
    let opts = msg.get("git").filter(|g| g.get("createWorktree").and_then(|c| c.as_bool()) == Some(true))?;
    Some(Spec {
        branch: text(opts, "newBranchName"),
        slug: text(opts, "worktreeSlug").or(name),
        cwd: cwd.to_string(),
        ..Spec::from_request(opts)
    })
}

fn slugify(input: &str) -> String {
    let mut slug = String::new();
    for c in input.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.len() <= MAX_SLUG {
        return slug.to_string();
    }
    let cut = &slug[..MAX_SLUG];
    match cut.rfind('-') {
        Some(i) if i > MAX_SLUG / 2 => cut[..i].to_string(),
        _ => cut.trim_end_matches('-').to_string(),
    }
}

fn repo_hash(repo: &Path) -> String {
    crate::snapshot::workspace_id(&repo.to_string_lossy())[3..11].to_string()
}

fn base_dir(home: &Path) -> PathBuf {
    let base = home.join("worktrees");
    std::fs::canonicalize(&base).unwrap_or(base)
}

/// `<home>/worktrees/<hash>/<slug>` when `dir` is that or inside it.
pub(crate) fn owned_root(home: &Path, dir: &str) -> Option<PathBuf> {
    let base = base_dir(home);
    let rel = Path::new(dir).strip_prefix(&base).ok()?;
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let mut parts = rel.components();
    let (hash, slug) = (parts.next()?, parts.next()?);
    Some(base.join(hash.as_os_str()).join(slug.as_os_str()))
}

static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Where the descriptor and status builders, which have no manager at hand, find feiyi's worktrees.
/// The first call wins.
pub(crate) fn set_home(home: &Path) {
    let _ = HOME.set(home.to_path_buf());
}

/// The checkout fields of `ProjectCheckout` a worktree root adds: `(isPaseoOwnedWorktree, mainRepoRoot)`.
/// `mainRepoRoot` is set for any linked worktree and read from its `.git` file, so no git is spawned.
pub(crate) fn checkout_fields(root: &Path) -> (bool, Option<String>) {
    let main = crate::checkout::linked_main_root(root);
    let owned = main.is_some() && HOME.get().is_some_and(|home| owned_root(home, &root.to_string_lossy()).is_some());
    (owned, main.map(|m| m.to_string_lossy().into_owned()))
}

struct Entry {
    path: String,
    head: Option<String>,
    branch: Option<String>,
}

fn parse_worktrees(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.push(Entry { path: path.to_string(), head: None, branch: None });
            continue;
        }
        let Some(last) = entries.last_mut() else { continue };
        if let Some(head) = line.strip_prefix("HEAD ") {
            last.head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            last.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string());
        }
    }
    entries
}

/// The main checkout of the repository `dir` belongs to (also from inside a worktree).
async fn repo_root(dir: &Path) -> Result<PathBuf, Fail> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .await
        .map_err(|_| fail("unknown", format!("Not a git repository: {}", dir.display())))?;
    let common = PathBuf::from(common.trim());
    let parent = if common.file_name().is_some_and(|n| n == ".git") { common.parent().map(Path::to_path_buf) } else { None };
    Ok(parent.unwrap_or(common))
}

async fn check_branch_name(repo: &Path, name: &str) -> Result<(), Fail> {
    if safe_ref(name) && git(repo, &["check-ref-format", &format!("refs/heads/{name}")]).await.is_ok() {
        return Ok(());
    }
    Err(fail("unknown", format!("Invalid branch name: {name}")))
}

/// `name`, or `name-2`, `name-3`, ... when a local branch already has it.
async fn unique_branch(repo: &Path, name: &str) -> Result<String, Fail> {
    for n in 1..=50 {
        let candidate = if n == 1 { name.to_string() } else { format!("{name}-{n}") };
        if git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{candidate}")]).await.is_err() {
            return Ok(candidate);
        }
    }
    Err(fail("unknown", format!("Too many branches named {name}")))
}

/// The ref to cut a new branch from: the local branch or commit, else `origin/<name>`.
async fn start_point(repo: &Path, name: &str) -> Result<String, Fail> {
    if safe_ref(name) {
        for candidate in [name.to_string(), format!("origin/{name}")] {
            if git(repo, &["rev-parse", "--verify", "--quiet", &format!("{candidate}^{{commit}}")]).await.is_ok() {
                return Ok(candidate);
            }
        }
    }
    Err(fail("unknown_branch", format!("Unknown branch: {name}")))
}

async fn plan_branch_off(repo: &Path, spec: &Spec, slug: &str) -> Result<Plan, Fail> {
    let name = spec.branch.clone().unwrap_or_else(|| slug.to_string());
    check_branch_name(repo, &name).await?;
    let base = match &spec.ref_name {
        Some(base) => base.clone(),
        None => crate::checkout::default_branch(repo).await.ok_or_else(|| fail("unknown", "Unable to resolve repository default branch"))?,
    };
    let start = start_point(repo, &base).await?;
    let branch = unique_branch(repo, &name).await?;
    Ok(Plan { new_branch: Some(branch.clone()), start, branch })
}

async fn plan_checkout(repo: &Path, spec: &Spec) -> Result<Plan, Fail> {
    let Some(target) = spec.ref_name.as_deref() else {
        return Err(fail("missing_checkout_target", "action \"checkout\" requires refName or checkoutSource"));
    };
    let resolved = if safe_ref(target) { resolve_branch(repo, target).await } else { None };
    let Some((name, remote_only)) = resolved else {
        return Err(fail("unknown_branch", format!("Unknown branch: {target}")));
    };
    if remote_only {
        git(repo, &["branch", "--track", &name, &format!("origin/{name}")]).await.map_err(|e| fail("unknown", e))?;
    }
    let listing = git(repo, &["worktree", "list", "--porcelain"]).await.map_err(|e| fail("unknown", e))?;
    if parse_worktrees(&listing).iter().any(|w| w.branch.as_deref() == Some(name.as_str())) {
        let branch = unique_branch(repo, &name).await?;
        return Ok(Plan { new_branch: Some(branch.clone()), start: name, branch });
    }
    Ok(Plan { new_branch: None, start: name.clone(), branch: name })
}

/// Fetch the head of pull request `number` from origin and plan a new local branch at it, created
/// together with the worktree so a failed add leaves nothing behind. The branch is named after the PR's head branch (`refName`) when that is a usable name, else `pr-<number>`.
async fn plan_pull_request(repo: &Path, spec: &Spec, number: u64) -> Result<Plan, Fail> {
    let wanted = spec.ref_name.clone().unwrap_or_default();
    let name = if check_branch_name(repo, &wanted).await.is_ok() { wanted } else { format!("pr-{number}") };
    let branch = unique_branch(repo, &name).await?;
    git(repo, &["fetch", "origin", &format!("pull/{number}/head")]).await.map_err(|e| fail("unknown", format!("Cannot fetch pull request {number}: {e}")))?;
    let head = git(repo, &["rev-parse", "FETCH_HEAD"]).await.map_err(|e| fail("unknown", e))?;
    Ok(Plan { new_branch: Some(branch.clone()), start: head.trim().to_string(), branch })
}

async fn create(home: &Path, spec: &Spec) -> Result<Created, Fail> {
    let dir = normalize_dir(&spec.cwd).map_err(|e| fail("unknown", format!("Directory not found: {e}")))?;
    let repo = repo_root(Path::new(&dir)).await?;
    let seed = match (&spec.slug, &spec.branch, &spec.ref_name) {
        (Some(name), _, _) | (None, Some(name), _) => name.clone(),
        (None, None, Some(name)) if spec.checkout => name.clone(),
        _ => format!("worktree-{}", &new_id()[..8]),
    };
    let slug = slugify(&seed);
    if let Some(e) = branch_slug_error(&slug) {
        return Err(fail("unknown", format!("Invalid worktree name: {e}")));
    }
    let plan = if let Some(number) = spec.pull_request {
        plan_pull_request(&repo, spec, number.map_err(|e| fail("unknown", e))?).await?
    } else if spec.checkout {
        plan_checkout(&repo, spec).await?
    } else {
        plan_branch_off(&repo, spec, &slug).await?
    };

    let parent = home.join("worktrees").join(repo_hash(&repo));
    std::fs::create_dir_all(&parent).map_err(|e| fail("unknown", format!("Cannot create {}: {e}", parent.display())))?;
    let mut path = parent.join(&slug);
    let mut n = 1;
    while path.exists() {
        path = parent.join(format!("{slug}-{n}"));
        n += 1;
    }
    let target = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add"];
    if let Some(branch) = &plan.new_branch {
        args.extend(["-b", branch.as_str(), "--no-track"]);
    }
    args.extend([target.as_str(), plan.start.as_str()]);
    git(&repo, &args).await.map_err(|e| fail("unknown", e))?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(Created { path: path.to_string_lossy().into_owned(), branch: plan.branch, repo_root: repo.to_string_lossy().into_owned() })
}

/// Why removing the worktree at `path` could lose work; None when it is safe.
async fn refusal(path: &Path) -> Option<String> {
    if let Some(reason) = unsaved_work(path).await {
        return Some(reason);
    }
    // Commits on a detached HEAD are on no branch, which would outlive the directory.
    match git(path, &["log", "HEAD", "--not", "--branches", "--remotes", "--oneline"]).await {
        Ok(s) if !s.trim().is_empty() => Some("it has commits on a detached HEAD".into()),
        Ok(_) => None,
        Err(e) => Some(format!("git log failed: {e}")),
    }
}

async fn remove(repo: &str, path: &str, force: bool) -> Result<(), String> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    git(Path::new(repo), &args).await.map(|_| ())
}

async fn branch_path(repo: &str, branch: &str) -> Result<String, Fail> {
    let listing = git(Path::new(repo), &["worktree", "list", "--porcelain"]).await.map_err(|e| fail("UNKNOWN", e))?;
    parse_worktrees(&listing).into_iter().find(|w| w.branch.as_deref() == Some(branch)).map(|w| w.path).ok_or_else(|| fail("UNKNOWN", format!("Paseo worktree not found for branch {branch}")))
}

async fn list(home: &Path, cwd: Option<String>) -> Result<Vec<Value>, Fail> {
    let Some(cwd) = cwd else { return Err(fail("UNKNOWN", "cwd or repoRoot is required")) };
    let dir = normalize_dir(&cwd).map_err(|e| fail("UNKNOWN", format!("Directory not found: {e}")))?;
    let repo = repo_root(Path::new(&dir)).await.map_err(|(_, message)| fail("NOT_GIT_REPO", message))?;
    let listing = git(&repo, &["worktree", "list", "--porcelain"]).await.map_err(|e| fail("UNKNOWN", e))?;
    let mine = base_dir(home).join(repo_hash(&repo));
    Ok(parse_worktrees(&listing)
        .into_iter()
        .filter(|w| Path::new(&w.path).starts_with(&mine))
        .map(|w| {
            let created = std::fs::metadata(&w.path)
                .and_then(|m| m.created().or_else(|_| m.modified()))
                .map(|t| humantime::format_rfc3339_millis(t).to_string())
                .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".into());
            json!({ "worktreePath": w.path, "createdAt": created, "branchName": w.branch, "head": w.head })
        })
        .collect())
}

fn archive_payload(rid: &str, archived: Archived) -> Value {
    let mut payload = match archived {
        Ok((_, agents)) => json!({ "success": true, "removedAgents": agents, "error": null }),
        Err((code, message)) => json!({ "success": false, "removedAgents": [], "error": { "code": code, "message": message } }),
    };
    payload["requestId"] = json!(rid);
    json!({ "type": "paseo_worktree_archive_response", "payload": payload })
}

impl Manager {
    pub(crate) fn handle_worktree_rpc(&mut self, kind: &str, msg: &Value, rid: &str, out: &Out) -> bool {
        match kind {
            "create_paseo_worktree_request" => self.create_reply("create_paseo_worktree_response", Spec::from_request(msg), rid, out),
            "paseo_worktree_list_request" => {
                let (home, out, rid) = (self.cfg.home.clone(), out.clone(), rid.to_string());
                let cwd = s(msg, "repoRoot").or_else(|| s(msg, "cwd"));
                tokio::spawn(async move {
                    let payload = match list(&home, cwd).await {
                        Ok(worktrees) => json!({ "worktrees": worktrees, "error": null, "requestId": rid }),
                        Err((code, message)) => json!({ "worktrees": [], "error": { "code": code, "message": message }, "requestId": rid }),
                    };
                    reply(&out, json!({ "type": "paseo_worktree_list_response", "payload": payload }));
                });
            }
            "paseo_worktree_archive_request" => self.archive_worktree_request(msg, rid, out),
            _ => return false,
        }
        true
    }

    /// Create the worktree in the background, then run `done` on the manager
    /// with the id of the workspace now backed by it.
    fn spawn_create(&self, spec: Spec, done: impl FnOnce(&mut Manager, Result<String, Fail>) + Send + 'static) {
        let (home, cmds) = (self.cfg.home.clone(), self.cmds_tx.clone());
        tokio::spawn(async move {
            let created = create(&home, &spec).await;
            let _ = cmds.send(Cmd::Run(Box::new(move |m: &mut Manager| {
                let workspace = created.map(|c| m.register_worktree(&spec, &c));
                done(m, workspace);
            })));
        });
    }

    fn register_worktree(&mut self, spec: &Spec, created: &Created) -> String {
        let root = spec.project_id.clone().filter(|p| self.projects.iter().any(|r| &r.root_path == p)).unwrap_or_else(|| self.project_root_for(&created.repo_root));
        let id = self.create_workspace(&root, &created.path);
        let repo = created.repo_root.clone();
        self.update_workspace(&id, |w| w.main_repo_root = Some(repo));
        crate::log!("workspace {id} created in worktree {} (branch {})", created.path, created.branch);
        id
    }

    fn create_reply(&mut self, response: &'static str, spec: Spec, rid: &str, out: &Out) {
        let (out, rid) = (out.clone(), rid.to_string());
        self.spawn_create(spec, move |m: &mut Manager, created| {
            let payload = match created {
                Ok(id) => json!({ "requestId": rid, "workspace": m.workspace_json(&id), "setupTerminalId": null, "error": null }),
                Err((code, error)) => json!({ "requestId": rid, "workspace": null, "setupTerminalId": null, "error": error, "errorCode": code }),
            };
            reply(&out, json!({ "type": response, "payload": payload }));
        });
    }

    pub(crate) fn create_worktree_workspace(&mut self, source: &Value, rid: &str, out: &Out) {
        self.create_reply("workspace.create.response", Spec::from_request(source), rid, out);
    }

    /// Create the worktree, then the agent inside it.
    pub(crate) fn create_agent_in_worktree(&mut self, spec: Spec, msg: &Value, rid: &str, out: &Out) {
        let (mut msg, rid, out) = (msg.clone(), rid.to_string(), out.clone());
        self.spawn_create(spec, move |m: &mut Manager, created| match created {
            Ok(id) => {
                let directory = m.workspaces.iter().find(|w| w.id == id).map(|w| w.directory.clone()).unwrap_or_default();
                msg["config"]["cwd"] = json!(directory);
                msg["workspaceId"] = json!(id);
                msg["worktree"] = Value::Null;
                msg["git"] = Value::Null;
                m.create_agent(&msg, &rid, &out);
            }
            Err((_, error)) => reply(&out, json!({ "type": "status", "payload": { "status": "agent_create_failed", "requestId": rid, "error": error } })),
        });
    }

    fn archive_worktree_request(&mut self, msg: &Value, rid: &str, out: &Out) {
        let (out, rid) = (out.clone(), rid.to_string());
        let whole = s(msg, "scope").as_deref() == Some("worktree");
        let workspace_id = s(msg, "workspaceId");
        let done = move |_: &mut Manager, archived: Archived| reply(&out, archive_payload(&rid, archived));
        if let Some(path) = s(msg, "worktreePath") {
            return self.archive_at(&path, workspace_id, whole, done);
        }
        let (Some(repo), Some(branch)) = (s(msg, "repoRoot"), s(msg, "branchName")) else {
            return done(self, Err(fail("UNKNOWN", "worktreePath, or repoRoot and branchName is required")));
        };
        let cmds = self.cmds_tx.clone();
        tokio::spawn(async move {
            let found = branch_path(&repo, &branch).await;
            let _ = cmds.send(Cmd::Run(Box::new(move |m: &mut Manager| match found {
                Ok(path) => m.archive_at(&path, workspace_id, whole, done),
                Err(e) => done(m, Err(e)),
            })));
        });
    }

    /// Archive the workspace `workspace_id` (default: the one at `path`), or with
    /// `whole` every workspace at `path`, which must be one of feiyi's worktrees.
    fn archive_at(&mut self, path: &str, workspace_id: Option<String>, whole: bool, done: impl FnOnce(&mut Manager, Archived) + Send + 'static) {
        let path = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
        if whole && owned_root(&self.cfg.home, &path).is_none() {
            return done(self, Err(fail("NOT_ALLOWED", "Worktree is not a Paseo-owned worktree")));
        }
        let at_path: Vec<String> = self.workspaces.iter().filter(|w| w.archived_at.is_none() && w.directory == path).map(|w| w.id.clone()).collect();
        let ids = match workspace_id {
            Some(id) if !whole => vec![id],
            _ if whole => at_path,
            _ => at_path.into_iter().take(1).collect(),
        };
        if ids.is_empty() {
            return done(self, Ok((now_iso(), Vec::new())));
        }
        self.archive_workspaces_checked(ids, done);
    }

    /// The worktree to remove once `ids` are archived: one feiyi created, that
    /// no other active workspace uses, unless deleting files is switched off.
    fn removable_worktree(&self, ids: &[String]) -> Option<(String, String)> {
        let ws = self.workspaces.iter().find(|w| ids.contains(&w.id))?;
        let repo = ws.main_repo_root.clone()?;
        let root = owned_root(&self.cfg.home, &ws.directory)?;
        let shared = self.workspaces.iter().any(|w| w.archived_at.is_none() && !ids.contains(&w.id) && w.directory == ws.directory);
        (self.cfg.delete_project_files != Policy::Never && !shared && root.as_path() == Path::new(&ws.directory)).then(|| (repo, ws.directory.clone()))
    }

    fn archive_ids(&mut self, ids: &[String]) -> Archived {
        let agents: Vec<String> = self.agents.values().filter(|a| a.rec.archived_at.is_none() && a.rec.workspace_id.as_ref().is_some_and(|w| ids.contains(w))).map(|a| a.rec.id.clone()).collect();
        let mut at = now_iso();
        for id in ids {
            at = self.archive_workspace(id).map_err(|e| fail("UNKNOWN", e))?;
            terminal::end_workspace(id);
        }
        Ok((at, agents))
    }

    /// Archive workspaces and their agents. When that frees a worktree feiyi
    /// created, remove it too, but archive nothing if it holds work git can't
    /// restore; `done` then gets the reason. A removal that fails after the
    /// archive keeps the directory and is only logged.
    pub(crate) fn archive_workspaces_checked(&mut self, ids: Vec<String>, done: impl FnOnce(&mut Manager, Archived) + Send + 'static) {
        let Some((repo, path)) = self.removable_worktree(&ids) else {
            let archived = self.archive_ids(&ids);
            return done(self, archived);
        };
        let (policy, cmds) = (self.cfg.delete_project_files, self.cmds_tx.clone());
        tokio::spawn(async move {
            let missing = !Path::new(&path).exists();
            let refused = if policy == Policy::Safe && !missing { refusal(Path::new(&path)).await } else { None };
            let _ = cmds.send(Cmd::Run(Box::new(move |m: &mut Manager| {
                if let Some(reason) = refused {
                    return done(m, Err(fail("NOT_ALLOWED", format!("Worktree not archived: {reason}"))));
                }
                let archived = m.archive_ids(&ids);
                let cmds = m.cmds_tx.clone();
                tokio::spawn(async move {
                    let removed = if missing { git(Path::new(&repo), &["worktree", "prune"]).await.map(|_| ()) } else { remove(&repo, &path, policy == Policy::All).await };
                    match removed {
                        Ok(()) => crate::log!("removed worktree {path}"),
                        Err(e) => crate::warn!("kept worktree {path}: {e}"),
                    }
                    let _ = cmds.send(Cmd::Run(Box::new(move |m: &mut Manager| done(m, archived))));
                });
            })));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_kebab_case_and_capped() {
        assert_eq!(slugify("Fix the Login Bug!"), "fix-the-login-bug");
        assert_eq!(slugify("feature/new_thing"), "feature-new-thing");
        assert_eq!(slugify("--x--"), "x");
        assert_eq!(slugify("***"), "");
        let long = slugify(&"word ".repeat(30));
        assert!(long.len() <= MAX_SLUG && !long.ends_with('-'));
    }

    #[test]
    fn parses_worktree_listing() {
        let text = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /w/x\nHEAD bbb\ndetached\n";
        let entries = parse_worktrees(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, "/w/x");
        assert_eq!(entries[1].head.as_deref(), Some("bbb"));
        assert!(entries[1].branch.is_none());
    }

    #[test]
    fn only_paths_under_the_worktrees_dir_are_owned() {
        let home = Path::new("/nonexistent-feiyi-home");
        let root = home.join("worktrees/abcd1234/brave-fox");
        assert_eq!(owned_root(home, &root.to_string_lossy()), Some(root.clone()));
        assert_eq!(owned_root(home, &root.join("src").to_string_lossy()), Some(root));
        assert_eq!(owned_root(home, "/nonexistent-feiyi-home/worktrees/abcd1234"), None);
        assert_eq!(owned_root(home, "/nonexistent-feiyi-home/worktrees/abcd1234/../../x"), None);
        assert_eq!(owned_root(home, "/somewhere/else/brave-fox"), None);
    }

    #[test]
    fn agent_options_map_to_a_spec() {
        let msg = json!({ "worktreeName": "my-slug", "worktree": { "mode": "branch-off", "newBranch": "feat/x", "base": "main" } });
        let spec = agent_spec(&msg, "/repo").unwrap();
        assert!(!spec.checkout && spec.pull_request.is_none());
        assert_eq!((spec.branch.as_deref(), spec.ref_name.as_deref(), spec.slug.as_deref()), (Some("feat/x"), Some("main"), Some("my-slug")));
        let msg = json!({ "git": { "createWorktree": true, "action": "checkout", "refName": "dev" } });
        let spec = agent_spec(&msg, "/repo").unwrap();
        assert!(spec.checkout);
        assert_eq!(spec.ref_name.as_deref(), Some("dev"));
        assert!(agent_spec(&json!({ "git": { "createWorktree": false } }), "/repo").is_none());
        assert!(agent_spec(&json!({ "worktree": null }), "/repo").is_none());
        assert_eq!(agent_spec(&json!({ "worktree": { "mode": "checkout-pr", "prNumber": 3 } }), "/repo").unwrap().pull_request, Some(Ok(3)));
        assert!(matches!(agent_spec(&json!({ "worktree": { "mode": "checkout-pr" } }), "/repo").unwrap().pull_request, Some(Err(_))));
    }

    #[test]
    fn pull_request_sources() {
        let of = |v: Value| Spec::from_request(&v).pull_request;
        assert_eq!(of(json!({ "checkoutSource": { "kind": "change_request", "forge": "github", "number": 7 } })), Some(Ok(7)));
        assert_eq!(of(json!({ "githubPrNumber": 8 })), Some(Ok(8)));
        assert!(matches!(of(json!({ "githubPrNumber": 0 })), Some(Err(_))));
        assert!(matches!(of(json!({ "githubPrNumber": -1 })), Some(Err(_))));
        assert!(matches!(of(json!({ "githubPrNumber": "3" })), Some(Err(_))));
        assert!(matches!(of(json!({ "checkoutSource": { "forge": "gitlab", "number": 1 } })), Some(Err(_))));
        assert!(matches!(of(json!({ "checkoutSource": { "kind": "change_request" } })), Some(Err(_))));
        assert_eq!(of(json!({ "action": "checkout", "refName": "main" })), None);
    }

    #[test]
    fn archive_payload_has_upstream_shape() {
        let ok = archive_payload("r", Ok(("t".into(), vec!["a".into()])));
        assert_eq!(ok["payload"]["removedAgents"], json!(["a"]));
        assert_eq!(ok["payload"]["requestId"], "r");
        let err = archive_payload("r", Err(("NOT_ALLOWED", "no".into())));
        assert_eq!(err["payload"]["error"]["code"], "NOT_ALLOWED");
        assert_eq!(err["payload"]["success"], false);
    }
}
