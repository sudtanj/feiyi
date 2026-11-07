//! Secret guard: a Claude Code `PreToolUse` hook (`feiyi guard-hook`, given
//! through `--settings` next to the RTK hook) that denies tool calls touching
//! the daemon's and the user's secret files, or reading secret variables out
//! of the environment. Off with `PASEO_SECRET_GUARD=0`.
//!
//! ponytail: string matching on the tool input, not a sandbox. Globs, `eval`,
//! encodings, scripts that build the path, or a program reading the file for
//! the agent get past it. It stops the plain `cat`/Read/`echo $VAR` leaks.
//! Upgrade path: Claude Code's sandbox or bwrap around the agent.

use serde_json::{json, Value};
use std::io::{Read, Write};

pub const HOOK_CMD: &str = "guard-hook";
/// Tools whose input the hook reads (the `--settings` matcher).
pub const MATCHER: &str = "Read|Edit|MultiEdit|Write|Grep|Glob|Bash|NotebookEdit";

/// Variables holding secrets, by name (GH_TOKEN is left out: agents push with it).
const SECRET_VARS: &[&str] = &["PASEO_ZILONG_API_KEY", "PASEO_LITE_MCP_TOKEN", "PASEO_PASSWORD", "FEIYI_BACKUP_PASSPHRASE", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_MESSAGING_TOKEN", "CODEX_API_KEY", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];
/// Paths that are secret wherever the home directory is.
const SECRET_PATHS: &[&str] = &["/.claude/.credentials.json", "/.codex/auth.json", "/.ssh"];
/// What an agent may still read inside the state dir.
const STATE_DIR_ALLOWED: &[&str] = &["/roles/", "/uploads/"];
/// Tool input fields that hold a path, pattern or command.
const INPUT_KEYS: &[&str] = &["file_path", "path", "notebook_path", "pattern", "command"];

pub fn enabled() -> bool {
    std::env::var("PASEO_SECRET_GUARD").map_or(true, |v| v != "0")
}

/// `feiyi guard-hook`: PreToolUse input on stdin; prints a deny, or nothing.
pub fn hook_main() {
    let mut input = String::new();
    if std::io::stdin().take(1 << 20).read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return };
    let Some(home) = std::env::var("HOME").ok().filter(|h| !h.is_empty()) else { return };
    let state = std::env::var("PASEO_LITE_HOME").ok().filter(|s| !s.trim_end_matches('/').is_empty()).unwrap_or_else(|| format!("{home}/.paseo-lite"));
    if let Some(reason) = denied(&event, &home, &state) {
        let out = json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason } });
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.to_string().as_bytes());
        let _ = stdout.flush();
    }
}

/// The reason to deny this tool call, None to leave it to the normal flow.
fn denied(event: &Value, home: &str, state: &str) -> Option<&'static str> {
    let tool = event.get("tool_name")?.as_str()?;
    let input = event.get("tool_input")?.as_object()?;
    for key in INPUT_KEYS {
        let Some(text) = input.get(*key).and_then(Value::as_str) else { continue };
        let text = text.replace("${HOME}", home).replace("$HOME", home).replace("~/", &format!("{home}/"));
        if names_secret_path(&text, state) {
            return Some("feiyi guard: that path holds daemon or user secrets");
        }
        if tool == "Bash" && *key == "command" && reads_secret_env(&text) {
            return Some("feiyi guard: reading secrets from the environment is not allowed");
        }
    }
    None
}

fn names_secret_path(text: &str, state: &str) -> bool {
    if SECRET_PATHS.iter().any(|p| text.contains(p)) || (text.contains("/proc/") && text.contains("/environ")) {
        return true;
    }
    let state = state.trim_end_matches('/');
    text.match_indices(state).any(|(i, _)| {
        let rest = &text[i + state.len()..];
        !STATE_DIR_ALLOWED.iter().any(|ok| rest.starts_with(ok))
    })
}

fn reads_secret_env(command: &str) -> bool {
    // Expansions only: grepping or committing a variable's name is fine.
    if SECRET_VARS.iter().any(|v| command.contains(&format!("${v}")) || command.contains(&format!("${{{v}"))) {
        return true;
    }
    // Commands start after a separator or a `$(`/backtick; bare parentheses
    // are left alone so `grep 'env(' src` is not read as a lone `env`.
    let command = command.replace("$(", "\n").replace('`', "\n");
    command.split(|c| matches!(c, ';' | '|' | '&' | '\n')).any(|segment| {
        let words: Vec<&str> = segment.split_whitespace().map(|w| w.trim_start_matches(['(', '{']).trim_end_matches(')')).filter(|w| !w.is_empty()).collect();
        match words.as_slice() {
            ["printenv", ..] | ["export" | "declare" | "typeset" | "set" | "env"] => true,
            ["env", rest @ ..] | ["export" | "declare" | "typeset" | "compgen", rest @ ..] => rest.iter().all(|w| w.starts_with('-')),
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(tool: &str, key: &str, value: &str) -> bool {
        denied(&json!({ "tool_name": tool, "tool_input": { key: value } }), "/home/x", "/home/x/.paseo-lite").is_some()
    }

    #[test]
    fn blocks_secret_files_and_env_reads() {
        assert!(deny("Bash", "command", "cat ~/.paseo-lite/claude-mcp.json"));
        assert!(deny("Bash", "command", "cd $HOME/.paseo-lite && cat claude-mcp.json"));
        assert!(deny("Read", "file_path", "/home/x/.claude/.credentials.json"));
        assert!(deny("Grep", "path", "~/.ssh"));
        assert!(deny("Bash", "command", "echo $PASEO_ZILONG_API_KEY"));
        assert!(deny("Bash", "command", "cat /proc/self/environ"));
        assert!(deny("Bash", "command", "ls && printenv"));
        assert!(deny("Bash", "command", "env | grep KEY"));
        assert!(deny("Bash", "command", "echo ${CLAUDE_CODE_OAUTH_TOKEN}"));
        assert!(deny("Bash", "command", "echo $(env)"));
        assert!(deny("Bash", "command", "x=`printenv`"));
        assert!(deny("Bash", "command", "(env)"));
    }

    #[test]
    fn allows_ordinary_work() {
        assert!(!deny("Bash", "command", "cat src/main.rs"));
        assert!(!deny("Bash", "command", "git push"));
        assert!(!deny("Bash", "command", "gh pr list"));
        assert!(!deny("Bash", "command", "env FOO=1 cargo test"));
        assert!(!deny("Read", "file_path", "~/.paseo-lite/roles/qa.md"));
        assert!(!deny("Read", "file_path", "~/.paseo-lite/uploads/ab12/shot.png"));
        assert!(!deny("Read", "file_path", "src/environment.rs"));
        assert!(!deny("Write", "content", "~/.ssh/id_rsa"));
        assert!(!deny("Bash", "command", "grep -n 'env(' src/config.rs"));
        assert!(!deny("Bash", "command", "git commit -m \"fix env (again)\""));
        assert!(!deny("Bash", "command", "set -euo pipefail; export FOO=bar"));
        assert!(!deny("Bash", "command", "grep -rn PASEO_ZILONG_API_KEY src"));
    }
}
