//! Token Saver, ported from zilong-router (which follows 9router's page of the
//! same name): the Caveman and Ponytail system prompts plus RTK tool-output
//! compression. zilong-router applies them to proxied API requests; feiyi
//! does not proxy the API, so here:
//!
//! - Caveman / Ponytail (`PASEO_CAVEMAN`, `PASEO_PONYTAIL`, always on, `ultra`
//!   unless set to another level) are added to every agent's system prompt
//!   (manager.rs builds it; Claude gets it
//!   through `--append-system-prompt`, Codex as `developerInstructions`).
//! - RTK (always on) is a Claude Code `PreToolUse` hook on
//!   the Bash tool, given through `--settings <home>/claude-settings.json`
//!   (which also carries the secret guard hook, guard.rs).
//!   The hook (`feiyi rtk-hook`) rewrites simple, well-known commands
//!   (git status/diff/log/show, grep, find, ls, cargo build, ...) to
//!   `feiyi rtk -- '<command>'`, which runs the command and prints its
//!   output compressed. It only rewrites in `bypassPermissions` mode: Claude
//!   Code checks permissions against the rewritten command, so in any other
//!   mode it would no longer match the user's allow rules or the read-only
//!   command list and would prompt where the original would not. Codex agents get
//!   no RTK: Codex's PreToolUse hooks are behind the experimental `codex_hooks`
//!   feature and a hook-trust step, left off here.

pub mod prompts;
pub mod rtk;

use crate::config::Config;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Hidden subcommands of the `feiyi` binary.
pub const HOOK_CMD: &str = "rtk-hook";
pub const RUN_CMD: &str = "rtk";

/// Shown after compressed output, so the agent knows how to see it raw
/// (commands with a pipe are never rewritten).
const FOOTER: &str = "[feiyi rtk: output compressed; append `| cat` to the command for the raw output]";

/// Caveman then Ponytail, as one block for the system prompt.
pub fn system_prompt(cfg: &Config) -> Option<String> {
    let parts: Vec<String> = [cfg.caveman.as_deref().and_then(prompts::caveman), cfg.ponytail.as_deref().and_then(prompts::ponytail)].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The running binary, for the hook command. Linux appends " (deleted)" once
/// the file was replaced (an upgrade); the new binary sits at the same path.
pub fn current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let s = exe.to_str()?;
    let path = PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(s));
    path.is_file().then_some(path)
}

fn settings_path(cfg: &Config) -> PathBuf {
    cfg.home.join("claude-settings.json")
}

/// The `--settings` contents: PreToolUse command hooks, the secret guard
/// (unless `PASEO_SECRET_GUARD=0`) and RTK on Bash (Token Saver on).
fn claude_settings_json(exe: &Path, rtk: bool, guard: bool) -> Option<Value> {
    let exe = shell_quote(exe.to_str()?);
    let hook = |matcher: &str, cmd: &str| json!({ "matcher": matcher, "hooks": [{ "type": "command", "command": format!("{exe} {cmd}"), "timeout": 5 }] });
    let mut hooks = Vec::new();
    if guard {
        hooks.push(hook(crate::guard::MATCHER, crate::guard::HOOK_CMD));
    }
    if rtk {
        hooks.push(hook("Bash", HOOK_CMD));
    }
    (!hooks.is_empty()).then(|| json!({ "hooks": { "PreToolUse": hooks } }))
}

/// Writes Claude's `--settings` file at startup, or removes a stale one when
/// there is no hook to run.
pub fn write_claude_settings(cfg: &Config) {
    let path = settings_path(cfg);
    let guard = crate::guard::enabled();
    let Some(settings) = current_exe().and_then(|exe| claude_settings_json(&exe, cfg.rtk, guard)) else {
        if cfg.rtk || guard {
            crate::warn!("claude hooks: cannot locate the feiyi binary; Claude Code starts without the secret guard and RTK");
        }
        let _ = std::fs::remove_file(&path);
        return;
    };
    match crate::util::write_atomic(&path, settings.to_string().as_bytes(), false) {
        Ok(()) => crate::log!("claude hooks: secret guard {}, RTK {} ({})", if guard { "on" } else { "off" }, if cfg.rtk { "on" } else { "off" }, path.display()),
        Err(e) => crate::warn!("claude hooks: cannot write {}: {e}; Claude Code starts without the secret guard and RTK", path.display()),
    }
}

/// `--settings <file>` for `claude`, when it holds any hook.
pub fn claude_settings(cfg: &Config) -> Option<PathBuf> {
    Some(settings_path(cfg)).filter(|p| p.exists())
}

/// Single-quoted for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ----- feiyi rtk-hook -------------------------------------------------------------

/// `feiyi rtk-hook`: Claude Code's PreToolUse hook input on stdin. Prints
/// the rewrite, or nothing (no change) on any error or for any other command.
pub fn hook_main() {
    let mut input = String::new();
    if std::io::stdin().take(1 << 20).read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return };
    let Some(exe) = current_exe() else { return };
    if let Some(out) = hook_output(&event, &exe) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.to_string().as_bytes());
        let _ = stdout.flush();
    }
}

/// `updatedInput` with the command wrapped, and no `permissionDecision`: the
/// normal permission flow still runs (on the rewritten input).
fn hook_output(event: &Value, exe: &Path) -> Option<Value> {
    if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    // See the module doc: other modes check permissions on the rewritten command.
    if event.get("permission_mode").and_then(Value::as_str) != Some("bypassPermissions") {
        return None;
    }
    let tool_input = event.get("tool_input")?.as_object()?;
    // Background shells are read while they run; wrapped, their output would
    // only appear at the end.
    if tool_input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let command = tool_input.get("command")?.as_str()?;
    if !worth_wrapping(command) {
        return None;
    }
    let mut updated = tool_input.clone();
    updated.insert("command".into(), json!(format!("{} {RUN_CMD} -- {}", shell_quote(exe.to_str()?), shell_quote(command.trim()))));
    Some(json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "updatedInput": updated } }))
}

/// One simple command running a well-known program whose output RTK shrinks,
/// with nothing a wrapper shell could run differently: no pipes, lists (`&&`,
/// `||`, `;`, `&`), redirections (but `2>&1`), subshells, braces, expansions
/// (`$`, backticks), heredocs or newlines. `cd`, `export` and the like are not
/// on the list, so commands that change the shell's state (which Claude Code
/// keeps between calls) are never wrapped, and neither is an already wrapped
/// command (its program is feiyi).
pub fn worth_wrapping(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || command.len() > 4096 || command.contains(['\n', '\r']) {
        return false;
    }
    words(command).map_or(false, |w| worth(&w))
}

/// The command's words, quotes removed; None when it uses any other shell
/// syntax (see `worth_wrapping`).
fn words(command: &str) -> Option<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()?.1 {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()?.1 {
                        '"' => break,
                        '$' | '`' | '\\' | '!' => return None,
                        c => w.push(c),
                    }
                }
            }
            '\\' => {
                let (_, next) = chars.next()?;
                word.get_or_insert_with(String::new).push(next);
            }
            ' ' | '\t' => words.extend(word.take()),
            // `2>&1` on its own: stderr into stdout, which the wrapper merges anyway.
            '2' if word.is_none() && command[i..].starts_with("2>&1") && command[i + 4..].chars().next().map_or(true, |n| n == ' ' || n == '\t') => {
                for _ in 0..3 {
                    chars.next();
                }
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '$' | '`' | '#' | '!' => return None,
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word.take());
    (!words.is_empty()).then_some(words)
}

/// The first argument that is not an option, skipping the value of the
/// options in `valued`.
fn operand<'a>(args: &'a [String], valued: &[&str]) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if valued.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') {
            return Some(a);
        }
        i += 1;
    }
    None
}

/// Whether one simple command is a program and subcommand RTK has a filter for.
fn worth(words: &[String]) -> bool {
    let Some(program) = words.first() else { return false };
    let args = &words[1..];
    match program.as_str() {
        "ls" | "tree" | "find" | "fd" | "grep" | "egrep" | "fgrep" | "rg" => true,
        "git" => matches!(operand(args, &["-C", "-c", "--git-dir", "--work-tree", "--namespace"]), Some("status" | "diff" | "log" | "show" | "grep")),
        "cargo" => matches!(operand(args, &["--manifest-path", "--color", "-Z", "--config"]), Some("build" | "b" | "check" | "c" | "test" | "t" | "clippy" | "doc")),
        "go" => matches!(operand(args, &[]), Some("build" | "test" | "vet")),
        "npm" | "pnpm" | "yarn" => match operand(args, &["--prefix", "-C", "--dir", "--cwd"]) {
            Some("install" | "i" | "ci" | "add" | "test" | "t" | "build") => true,
            Some("run" | "run-script") => {
                let script = args.iter().skip_while(|a| !matches!(a.as_str(), "run" | "run-script")).nth(1).map(String::as_str);
                matches!(script, Some("build" | "test" | "lint" | "typecheck"))
            }
            _ => false,
        },
        _ => false,
    }
}

// ----- feiyi rtk -- <command> -----------------------------------------------------

/// `feiyi rtk -- <command>`: runs the command with `sh -c` (stderr merged
/// into stdout), prints its output compressed when a filter matched (else as
/// is) and exits with its exit code.
pub fn run_main(args: &[String]) -> ! {
    let args = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
    if args.is_empty() {
        eprintln!("usage: feiyi rtk -- <shell command>");
        std::process::exit(2);
    }
    let command = args.join(" ");
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("feiyi rtk: cannot run sh: {e}");
            std::process::exit(127);
        }
    };
    let mut stdout = std::io::stdout();
    match std::str::from_utf8(&output.stdout).ok().and_then(rtk::compress_text) {
        Some((_, text)) => {
            let _ = stdout.write_all(text.trim_end_matches('\n').as_bytes());
            let _ = stdout.write_all(format!("\n{FOOTER}\n").as_bytes());
        }
        None => {
            let _ = stdout.write_all(&output.stdout);
        }
    }
    let _ = stdout.flush();
    std::process::exit(exit_code(&output.status));
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_only_plain_known_commands() {
        for ok in [
            "git status",
            "git -C sub diff --stat",
            "git log --oneline -n 50",
            "rg 'fn main' src",
            "grep -rn \"TODO\" src",
            "find . -name '*.rs'",
            "ls -la",
            "tree -L 2",
            "cargo build --release 2>&1",
            "cargo test -p foo",
            "npm install",
            "npm run build",
            "go test ./...",
        ] {
            assert!(worth_wrapping(ok), "{ok}");
        }
        for no in [
            "",
            "cd sub && git status",
            "git status && git diff",
            "ls || true",
            "ls\nrm x",
            "git push",
            "git add -p",
            "git status | head",
            "git diff > out.patch",
            "ls; rm x",
            "cat <<EOF\nx\nEOF",
            "ls $HOME",
            "grep \"$X\" f",
            "ls `pwd`",
            "(ls)",
            "ls &",
            "FOO=1 cargo test",
            "npm run dev",
            "npm start",
            "echo hi",
            "find . -exec rm {} \\;",
            "'/x/feiyi' rtk -- 'git status'",
            "git status 'unterminated",
            "ls 2>/dev/null",
        ] {
            assert!(!worth_wrapping(no), "{no}");
        }
    }

    #[test]
    fn hook_rewrites_bash_in_bypass_mode_only() {
        let exe = Path::new("/opt/my tools/feiyi");
        let event = |mode: &str, command: &str| json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "permission_mode": mode, "tool_input": { "command": command, "description": "d", "timeout": 1000 } });
        let out = hook_output(&event("bypassPermissions", "git log -n 5 --format='%h %s'"), exe).unwrap();
        let hso = &out["hookSpecificOutput"];
        assert_eq!(hso["hookEventName"], "PreToolUse");
        assert!(hso.get("permissionDecision").is_none());
        assert_eq!(hso["updatedInput"]["command"], r#"'/opt/my tools/feiyi' rtk -- 'git log -n 5 --format='\''%h %s'\'''"#);
        assert_eq!(hso["updatedInput"]["description"], "d");
        assert_eq!(hso["updatedInput"]["timeout"], 1000);
        for mode in ["default", "acceptEdits", "plan"] {
            assert!(hook_output(&event(mode, "git status"), exe).is_none());
        }
        assert!(hook_output(&event("bypassPermissions", "git push"), exe).is_none());
        let mut bg = event("bypassPermissions", "git status");
        bg["tool_input"]["run_in_background"] = json!(true);
        assert!(hook_output(&bg, exe).is_none());
        let mut other = event("bypassPermissions", "git status");
        other["tool_name"] = json!("Read");
        assert!(hook_output(&other, exe).is_none());
    }

    #[test]
    fn settings_hold_the_guard_and_the_bash_hook() {
        let exe = Path::new("/usr/bin/feiyi");
        let s = claude_settings_json(exe, true, true).unwrap();
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], crate::guard::MATCHER);
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/usr/bin/feiyi' guard-hook");
        assert_eq!(s["hooks"]["PreToolUse"][1]["matcher"], "Bash");
        assert_eq!(s["hooks"]["PreToolUse"][1]["hooks"][0]["command"], "'/usr/bin/feiyi' rtk-hook");
        assert_eq!(claude_settings_json(exe, false, true).unwrap()["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(claude_settings_json(exe, false, false).is_none());
    }

    #[test]
    fn prompts_follow_the_levels() {
        let mut cfg = Config::load(&[]);
        cfg.caveman = None;
        cfg.ponytail = None;
        assert!(system_prompt(&cfg).is_none());
        cfg.caveman = Some("ultra".into());
        assert_eq!(system_prompt(&cfg), prompts::caveman("ultra"));
        cfg.ponytail = Some("lite".into());
        let both = system_prompt(&cfg).unwrap();
        assert!(both.starts_with(&prompts::caveman("ultra").unwrap()));
        assert!(both.ends_with(&prompts::ponytail("lite").unwrap()));
    }
}
