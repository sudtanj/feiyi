//! `feiyi graph-hook`: an advisory Claude Code `PreToolUse` hook (given
//! through `--settings`). Before a Grep, or a Read of a long file, it asks the
//! daemon's MCP server for `graph_query` and hands the answer to Claude as
//! context. It never denies: on any failure it prints nothing.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub const HOOK_CMD: &str = "graph-hook";

/// Total time for the daemon call (the hook's own timeout is 5 s).
const TIMEOUT: Duration = Duration::from_secs(3);
/// Longest context handed to Claude, in bytes.
const MAX_CONTEXT: usize = 1500;
/// Shortest word worth a graph lookup.
const MIN_WORD: usize = 3;

pub fn hook_main() {
    let mut input = String::new();
    if std::io::stdin().take(1 << 20).read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return };
    let Some((dir, q)) = query_for(&event) else { return };
    let Some(text) = ask_daemon(&dir, &q) else { return };
    if let Some(out) = render(&q, &text) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.to_string().as_bytes());
        let _ = stdout.flush();
    }
}

/// The project directory and the `graph_query` text for this tool call, None
/// when there is nothing to look up.
fn query_for(event: &Value) -> Option<(String, String)> {
    let dir = event.get("cwd")?.as_str().filter(|d| !d.is_empty())?;
    let input = event.get("tool_input")?.as_object()?;
    let q = match event.get("tool_name")?.as_str()? {
        "Grep" => grep_word(input.get("pattern")?.as_str()?)?,
        "Read" => {
            if ["offset", "limit"].iter().any(|k| input.get(*k).is_some_and(|v| !v.is_null())) {
                return None;
            }
            let path = input.get("file_path")?.as_str()?;
            if !is_long(path) {
                return None;
            }
            // The graph stores paths relative to the project directory.
            path.strip_prefix(dir).map_or(path, |p| p.trim_start_matches('/')).to_string()
        }
        _ => return None,
    };
    Some((dir.to_string(), q))
}

/// The longest identifier-like run of a regex (first on a tie); an escape such
/// as `\s` or `\(` is not part of one.
fn grep_word(pattern: &str) -> Option<String> {
    let mut words = vec![String::new()];
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            words.push(String::new());
        } else if c.is_alphanumeric() || c == '_' {
            words.last_mut()?.push(c);
        } else {
            words.push(String::new());
        }
    }
    let best = words.into_iter().rev().max_by_key(|w| w.chars().count())?;
    (best.chars().count() >= MIN_WORD).then_some(best)
}

/// A regular file over `READ_WHOLE_MAX_LINES` lines; huge files are skipped
/// unread.
fn is_long(path: &str) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() || meta.len() > 8 << 20 {
        return false;
    }
    std::fs::read(path).is_ok_and(|b| b.iter().filter(|c| **c == b'\n').count() > crate::outline::READ_WHOLE_MAX_LINES)
}

/// The hook output, None when the graph had nothing.
fn render(q: &str, text: &str) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() || text.starts_with("nothing matches") {
        return None;
    }
    let context = crate::util::truncate(&format!("Code graph for {q}:\n{text}"), MAX_CONTEXT);
    Some(json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": context } }))
}

/// Where the daemon's MCP server is and the bearer token, from the private
/// `claude-mcp.json` the daemon wrote (the token never touches a command line).
fn endpoint(config: &Value) -> Option<(SocketAddr, String, String)> {
    let server = config.pointer(&format!("/mcpServers/{}", crate::mcp::DEBUG_NAME))?;
    let rest = server.get("url")?.as_str()?.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/')?;
    let token = server.pointer("/headers/Authorization")?.as_str()?.strip_prefix("Bearer ")?;
    Some((host.parse().ok()?, format!("/{path}"), token.to_string()))
}

fn ask_daemon(dir: &str, q: &str) -> Option<String> {
    let start = Instant::now();
    let home = std::env::var("PASEO_LITE_HOME").ok().filter(|s| !s.trim_end_matches('/').is_empty()).or_else(|| std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(|h| format!("{h}/.paseo-lite")))?;
    let config: Value = serde_json::from_slice(&std::fs::read(format!("{home}/claude-mcp.json")).ok()?).ok()?;
    let (addr, path, token) = endpoint(&config)?;
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "graph_query", "arguments": { "dir": dir, "q": q, "depth": 1, "budget": MAX_CONTEXT } } }).to_string();
    let request = format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/json\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT).ok()?;
    let left = || TIMEOUT.checked_sub(start.elapsed()).filter(|d| !d.is_zero());
    tcp.set_write_timeout(Some(left()?)).ok()?;
    tcp.write_all(request.as_bytes()).ok()?;
    tcp.set_read_timeout(Some(left()?)).ok()?;
    let mut raw = Vec::new();
    (&tcp).take(256 * 1024).read_to_end(&mut raw).ok()?;
    let (status, body) = crate::push::parse_response(&raw).ok()?;
    let reply: Value = serde_json::from_str(&body).ok().filter(|_| status == 200)?;
    let result = reply.get("result")?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(result.pointer("/content/0/text")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(tool: &str, input: Value) -> Value {
        json!({ "tool_name": tool, "tool_input": input, "cwd": "/work/app" })
    }

    fn long_file(lines: usize) -> String {
        let path = std::env::temp_dir().join(format!("graph-hook-{}-{lines}.rs", std::process::id()));
        std::fs::write(&path, "x\n".repeat(lines)).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn grep_patterns_reduce_to_a_word() {
        assert_eq!(grep_word(r"fn\s+foo\("), Some("foo".into()));
        assert_eq!(grep_word("handle_request"), Some("handle_request".into()));
        assert_eq!(grep_word(r"^\w+Handler$"), Some("Handler".into()));
        assert_eq!(grep_word(r"a|b\d+"), None);
        assert_eq!(grep_word(r"\s*\(.*\)"), None);
        let (dir, q) = query_for(&event("Grep", json!({ "pattern": r"fn\s+foo\(" }))).unwrap();
        assert_eq!((dir.as_str(), q.as_str()), ("/work/app", "foo"));
    }

    #[test]
    fn read_asks_only_for_long_whole_files() {
        let long = long_file(crate::outline::READ_WHOLE_MAX_LINES + 1);
        let short = long_file(crate::outline::READ_WHOLE_MAX_LINES);
        assert_eq!(query_for(&event("Read", json!({ "file_path": long }))).unwrap().1, long);
        assert!(query_for(&event("Read", json!({ "file_path": short }))).is_none());
        assert!(query_for(&event("Read", json!({ "file_path": long, "limit": 50 }))).is_none());
        assert!(query_for(&event("Read", json!({ "file_path": long, "offset": 10 }))).is_none());
        assert!(query_for(&event("Read", json!({ "file_path": "/nonexistent/file.rs" }))).is_none());
        let _ = (std::fs::remove_file(long), std::fs::remove_file(short));
    }

    #[test]
    fn read_path_is_relative_to_the_project() {
        let long = long_file(crate::outline::READ_WHOLE_MAX_LINES + 1);
        let (dir, name) = long.rsplit_once('/').unwrap();
        let ev = json!({ "tool_name": "Read", "tool_input": { "file_path": long }, "cwd": dir });
        assert_eq!(query_for(&ev).unwrap().1, name);
        let _ = std::fs::remove_file(long);
    }

    #[test]
    fn other_calls_and_bad_input_give_nothing() {
        assert!(query_for(&event("Bash", json!({ "command": "ls" }))).is_none());
        assert!(query_for(&event("Grep", json!({}))).is_none());
        assert!(query_for(&json!({ "tool_name": "Grep", "tool_input": { "pattern": "foo_bar" } })).is_none());
    }

    #[test]
    fn output_is_context_only_and_capped() {
        let out = render("foo", "2 nodes\nfn foo src/a.rs:1-3\n").unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(out["hookSpecificOutput"].get("permissionDecision").is_none());
        assert!(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().starts_with("Code graph for foo:\n2 nodes"));
        let long = render("foo", &"x".repeat(5000)).unwrap();
        assert!(long["hookSpecificOutput"]["additionalContext"].as_str().unwrap().len() <= MAX_CONTEXT + 3);
        assert!(render("foo", "  \n").is_none());
        assert!(render("foo", "nothing matches \"foo\" in /work/app\n").is_none());
    }

    #[test]
    fn endpoint_comes_from_the_feiyi_server_entry() {
        let config = json!({ "mcpServers": { "feiyi": { "type": "http", "url": "http://127.0.0.1:8123/mcp", "headers": { "Authorization": "Bearer abc" } } } });
        assert_eq!(endpoint(&config), Some(("127.0.0.1:8123".parse().unwrap(), "/mcp".into(), "abc".into())));
        assert!(endpoint(&json!({ "mcpServers": {} })).is_none());
    }
}
