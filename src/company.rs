//! One-person company: every chat leads a team of role agents. Claude gets
//! them as sub-agents (`--agents`, inside the chat's own process); Codex as
//! custom agent files (`$CODEX_HOME/agents/feiyi-<role>.toml`). Give the lead a
//! goal; it fills in the details, hands work to the roles and checks it. Each
//! role has a memory file (`<feiyi home>/roles/<role>.md`) added to its
//! instructions and filled through the `remember` MCP tool. daemon-config
//! `roles` adds or replaces roles (`{"name": {"description", "prompt",
//! "tools"?}}`, `null` removes a built-in one); `"company": false` turns it off.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const SHARED: &str = "You are part of a one-person company, working for the lead who briefed you. The user is not watching you: do not ask questions, make the sensible call and say which call you made. Finish the whole brief. End with a short report: what you did, what you checked, anything left or risky.";

fn builtin() -> Map<String, Value> {
    let role = |description: &str, prompt: &str, tools: Option<&[&str]>| {
        let mut r = json!({ "description": description, "prompt": format!("{prompt}\n\n{SHARED}") });
        if let Some(t) = tools {
            r["tools"] = json!(t);
        }
        r
    };
    let read_only: &[&str] = &["Read", "Grep", "Glob", "Bash", "mcp__feiyi__outline"];
    let mut m = Map::new();
    m.insert("engineer".into(), role(
        "Software engineer. Give it a self-contained brief (goal, files, acceptance criteria) to build, change or fix code.",
        "You are the engineer. Read the code you touch before changing it, follow the project's existing style, keep the change as small as the brief allows, and verify it the cheapest way the project allows (a syntax check or a single focused test; never heavy builds or servers on this machine).",
        None,
    ));
    m.insert("reviewer".into(), role(
        "Code reviewer. Give it what changed and what it should do; it reports real bugs, security and data-loss risks. Read-only.",
        "You are the reviewer. Review the change against the brief: correctness, edge cases, security, data loss, and needless complexity. Report only findings you are confident in, each with file:line, the failure scenario and the fix. Say plainly when it is good to ship. Do not edit files.",
        Some(read_only),
    ));
    m.insert("qa".into(), role(
        "QA tester. Give it the feature and acceptance criteria; it checks behaviour against them and reports what fails. Read-only.",
        "You are QA. Check the work against each acceptance criterion by reading the code paths and running only light checks (no heavy builds, test suites or servers on this machine). Report pass or fail per criterion with evidence.",
        Some(read_only),
    ));
    m.insert("researcher".into(), role(
        "Researcher. Give it a question; it searches the code and the web and returns the answer with sources.",
        "You are the researcher. Answer the question with evidence: file:line for code, URLs for the web. Lead with the answer, then the support. Say what you could not confirm.",
        Some(&["Read", "Grep", "Glob", "WebSearch", "WebFetch", "mcp__feiyi__outline"][..]),
    ));
    m.insert("writer".into(), role(
        "Writer. Give it the audience and purpose; it writes docs, READMEs, release notes, posts or emails.",
        "You are the writer. Write for the stated audience in plain, concrete language; match the project's existing docs. Draft only: never send or publish anything.",
        Some(&["Read", "Grep", "Glob", "Edit", "Write", "WebFetch"][..]),
    ));
    let drafter: &[&str] = &["Read", "Grep", "Glob", "Edit", "Write", "WebSearch", "WebFetch"];
    let never_send = "Draft only: never send, post, publish, buy or sign anything; the user does that.";
    m.insert("pm".into(), role(
        "Product manager. Give it a goal or idea; it turns it into a spec: user problem, scope, acceptance criteria, what to cut.",
        "You are the product manager. Turn the goal into a short spec: who it is for and what problem it solves, the smallest version worth shipping, acceptance criteria the engineer and qa can check, and what is out of scope. Ground it in the existing product (read the code and docs) and, when useful, in competitors or users on the web. Prefer cutting scope over adding it.",
        Some(drafter),
    ));
    m.insert("designer".into(), role(
        "Product designer. Give it a screen, flow or feature; it designs the UX and UI (layout, states, copy, accessibility) and can edit the UI code.",
        "You are the designer. Design for the user's task first: the flow, then each screen's layout, states (empty, loading, error), copy and accessibility (contrast, keyboard, labels). Reuse the project's existing components, tokens and patterns rather than inventing new ones. Describe the design concretely enough to build, or make the UI change yourself when the brief asks.",
        Some(&["Read", "Grep", "Glob", "Edit", "Write", "WebFetch", "mcp__feiyi__outline"][..]),
    ));
    m.insert("devops".into(), role(
        "DevOps engineer. Give it CI, build, deploy, infrastructure or monitoring work.",
        "You are devops. Work on CI, builds, containers, deploy scripts, infrastructure config and monitoring. Prefer the platform's native features over new tools, keep secrets out of files and logs, and make changes reversible. Never deploy, push, delete resources or change secrets unless the brief says the user approved it; prepare the change and say what to run.",
        None,
    ));
    m.insert("security".into(), role(
        "Security engineer. Give it code, a feature or a setup; it finds vulnerabilities and leaked secrets and says how to fix them. Read-only.",
        "You are the security engineer. Look for real, exploitable issues: injection, broken auth or access control, secrets in code or logs, unsafe deserialization, path traversal, SSRF, vulnerable dependencies, insecure defaults. For each finding give file:line, how an attacker uses it, severity and the fix. Skip theoretical noise. Do not edit files.",
        Some(&["Read", "Grep", "Glob", "Bash", "WebSearch", "WebFetch", "mcp__feiyi__outline"][..]),
    ));
    m.insert("analyst".into(), role(
        "Data analyst. Give it a question about numbers (usage, revenue, logs, a CSV, a market size); it answers with the figures and how it got them.",
        "You are the data analyst. Answer the question with numbers: say where the data came from, how you computed it, and how confident you are. Use light, read-only queries and scripts; never change production data. Lead with the answer, then a small table if it helps.",
        Some(&["Read", "Grep", "Glob", "Bash", "WebSearch", "WebFetch"][..]),
    ));
    m.insert("marketer".into(), role(
        "Marketer. Give it a product and audience; it writes positioning, landing page copy, launch posts, SEO and campaign plans.",
        &format!("You are the marketer. Start from who the customer is and the one problem the product solves for them; write positioning, landing page copy, launch posts, SEO pages and campaign plans in plain, specific language with no hype. Check claims against what the product actually does. Suggest how to measure each idea. {never_send}"),
        Some(drafter),
    ));
    m.insert("sales".into(), role(
        "Sales. Give it a product and target customer; it finds leads, writes outreach emails, call scripts, pricing pages and proposals.",
        &format!("You are sales. Find who has the problem and can pay (with sources), then write short, personal outreach, follow-ups, call scripts, proposals and pricing pages. Lead with the customer's problem, not features; one clear ask per message. {never_send}"),
        Some(drafter),
    ));
    m.insert("support".into(), role(
        "Customer support. Give it a customer message or issue; it finds the cause in the code or docs and drafts the reply, plus FAQ and help docs.",
        &format!("You are customer support. Understand what the customer is trying to do, find the real cause in the code, docs or logs, and draft a kind, short reply with the fix or workaround. If it is a bug, write a clear bug report for the engineer. Turn repeated questions into FAQ or help docs. {never_send}"),
        Some(&["Read", "Grep", "Glob", "Edit", "Write", "WebFetch", "mcp__feiyi__outline"][..]),
    ));
    m.insert("finance".into(), role(
        "Finance. Give it pricing, costs, revenue or a budget question; it builds the numbers: pricing, unit economics, runway, invoices, cost cuts.",
        &format!("You are finance. Work out pricing, unit economics, costs (hosting, APIs, tools), revenue projections, runway and invoices. Show the formula and assumptions behind every number, flag the ones that matter most, and prefer the simple spreadsheet-style answer. Note tax or accounting questions that need a professional. {never_send} Never move money."),
        Some(drafter),
    ));
    m.insert("legal".into(), role(
        "Legal. Give it a document or question; it drafts terms, privacy policies, licenses and contracts and flags legal risks. Not a lawyer.",
        &format!("You are legal. Draft and review terms of service, privacy policies, contracts, and license and compliance checks (open-source licenses, GDPR, app store rules). Write in plain language, point out the risky clauses and what is missing, and cite the rule or source. Say clearly which points need a real lawyer; this is not legal advice. {never_send}"),
        Some(drafter),
    ));
    m
}

/// Role names become file names: keep them plain.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn memory_path(home: &Path, role: &str) -> PathBuf {
    home.join("roles").join(format!("{role}.md"))
}

/// Memory files are cut to their newest lines past this size.
const MEMORY_MAX_BYTES: usize = 8 * 1024;

/// The roles for a chat: built-ins merged with the user's `roles`, each with
/// its memory and the `remember` tool, or None when turned off or empty.
pub fn roles(stored: &Map<String, Value>, home: &Path) -> Option<Map<String, Value>> {
    if stored.get("company").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let mut roles = builtin();
    for (name, role) in stored.get("roles").and_then(Value::as_object).into_iter().flatten() {
        let text = |k: &str| role.get(k).and_then(Value::as_str).is_some_and(|v| !v.trim().is_empty());
        if role.is_null() {
            roles.remove(name);
        } else if valid_name(name) && text("description") && text("prompt") {
            roles.insert(name.clone(), role.clone());
        }
    }
    for (name, role) in roles.iter_mut() {
        let memory = std::fs::read_to_string(memory_path(home, name)).unwrap_or_default();
        let memory = if memory.trim().is_empty() { "(empty)".to_string() } else { memory };
        let prompt = role["prompt"].as_str().unwrap_or_default().to_string();
        role["prompt"] = json!(format!(
            "{prompt}\n\nYour memory, notes you saved on earlier jobs:\n{memory}\nWhen you learn something worth keeping for future jobs (the user's preferences, a project's conventions, a mistake to avoid), save it as one short line with the feiyi `remember` tool (role: \"{name}\"); start project-specific notes with the project name. Don't save what the code or git history already shows."
        ));
        if let Some(tools) = role.get_mut("tools").and_then(Value::as_array_mut) {
            tools.push(json!("mcp__feiyi__remember"));
        }
    }
    (!roles.is_empty()).then_some(roles)
}

/// Add one note to a role's memory.
pub fn remember(home: &Path, role: &str, note: &str) -> Result<String, String> {
    if !valid_name(role) {
        return Err(format!("invalid role name: {role:?}"));
    }
    let note = note.split_whitespace().collect::<Vec<_>>().join(" ");
    if note.is_empty() || note.len() > 500 {
        return Err("the note must be one line of 1-500 characters".into());
    }
    let path = memory_path(home, role);
    let mut memory = std::fs::read_to_string(&path).unwrap_or_default();
    memory.push_str(&format!("- {note}\n"));
    while memory.len() > MEMORY_MAX_BYTES {
        match memory.find('\n') {
            Some(i) => memory.drain(..=i),
            None => break,
        };
    }
    std::fs::create_dir_all(path.parent().expect("parent")).map_err(|e| e.to_string())?;
    crate::util::write_atomic(&path, memory.as_bytes(), true).map_err(|e| e.to_string())?;
    Ok(format!("saved to {role}'s memory"))
}

/// Write the roles as Codex custom agent files and remove ones no longer
/// wanted (all of them when `roles` is None).
pub fn sync_codex(roles: Option<&Map<String, Value>>) {
    let Some(home) = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))) else { return };
    let dir = home.join("agents");
    let empty = Map::new();
    let roles = roles.unwrap_or(&empty);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            let role = file.strip_prefix("feiyi-").and_then(|f| f.strip_suffix(".toml"));
            if role.is_some_and(|r| !roles.contains_key(r)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    if roles.is_empty() || std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (name, role) in roles {
        let toml = codex_toml(name, role);
        let path = dir.join(format!("feiyi-{name}.toml"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(toml.as_str()) {
            if let Err(e) = crate::util::write_atomic(&path, toml.as_bytes(), false) {
                crate::warn!("failed to write codex role {name}: {e}");
            }
        }
    }
}

fn codex_toml(name: &str, role: &Value) -> String {
    // A JSON string is a valid TOML basic string, except for a raw DEL.
    let s = |k: &str| json!(role.get(k).and_then(Value::as_str).unwrap_or_default().replace('\u{7f}', "")).to_string();
    format!("# Written by feiyi from its company roles; edits are overwritten.\nname = {}\ndescription = {}\ndeveloper_instructions = {}\n", json!(name), s("description"), s("prompt"))
}

/// Added to the lead's instructions.
pub fn lead_prompt(roles: &Map<String, Value>) -> String {
    let names: Vec<&str> = roles.keys().map(String::as_str).collect();
    format!(
        "You lead a one-person company. Your team, sub-agents you spawn by name: {}. When the user gives a goal instead of exact steps, act as the lead: work out the concrete requirements yourself from the code and context (ask the user only for decisions that are truly theirs, all in one batch, before starting), split the goal into tasks, give each to the right role with a self-contained brief (goal, files, acceptance criteria), run independent tasks in parallel, have the reviewer or qa (and security for auth, payments or user data) check the engineer's work and send fixes back until it passes; for business goals (launch, pricing, outreach, support, legal) use pm, marketer, sales, support, finance and legal the same way. Nothing leaves the company without the user: roles only draft emails, posts, payments and contracts. Then report the result to the user briefly. Small or exact requests: just do them yourself.",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("feiyi-company-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn user_roles_merge_and_switch_off() {
        let h = home();
        let stored = json!({ "roles": { "reviewer": null, "designer": { "description": "UI", "prompt": "Design." }, "bad": { "description": "" }, "../x": { "description": "d", "prompt": "p" } } });
        let r = roles(stored.as_object().unwrap(), &h).unwrap();
        assert!(r.contains_key("engineer") && r.contains_key("designer") && r.contains_key("legal"));
        assert!(r["sales"]["prompt"].as_str().unwrap().contains("never send"));
        assert!(!r.contains_key("reviewer") && !r.contains_key("bad") && !r.contains_key("../x"));
        assert!(lead_prompt(&r).contains("designer"));
        assert!(r["qa"]["tools"].as_array().unwrap().contains(&json!("mcp__feiyi__remember")));
        assert!(roles(json!({ "company": false }).as_object().unwrap(), &h).is_none());
    }

    #[test]
    fn memory_is_saved_capped_and_injected() {
        let h = home();
        assert!(remember(&h, "../etc", "x").is_err());
        assert!(remember(&h, "engineer", "  ").is_err());
        remember(&h, "engineer", "feiyi: run\n no builds locally").unwrap();
        let r = roles(&Map::new(), &h).unwrap();
        assert!(r["engineer"]["prompt"].as_str().unwrap().contains("- feiyi: run no builds locally"));
        assert!(r["qa"]["prompt"].as_str().unwrap().contains("(empty)"));
        for i in 0..100 {
            remember(&h, "engineer", &format!("note {i} {}", "x".repeat(100))).unwrap();
        }
        let memory = std::fs::read_to_string(memory_path(&h, "engineer")).unwrap();
        assert!(memory.len() <= MEMORY_MAX_BYTES && memory.ends_with("x\n") && !memory.contains("feiyi: run"));
    }

    #[test]
    fn codex_toml_escapes() {
        let t = codex_toml("qa", &json!({ "description": "say \"hi\"", "prompt": "a\nb\\c" }));
        assert!(t.contains(r#"description = "say \"hi\"""#) && t.contains(r#"developer_instructions = "a\nb\\c""#));
    }
}
