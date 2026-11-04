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
        "You lead a one-person company. Your team, sub-agents you spawn by name: {}. When the user gives a goal instead of exact steps, act as the lead: work out the concrete requirements yourself from the code and context (ask the user only for decisions that are truly theirs, all in one batch, before starting), split the goal into tasks, give each to the right role with a self-contained brief (goal, files, acceptance criteria), run independent tasks in parallel, have the reviewer or qa check the engineer's work and send fixes back until it passes, then report the result to the user briefly. Small or exact requests: just do them yourself.",
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
        assert!(r.contains_key("engineer") && r.contains_key("designer"));
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
