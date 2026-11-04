//! One-person company: every Claude chat leads a team of role agents (Claude
//! Code sub-agents passed with `--agents`, so they run inside the chat's own
//! process). Give the lead a goal; it fills in the details, hands work to the
//! roles and checks it. daemon-config `roles` adds or replaces roles
//! (`{"name": {"description", "prompt", "tools"?}}`, `null` removes a built-in
//! one); `"company": false` turns it off. Codex has no sub-agents.

use serde_json::{json, Map, Value};

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

/// The roles for a chat: built-ins merged with the user's `roles`, or None
/// when turned off or empty.
pub fn roles(stored: &Map<String, Value>) -> Option<Map<String, Value>> {
    if stored.get("company").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let mut roles = builtin();
    for (name, role) in stored.get("roles").and_then(Value::as_object).into_iter().flatten() {
        let valid = role.get("description").and_then(Value::as_str).is_some_and(|d| !d.trim().is_empty()) && role.get("prompt").and_then(Value::as_str).is_some_and(|p| !p.trim().is_empty());
        if role.is_null() {
            roles.remove(name);
        } else if valid {
            roles.insert(name.clone(), role.clone());
        }
    }
    (!roles.is_empty()).then_some(roles)
}

/// Added to the lead's instructions.
pub fn lead_prompt(roles: &Map<String, Value>) -> String {
    let names: Vec<&str> = roles.keys().map(String::as_str).collect();
    format!(
        "You lead a one-person company. Your team (Agent tool): {}. When the user gives a goal instead of exact steps, act as the lead: work out the concrete requirements yourself from the code and context (ask the user only for decisions that are truly theirs, all in one batch, before starting), split the goal into tasks, give each to the right role with a self-contained brief (goal, files, acceptance criteria), run independent tasks in parallel, have the reviewer or qa check the engineer's work and send fixes back until it passes, then report the result to the user briefly. Small or exact requests: just do them yourself.",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_roles_merge_and_switch_off() {
        let stored = json!({ "roles": { "reviewer": null, "designer": { "description": "UI", "prompt": "Design." }, "bad": { "description": "" } } });
        let r = roles(stored.as_object().unwrap()).unwrap();
        assert!(r.contains_key("engineer") && r.contains_key("designer"));
        assert!(!r.contains_key("reviewer") && !r.contains_key("bad"));
        assert!(lead_prompt(&r).contains("designer"));
        assert!(roles(json!({ "company": false }).as_object().unwrap()).is_none());
    }
}
