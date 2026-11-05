//! End-to-end tests: start the real daemon binary on a free port with a
//! throwaway home, then talk to it over HTTP and WebSocket exactly like the
//! app does. A fake `claude` script stands in for the Claude Code CLI.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

struct Daemon {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("feiyi-e2e-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in for `claude -p --input-format stream-json`: answers every user
/// message with one assistant message and a successful result.
fn fake_claude(dir: &std::path::Path, mode: &str) -> PathBuf {
    let path = dir.join(format!("fake-claude-{mode}.sh"));
    let script = r#"#!/bin/sh
n=0
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      printf '%s\n' '{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"hello from fake claude"}],"usage":{"input_tokens":3,"output_tokens":4}}}'
      n=$((n+1))
      printf '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":3,"output_tokens":4},"total_cost_usd":0.0,"session_id":"fake-session","modelUsage":{"claude-test":{"inputTokens":%d,"outputTokens":%d,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"costUSD":%d}}}\n' $((100*n)) $((10*n)) $n
      ;;
  esac
done
"#;
    let hanging = "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"user\"'*) sleep 600 ;;\n  esac\ndone\n";
    // One tool call (running, then its result), then text: exercises rows updated in place.
    let tool = concat!(
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"user\"'*)\n",
        "      printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]}}'\n",
        "      printf '%s\\n' '{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"file.txt\"}]}}'\n",
        "      printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"id\":\"m2\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}'\n",
        "      printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n",
        "      ;;\n  esac\ndone\n"
    );
    // Twenty streamed text deltas in quick succession: exercises delta batching.
    let mut stream = String::from("#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"user\"'*)\n");
    stream.push_str("      printf '%s\\n' '{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}}'\n");
    for i in 0..20 {
        stream.push_str(&format!("      printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"w{i} \"}}}}}}'\n"));
    }
    stream.push_str("      printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n      ;;\n  esac\ndone\n");
    // Refused by the usage limit the first time (state kept next to the script,
    // since the daemon restarts the CLI), answered normally after that.
    let limit = format!(
        concat!(
            "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"user\"'*)\n",
            "      if [ ! -e '{marker}' ]; then\n        : > '{marker}'\n",
            "        printf '%s\\n' '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"resetsAt\":1,\"rateLimitType\":\"five_hour\"}}}}'\n",
            "        printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":true,\"result\":\"Session limit reached\"}}'\n",
            "      else\n",
            "        printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"back to work\"}}]}}}}'\n",
            "        printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}}'\n",
            "      fi\n      ;;\n  esac\ndone\n"
        ),
        marker = dir.join("limit-hit").display()
    );
    // A cold resume: the process's first prompt is preceded by the empty,
    // zero-token result of a stopped-notice turn (not echoed), then answered.
    let resume = r#"#!/bin/sh
first=1
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      if [ "$first" = 1 ]; then
        first=0
        printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"","usage":{"output_tokens":0}}'
      fi
      printf '%s\n' '{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"answered after the notice"}]}}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"answered after the notice","usage":{"output_tokens":4}}'
      ;;
  esac
done
"#;
    let body = match mode {
        "resume" => resume.to_string(),
        "hang" => hanging.to_string(),
        "tool" => tool.to_string(),
        "stream" => stream,
        "limit" => limit,
        // Like "ok", and saves its arguments (one per line) for the test to check.
        "args" => format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{}", dir.join("claude-args").display(), script.trim_start_matches("#!/bin/sh\n")),
        // Like "args", and also saves the environment it was started with.
        "env" => format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n/usr/bin/env > '{}'\n{}",
            dir.join("claude-args").display(),
            dir.join("claude-env").display(),
            script.trim_start_matches("#!/bin/sh\n")
        ),
        _ => script.to_string(),
    };
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn start(tag: &str, password: Option<&str>) -> Daemon {
    start_at(unique_dir(tag), password, "ok")
}

/// Start (or restart) the daemon on an existing directory. `hang`: the fake
/// claude accepts a prompt and never answers, to simulate a turn in flight.
fn start_at(dir: PathBuf, password: Option<&str>, mode: &str) -> Daemon {
    start_with(dir, password, mode, &[])
}

/// `start_at` plus extra daemon environment variables (they override the defaults).
fn start_with(dir: PathBuf, password: Option<&str>, mode: &str, extra_env: &[(&str, &str)]) -> Daemon {
    let claude = fake_claude(&dir, mode);
    // free_port() can hand two parallel tests the same port; a daemon that
    // lost the race exits, so retry on a fresh port rather than talk to the
    // other test's daemon.
    for _ in 0..5 {
        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_feiyi"));
        cmd.args(["--listen", &format!("127.0.0.1:{port}"), "--no-relay"])
            .env("PASEO_LITE_HOME", dir.join("home"))
            .env("PASEO_CLAUDE_BIN", &claude)
            .env("PASEO_CODEX_BIN", "/nonexistent/codex")
            .env("PASEO_DEFAULT_SYSTEM_PROMPT", "")
            .env("PASEO_RETRY_MAX", "0")
            .env("PASEO_LIMIT_POLL_SECS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(p) = password {
            cmd.env("PASEO_PASSWORD", p);
        }
        cmd.envs(extra_env.iter().copied());
        let mut child = cmd.spawn().expect("start feiyi");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut exited = false;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                std::thread::sleep(Duration::from_millis(100));
                if matches!(child.try_wait(), Ok(Some(_))) {
                    exited = true;
                    break;
                }
                return Daemon { child, port, dir };
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon did not start listening on {port}");
        }
    }
    panic!("daemon kept exiting at start (port collisions?)");
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws")).await.expect("ws connect");
    ws
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), ws.next()).await.expect("timed out waiting for a frame") {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(_)) => continue,
            other => panic!("socket closed: {other:?}"),
        }
    }
}

/// Read frames until `pick` returns something.
async fn recv_until<T>(ws: &mut Ws, mut pick: impl FnMut(&Value) -> Option<T>) -> T {
    loop {
        let v = recv(ws).await;
        if let Some(found) = pick(&v) {
            return found;
        }
    }
}

async fn hello(ws: &mut Ws, auth: Option<&str>) -> Value {
    let mut msg = json!({ "type": "hello", "protocolVersion": 1, "clientId": "test", "clientType": "cli" });
    if let Some(p) = auth {
        msg["auth"] = json!({ "password": p });
    }
    send(ws, msg).await;
    recv(ws).await
}

fn session(msg: Value) -> Value {
    json!({ "type": "session", "message": msg })
}

#[test]
fn serves_the_web_ui() {
    let d = start("http", None);
    let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    let lower = body.to_ascii_lowercase();
    assert!(lower.contains("<html"));
    // Pairing page only: behind the password, never framed or cached, no agent/project RPCs.
    assert!(lower.contains("x-frame-options: deny"), "{body}");
    assert!(lower.contains("cache-control: no-store"), "{body}");
    assert!(lower.contains("type=\"password\""));
    assert!(lower.contains("daemon.get_pairing_offer.request"));
    assert!(!lower.contains("fetch_agents_request"));
}

#[tokio::test]
async fn handshake_announces_features() {
    let d = start("hello", None);
    let mut ws = connect(d.port).await;
    let info = hello(&mut ws, None).await;
    let payload = &info["message"]["payload"];
    assert_eq!(payload["status"], "server_info");
    assert_eq!(payload["features"]["projectAdd"], true);
    assert_eq!(payload["features"]["providerRemoval"], true);
    assert_eq!(payload["features"]["usageSources"], true);
}

#[tokio::test]
async fn rejects_incompatible_protocol_and_bad_password() {
    let d = start("auth", Some("secret"));
    let mut ws = connect(d.port).await;
    send(&mut ws, json!({ "type": "hello", "protocolVersion": 99 })).await;
    assert_eq!(recv(&mut ws).await["type"], "hello.rejected");

    let mut ws = connect(d.port).await;
    let r = hello(&mut ws, None).await;
    assert_eq!(r["type"], "hello.rejected");
    assert_eq!(r["reason"], "password_required");

    let mut ws = connect(d.port).await;
    let r = hello(&mut ws, Some("wrong")).await;
    assert_eq!(r["reason"], "incorrect_password");

    let mut ws = connect(d.port).await;
    let r = hello(&mut ws, Some("secret")).await;
    assert_eq!(r["message"]["payload"]["status"], "server_info");
}

/// The app offers its password as a `paseo.bearer.<password>` subprotocol and
/// fails the connection (1006) when the server does not select it.
#[tokio::test]
async fn selects_the_bearer_subprotocol_the_app_offers() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let d = start("bearer", Some("secret"));
    let mut req = format!("ws://127.0.0.1:{}/ws", d.port).into_client_request().unwrap();
    req.headers_mut().insert("Sec-WebSocket-Protocol", "paseo.bearer.secret".parse().unwrap());
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.expect("ws connect with subprotocol");
    assert_eq!(resp.headers()["sec-websocket-protocol"], "paseo.bearer.secret");
    let r = hello(&mut ws, Some("secret")).await;
    assert_eq!(r["message"]["payload"]["status"], "server_info");
}

#[tokio::test]
async fn unsupported_requests_get_an_rpc_error() {
    let d = start("rpcerr", None);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(&mut ws, session(json!({ "type": "create_terminal_request", "requestId": "r1" }))).await;
    let err = recv_until(&mut ws, |v| (v["message"]["type"] == "rpc_error").then(|| v["message"]["payload"].clone())).await;
    assert_eq!(err["requestId"], "r1");
}

#[tokio::test]
async fn daemon_config_custom_models_round_trip() {
    let d = start("config", None);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let patch = json!({ "providers": { "claude": { "additionalModels": [{ "id": "my-custom-model", "label": "Mine" }] } } });
    send(&mut ws, session(json!({ "type": "set_daemon_config_request", "requestId": "c1", "config": patch }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "set_daemon_config_response").then_some(())).await;

    send(&mut ws, session(json!({ "type": "list_provider_models_request", "requestId": "c2", "provider": "claude" }))).await;
    let models = recv_until(&mut ws, |v| (v["message"]["type"] == "list_provider_models_response").then(|| v["message"]["payload"]["models"].clone())).await;
    assert!(models.as_array().unwrap().iter().any(|m| m["id"] == "my-custom-model" && m["label"] == "Mine"));

    // Persisted: a second connection sees it in get_daemon_config.
    let mut ws2 = connect(d.port).await;
    hello(&mut ws2, None).await;
    send(&mut ws2, session(json!({ "type": "get_daemon_config_request", "requestId": "c3" }))).await;
    let cfg = recv_until(&mut ws2, |v| (v["message"]["type"] == "get_daemon_config_response").then(|| v["message"]["payload"]["config"].clone())).await;
    assert_eq!(cfg["providers"]["claude"]["additionalModels"][0]["id"], "my-custom-model");
}

#[tokio::test]
async fn project_add_and_list() {
    let d = start("project", None);
    let proj = d.dir.join("myproject");
    std::fs::create_dir_all(&proj).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(&mut ws, session(json!({ "type": "project.add.request", "requestId": "p1", "cwd": proj.to_string_lossy() }))).await;
    let added = recv_until(&mut ws, |v| (v["message"]["type"] == "project.add.response").then(|| v["message"]["payload"].clone())).await;
    assert!(added["error"].is_null(), "{added}");
    send(&mut ws, session(json!({ "type": "project.list.request", "requestId": "p2" }))).await;
    let list = recv_until(&mut ws, |v| (v["message"]["type"] == "project.list.response").then(|| v["message"]["payload"]["projects"].clone())).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["projectDisplayName"], "myproject");
}

#[tokio::test]
async fn agent_turn_with_fake_claude() {
    let d = start("agent", None);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({
            "type": "create_agent_request",
            "requestId": "a1",
            "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" },
            "initialPrompt": "say hi",
        })),
    )
    .await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;

    // The turn completes, and the assistant text lands in the timeline.
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    send(&mut ws, session(json!({ "type": "fetch_agent_timeline_request", "requestId": "t1", "agentId": agent_id, "direction": "tail", "limit": 50 }))).await;
    let tl = recv_until(&mut ws, |v| (v["message"]["type"] == "fetch_agent_timeline_response").then(|| v["message"]["payload"].clone())).await;
    assert!(tl["error"].is_null(), "{tl}");
    let items: Vec<String> = tl["entries"].as_array().unwrap().iter().map(|e| e["item"]["type"].as_str().unwrap().to_string()).collect();
    assert!(items.contains(&"user_message".to_string()), "{items:?}");
    assert!(items.contains(&"assistant_message".to_string()), "{items:?}");
    let text = tl["entries"].as_array().unwrap().iter().find(|e| e["item"]["type"] == "assistant_message").unwrap()["item"]["text"].as_str().unwrap().to_string();
    assert!(text.contains("hello from fake claude"));

    // Usage: the turn's tokens and cost are booked for Claude, streamed per provider.
    send(&mut ws, session(json!({ "type": "usage.list_reports.request", "requestId": "u1" }))).await;
    let mut reports = Vec::new();
    loop {
        let v = recv_until(&mut ws, |v| {
            let t = v["message"]["type"].as_str().unwrap_or("");
            (t.starts_with("usage.list_reports.") && v["message"]["payload"]["requestId"] == "u1").then(|| v["message"].clone())
        })
        .await;
        if v["type"] == "usage.list_reports.response" {
            assert!(v["payload"]["error"].is_null(), "{v}");
            break;
        }
        reports.push(v["payload"]["report"].clone());
    }
    let claude = reports.iter().find(|r| r["id"] == "claude").expect("claude report");
    let balance = |r: &Value, id: &str| r["report"]["balances"].as_array().unwrap().iter().find(|b| b["id"] == id).unwrap()["used"].clone();
    assert_eq!(balance(claude, "tokens-today"), 110, "{claude}");
    assert_eq!(balance(claude, "cost-today").as_f64(), Some(1.0));
    assert!(claude["report"]["details"].as_array().unwrap().iter().any(|d| d["label"] == "claude-test"), "{claude}");
    let model = reports.iter().find(|r| r["id"] == "claude:claude-test").expect("per-model card");
    assert_eq!(balance(model, "tokens-all"), 110, "{model}");
    assert_eq!(balance(model, "cost-7d").as_f64(), Some(1.0));
    // An agent sees its own provider, with its own spend first.
    send(&mut ws, session(json!({ "type": "usage.list_reports.request", "requestId": "u2", "agentId": agent_id }))).await;
    let mine = recv_until(&mut ws, |v| (v["message"]["type"] == "usage.list_reports.update" && v["message"]["payload"]["requestId"] == "u2").then(|| v["message"]["payload"]["report"].clone())).await;
    assert_eq!(mine["id"], "claude");
    assert_eq!(balance(&mine, "agent-tokens"), 110);

    // History lists the agent, and it can be archived.
    send(&mut ws, session(json!({ "type": "fetch_agent_history_request", "requestId": "h1" }))).await;
    let hist = recv_until(&mut ws, |v| (v["message"]["type"] == "fetch_agent_history_response").then(|| v["message"]["payload"].clone())).await;
    assert_eq!(hist["entries"].as_array().unwrap().len(), 1);
    send(&mut ws, session(json!({ "type": "archive_agent_request", "requestId": "z1", "agentId": agent_id }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_archived").then_some(())).await;
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

async fn rpc(ws: &mut Ws, msg: Value, response: &str) -> Value {
    send(ws, session(msg)).await;
    recv_until(ws, |v| (v["message"]["type"] == response).then(|| v["message"]["payload"].clone())).await
}

#[tokio::test]
async fn git_actions_commit_branch_stash_history() {
    let d = start("git", None);
    let repo = d.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let cwd = repo.to_string_lossy().to_string();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    // Commit with an explicit message, and with none (auto-generated).
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    let r = rpc(&mut ws, json!({ "type": "checkout_commit_request", "requestId": "g1", "cwd": cwd, "message": "Change a" }), "checkout_commit_response").await;
    assert_eq!(r["success"], true, "{r}");
    std::fs::write(repo.join("b.txt"), "new\n").unwrap();
    let r = rpc(&mut ws, json!({ "type": "checkout_commit_request", "requestId": "g2", "cwd": cwd }), "checkout_commit_response").await;
    assert_eq!(r["success"], true, "{r}");
    assert!(git(&repo, &["log", "-1", "--format=%s"]).contains("b.txt"));

    // Rename the branch, then check suggestions and validation see it.
    let r = rpc(&mut ws, json!({ "type": "checkout.rename_branch.request", "requestId": "g3", "cwd": cwd, "branch": "feat/x" }), "checkout.rename_branch.response").await;
    assert_eq!(r["currentBranch"], "feat/x", "{r}");
    let r = rpc(&mut ws, json!({ "type": "branch_suggestions_request", "requestId": "g4", "cwd": cwd, "query": "feat" }), "branch_suggestions_response").await;
    assert_eq!(r["branches"], json!(["feat/x"]), "{r}");
    let r = rpc(&mut ws, json!({ "type": "validate_branch_request", "requestId": "g5", "cwd": cwd, "branchName": "feat/x" }), "validate_branch_response").await;
    assert_eq!(r["exists"], true);
    let r = rpc(&mut ws, json!({ "type": "validate_branch_request", "requestId": "g6", "cwd": cwd, "branchName": "nope" }), "validate_branch_response").await;
    assert_eq!(r["exists"], false);

    // Stash a change, list it, pop it back.
    std::fs::write(repo.join("a.txt"), "three\n").unwrap();
    let r = rpc(&mut ws, json!({ "type": "stash_save_request", "requestId": "g7", "cwd": cwd, "branch": "feat/x" }), "stash_save_response").await;
    assert_eq!(r["success"], true, "{r}");
    let r = rpc(&mut ws, json!({ "type": "stash_list_request", "requestId": "g8", "cwd": cwd }), "stash_list_response").await;
    assert_eq!(r["entries"][0]["branch"], "feat/x", "{r}");
    assert_eq!(r["entries"][0]["isPaseo"], true);
    let r = rpc(&mut ws, json!({ "type": "stash_pop_request", "requestId": "g9", "cwd": cwd, "stashIndex": 0 }), "stash_pop_response").await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "three\n");

    // Discard brings the file back to HEAD; untracked files are removed.
    std::fs::write(repo.join("junk.txt"), "x").unwrap();
    let r = rpc(&mut ws, json!({ "type": "checkout.discard_changes.request", "requestId": "g10", "cwd": cwd, "paths": ["a.txt", "junk.txt"] }), "checkout.discard_changes.response").await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two\n");
    assert!(!repo.join("junk.txt").exists());

    // Switch back to a new main and merge feat/x into it from the branch.
    git(&repo, &["branch", "main"]);
    let r = rpc(&mut ws, json!({ "type": "checkout_switch_branch_request", "requestId": "g11", "cwd": cwd, "branch": "main" }), "checkout_switch_branch_response").await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["source"], "local");

    // History with file stats, and a single commit's file diff.
    let r = rpc(&mut ws, json!({ "type": "checkout.commits.list.request", "requestId": "g12", "cwd": cwd }), "checkout.commits.list.response").await;
    let commits = r["commits"].as_array().unwrap();
    assert!(commits.len() >= 3, "{r}");
    let change_a = commits.iter().find(|c| c["subject"] == "Change a").unwrap();
    assert_eq!(change_a["files"][0]["path"], "a.txt");
    let sha = change_a["sha"].as_str().unwrap();
    let r = rpc(&mut ws, json!({ "type": "checkout.commits.file_diff.request", "requestId": "g13", "cwd": cwd, "sha": sha, "path": "a.txt" }), "checkout.commits.file_diff.response").await;
    assert_eq!(r["file"]["path"], "a.txt", "{r}");
    assert_eq!(r["file"]["additions"], 1);

    // Not a repository -> NOT_GIT_REPO; PR status without a remote -> no_remote.
    let r = rpc(&mut ws, json!({ "type": "checkout_push_request", "requestId": "g14", "cwd": d.dir.to_string_lossy() }), "checkout_push_response").await;
    assert_eq!(r["error"]["code"], "NOT_GIT_REPO", "{r}");
    let r = rpc(&mut ws, json!({ "type": "checkout_pr_status_request", "requestId": "g15", "cwd": cwd }), "checkout_pr_status_response").await;
    assert_eq!(r["authState"], "no_remote", "{r}");
    assert!(r["status"].is_null());
}

/// A turn in flight when the daemon is killed (container replaced) resumes by
/// itself after the restart, in the same conversation.
#[tokio::test]
async fn interrupted_turn_resumes_after_hard_kill() {
    let dir = unique_dir("resume");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    // First life: the "model" takes the prompt and never answers.
    let mut first = start_at(dir.clone(), None, "hang");
    let mut ws = connect(first.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "r1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "do the thing" })),
    )
    .await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    // Wait until the agent record with its running turn is on disk, then SIGKILL (no graceful flush).
    let agents_file = dir.join("home").join("agents.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let saved = std::fs::read_to_string(&agents_file).unwrap_or_default();
        if saved.contains("\"pendingTurn\":\"do the thing\"") {
            break;
        }
        assert!(Instant::now() < deadline, "pending turn never saved: {saved}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    first.child.kill().unwrap();
    let _ = first.child.wait();
    std::mem::forget(first); // keep the directory: it is the "volume"

    // Second life: a working model. The daemon resumes the turn on its own.
    let second = start_at(dir, None, "ok");
    let mut ws = connect(second.port).await;
    hello(&mut ws, None).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let tl = loop {
        let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "r2", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
        let has_answer = tl["entries"].as_array().unwrap().iter().any(|e| e["item"]["type"] == "assistant_message");
        if has_answer {
            break tl;
        }
        assert!(Instant::now() < deadline, "turn was not resumed: {tl}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let texts: Vec<String> = tl["entries"].as_array().unwrap().iter().filter(|e| e["item"]["type"] == "notification").map(|e| e["item"]["message"].as_str().unwrap().to_string()).collect();
    assert!(texts.iter().any(|t| t.contains("restarted")), "{texts:?}");
}

/// An app that was in the background catches up with `direction: "after"`.
/// A tool call row is created "running" and later updated in place to
/// "completed": catching up from a cursor at its start must return the update.
#[tokio::test]
async fn catch_up_returns_rows_updated_in_place() {
    let d = start_at(unique_dir("catchup"), None, "tool");
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "c1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "list files" })),
    )
    .await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;

    let tail = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "c2", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let tool = tail["entries"].as_array().unwrap().iter().find(|e| e["item"]["type"] == "tool_call").cloned().expect("tool call row");
    assert_eq!(tool["item"]["status"], "completed");
    let start = tool["seqStart"].as_u64().unwrap();
    assert!(tool["seqEnd"].as_u64().unwrap() > start, "the row was updated in place: {tool}");

    // The app last saw the row when it was created (cursor = its seqStart).
    let after = rpc(
        &mut ws,
        json!({ "type": "fetch_agent_timeline_request", "requestId": "c3", "agentId": agent_id, "direction": "after", "cursor": { "epoch": tail["epoch"], "seq": start }, "limit": 50 }),
        "fetch_agent_timeline_response",
    )
    .await;
    let entries = after["entries"].as_array().unwrap();
    let updated = entries.iter().find(|e| e["item"]["type"] == "tool_call").expect("updated tool call must be returned");
    assert_eq!(updated["item"]["status"], "completed");
    assert!(entries.iter().any(|e| e["item"]["type"] == "assistant_message"));
    assert_eq!(after["hasNewer"], false);
    assert_eq!(after["endCursor"]["seq"], after["window"]["maxSeq"]);
}

fn file_frame(op: u8, request_id: &str, body: &[u8]) -> Vec<u8> {
    let mut v = vec![op, request_id.len() as u8];
    v.extend_from_slice(request_id.as_bytes());
    v.extend_from_slice(body);
    v
}

/// The app's upload protocol: file.upload.request, then binary frames
/// (begin / chunks / end); the file is then attached to a message.
#[tokio::test]
async fn upload_file_and_attach_it_with_an_image() {
    let d = start("upload", None);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let content = b"line one\nline two\n";
    send(&mut ws, session(json!({ "type": "file.upload.request", "requestId": "up1", "fileName": "../notes.txt", "mimeType": "text/plain", "size": content.len(), "modifiedAt": "2026-10-11T00:00:00Z" }))).await;
    let meta = br#"{"mime":"text/plain","size":18,"encoding":"binary","modifiedAt":"2026-10-11T00:00:00Z"}"#;
    let mut begin_body = (meta.len() as u16).to_be_bytes().to_vec();
    begin_body.extend_from_slice(meta);
    ws.send(Message::Binary(file_frame(0x10, "up1", &begin_body))).await.unwrap();
    ws.send(Message::Binary(file_frame(0x11, "up1", &content[..9]))).await.unwrap();
    ws.send(Message::Binary(file_frame(0x11, "up1", &content[9..]))).await.unwrap();
    ws.send(Message::Binary(file_frame(0x12, "up1", b""))).await.unwrap();
    let up = recv_until(&mut ws, |v| (v["message"]["type"] == "file.upload.response").then(|| v["message"]["payload"].clone())).await;
    assert!(up["error"].is_null(), "{up}");
    let file = up["file"].clone();
    assert_eq!(file["fileName"], "notes.txt", "path components are stripped");
    let saved = std::fs::read(file["path"].as_str().unwrap()).unwrap();
    assert_eq!(saved, content);

    // Create an agent and send a message with the file and an inline image.
    send(&mut ws, session(json!({ "type": "create_agent_request", "requestId": "a1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" } }))).await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    let png = "iVBORw0KGgo=";
    send(
        &mut ws,
        session(json!({ "type": "send_agent_message_request", "requestId": "m1", "agentId": agent_id, "text": "what is in these?", "images": [{ "data": png, "mimeType": "image/png" }], "attachments": [file] })),
    )
    .await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "t1", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let bubble = tl["entries"].as_array().unwrap().iter().find(|e| e["item"]["type"] == "user_message").unwrap()["item"]["text"].as_str().unwrap().to_string();
    assert!(bubble.starts_with("what is in these?"), "{bubble}");
    assert!(bubble.contains("📎") && bubble.contains("notes.txt") && bubble.contains("image.png"), "{bubble}");
    assert!(!bubble.contains("Path:"), "agent notes stay out of the bubble: {bubble}");
}

#[tokio::test]
async fn streamed_text_is_sent_in_batches() {
    let d = start_at(unique_dir("stream"), None, "stream");
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "s1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "talk" })),
    )
    .await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    // Count the assistant text messages until the turn ends.
    let mut texts = Vec::new();
    recv_until(&mut ws, |v| {
        let ev = &v["message"]["payload"]["event"];
        if v["message"]["type"] != "agent_stream" {
            return None;
        }
        if ev["type"] == "timeline" && ev["item"]["type"] == "assistant_message" {
            texts.push(ev["item"]["text"].as_str().unwrap_or("").to_string());
        }
        (ev["type"] == "turn_completed").then_some(())
    })
    .await;
    let expected: String = (0..20).map(|i| format!("w{i} ")).collect();
    assert_eq!(texts.concat(), expected);
    assert!(texts.len() < 20, "deltas were not batched: {} messages", texts.len());
    // The stored row has the whole text, before the turn ended.
    let tail = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "s2", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let rows: Vec<&Value> = tail["entries"].as_array().unwrap().iter().filter(|e| e["item"]["type"] == "assistant_message").collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["item"]["text"], expected);
}

#[tokio::test]
async fn usage_limit_waits_and_continues() {
    let d = start_at(unique_dir("limit"), None, "limit");
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "l1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "build it" })),
    )
    .await;
    // The limit is announced, then the same turn carries on and completes: never fails.
    let mut noted = false;
    recv_until(&mut ws, |v| {
        let ev = &v["message"]["payload"]["event"];
        if v["message"]["type"] != "agent_stream" {
            return None;
        }
        assert_ne!(ev["type"], "turn_failed", "{ev}");
        if ev["type"] == "timeline" && ev["item"]["type"] == "notification" {
            noted |= ev["item"]["message"].as_str().unwrap_or("").starts_with("Usage limit reached");
        }
        (ev["type"] == "turn_completed").then_some(())
    })
    .await;
    assert!(noted, "the wait was not announced");
    assert!(d.dir.join("limit-hit").exists());
}

#[tokio::test]
async fn a_cold_resume_notice_result_does_not_end_the_prompts_turn() {
    let d = start_at(unique_dir("resume"), None, "resume");
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "r1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "continue" })),
    )
    .await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    // The empty result must not end the turn: the answer arrives before turn_completed.
    let mut seen = Vec::new();
    recv_until(&mut ws, |v| {
        let ev = &v["message"]["payload"]["event"];
        if v["message"]["type"] != "agent_stream" {
            return None;
        }
        assert_ne!(ev["type"], "turn_failed", "{ev}");
        if ev["type"] == "timeline" {
            seen.push(ev["item"].clone());
        }
        (ev["type"] == "turn_completed").then_some(())
    })
    .await;
    assert!(seen.iter().any(|i| i["type"] == "assistant_message" && i["text"].as_str().unwrap_or("").contains("answered after the notice")), "turn ended before the answer: {seen:?}");
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "r2", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let all = tl["entries"].as_array().unwrap().iter().map(|e| e["item"].to_string()).collect::<Vec<_>>().join("\n");
    assert!(all.contains("answered after the notice"), "{all}");
    assert!(!all.contains("Something went wrong") && !all.contains("no response"), "{all}");
}

/// Start a claude chat and wait for its first turn to finish.
async fn run_chat(ws: &mut Ws, cwd: &std::path::Path, id: &str) {
    std::fs::create_dir_all(cwd).unwrap();
    let config = json!({ "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" });
    send(ws, session(json!({ "type": "create_agent_request", "requestId": id, "config": config, "initialPrompt": "hi" }))).await;
    recv_until(ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
}

#[tokio::test]
async fn company_roles_reach_claude_with_memory_and_switch_off() {
    let dir = unique_dir("company");
    std::fs::create_dir_all(dir.join("home/roles")).unwrap();
    std::fs::write(dir.join("home/roles/engineer.md"), "- acme: tabs not spaces\n").unwrap();
    let d = start_at(dir, None, "args");
    let args_file = d.dir.join("claude-args");
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let roles = json!({ "roles": { "legal": null, "tester": { "description": "Tests things.", "prompt": "You test." } } });
    send(&mut ws, session(json!({ "type": "set_daemon_config_request", "requestId": "c1", "config": roles }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "set_daemon_config_response").then_some(())).await;

    run_chat(&mut ws, &d.dir.join("w1"), "a1").await;
    let args = std::fs::read_to_string(&args_file).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let at = lines.iter().position(|l| *l == "--agents").expect("--agents passed");
    let agents: Value = serde_json::from_str(lines[at + 1]).unwrap();
    assert!(agents["engineer"]["prompt"].as_str().unwrap().contains("- acme: tabs not spaces"), "memory injected");
    assert!(agents["tester"]["prompt"].as_str().unwrap().starts_with("You test."), "custom role added");
    assert!(agents.get("legal").is_none(), "null removes a built-in role");
    assert!(agents["qa"]["tools"].as_array().unwrap().contains(&json!("mcp__feiyi__remember")));
    assert!(args.contains("You lead a one-person company") && args.contains("tester"), "lead prompt names the team");

    send(&mut ws, session(json!({ "type": "set_daemon_config_request", "requestId": "c2", "config": { "company": false } }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "set_daemon_config_response").then_some(())).await;
    run_chat(&mut ws, &d.dir.join("w2"), "a2").await;
    let args = std::fs::read_to_string(&args_file).unwrap();
    assert!(!args.lines().any(|l| l == "--agents") && !args.contains("one-person company"), "company: false turns it off");
}

/// Run `cmd` with `input` on stdin; returns the exit code and stdout.
fn pipe(mut cmd: Command, input: &str) -> (Option<i32>, String) {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn");
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
}

fn hook_event(tool: &str, key: &str, value: &str) -> String {
    json!({ "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": { (key): value } }).to_string()
}

fn guard_hook(home: &std::path::Path, input: &str) -> (Option<i32>, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_feiyi"));
    cmd.arg("guard-hook").env("HOME", home).env("PASEO_LITE_HOME", home.join(".paseo-lite")).env_remove("PASEO_SECRET_GUARD");
    pipe(cmd, input)
}

fn assert_denied(out: (Option<i32>, String), what: &str) {
    assert_eq!(out.0, Some(0), "{what}");
    let v: Value = serde_json::from_str(&out.1).unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {:?}", out.1));
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{what}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse", "{what}");
}

fn assert_allowed(out: (Option<i32>, String), what: &str) {
    assert_eq!(out, (Some(0), String::new()), "{what}");
}

#[test]
fn guard_hook_denies_secrets_and_allows_ordinary_work() {
    let home = unique_dir("guard");
    let creds = home.join(".claude/.credentials.json");
    let upload = home.join(".paseo-lite/uploads/ab12/shot.png");
    for (tool, key, value) in [
        ("Read", "file_path", "~/.claude/.credentials.json"),
        ("Read", "file_path", creds.to_str().unwrap()),
        ("Bash", "command", "printenv"),
        ("Bash", "command", "echo $(env)"),
    ] {
        assert_denied(guard_hook(&home, &hook_event(tool, key, value)), value);
    }
    for (tool, key, value) in [
        ("Bash", "command", "grep -n 'env(' src/x.rs"),
        ("Bash", "command", "set -euo pipefail; ps aux"),
        ("Read", "file_path", "/work/src/main.rs"),
        ("Read", "file_path", upload.to_str().unwrap()),
    ] {
        assert_allowed(guard_hook(&home, &hook_event(tool, key, value)), value);
    }
    // Garbage on stdin never blocks a tool call.
    assert_allowed(guard_hook(&home, "not json"), "garbage");
    let _ = std::fs::remove_dir_all(&home);
}

/// The daemon as deployed holds every secret in its environment. Claude must
/// get none of the removed ones, no PID-namespace scrub flag, and a `--settings`
/// file whose guard hook command works when run exactly as written.
#[tokio::test]
async fn claude_child_gets_no_daemon_secrets_and_a_working_guard_hook() {
    let dir = unique_dir("prodenv");
    let env = [
        ("PASEO_ZILONG_API_KEY", "dummy-zilong"),
        ("PASEO_LITE_MCP_TOKEN", "dummy-mcp"),
        ("CODEX_API_KEY", "dummy-codex"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "dummy-oauth"),
        ("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB", "1"),
    ];
    let d = start_with(dir, Some("dummy-password"), "env", &env);
    let mut ws = connect(d.port).await;
    hello(&mut ws, Some("dummy-password")).await;
    run_chat(&mut ws, &d.dir.join("w1"), "a1").await;

    let child_env = std::fs::read_to_string(d.dir.join("claude-env")).unwrap();
    let keys: Vec<&str> = child_env.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
    assert!(child_env.lines().any(|l| l == "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=0"), "scrub must be off: it breaks Bash on Linux");
    for gone in ["PASEO_ZILONG_API_KEY", "PASEO_PASSWORD", "PASEO_LITE_MCP_TOKEN", "CODEX_API_KEY"] {
        assert!(!keys.contains(&gone), "{gone} leaked into the claude child");
    }
    for value in ["dummy-zilong", "dummy-password", "dummy-mcp", "dummy-codex"] {
        assert!(!child_env.contains(value), "{value} leaked into the claude child");
    }
    assert!(keys.contains(&"PATH") && keys.contains(&"HOME"), "{keys:?}");
    assert!(child_env.lines().any(|l| l == "CLAUDE_CODE_OAUTH_TOKEN=dummy-oauth"), "claude keeps its own token");

    let args = std::fs::read_to_string(d.dir.join("claude-args")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let at = lines.iter().position(|l| *l == "--settings").expect("--settings passed");
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(lines[at + 1]).unwrap()).unwrap();
    let command = settings["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|h| h["hooks"].as_array().unwrap())
        .filter_map(|h| h["command"].as_str())
        .find(|c| c.contains("guard-hook"))
        .expect("guard hook wired")
        .to_string();

    // Run the wired command string through a shell, as Claude Code does.
    let home = d.dir.join("hookhome");
    std::fs::create_dir_all(&home).unwrap();
    let wired = |input: &str| {
        let mut sh = Command::new("sh");
        sh.args(["-c", &command]).env("HOME", &home).env("PASEO_LITE_HOME", home.join(".paseo-lite"));
        pipe(sh, input)
    };
    assert_denied(wired(&hook_event("Bash", "command", "printenv")), "wired hook, printenv");
    assert_allowed(wired(&hook_event("Bash", "command", "ls -la")), "wired hook, ls");
}

/// The codex app-server gets the shell exclude policy. The fake codex records
/// its arguments and exits; the daemon spawns it when a codex agent is created.
#[tokio::test]
async fn codex_gets_the_shell_environment_exclude() {
    let dir = unique_dir("codexargs");
    let codex = dir.join("fake-codex.sh");
    std::fs::write(&codex, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", dir.join("codex-args").display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let d = start_with(dir, None, "ok", &[("PASEO_CODEX_BIN", codex.to_str().unwrap())]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let cwd = d.dir.join("w1");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = json!({ "provider": "codex", "cwd": cwd.to_string_lossy() });
    send(&mut ws, session(json!({ "type": "create_agent_request", "requestId": "x1", "config": config, "initialPrompt": "hi" }))).await;
    let file = d.dir.join("codex-args");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !file.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let args = std::fs::read_to_string(&file).expect("codex was started");
    assert!(args.lines().any(|l| l.starts_with("shell_environment_policy.exclude=") && l.contains("PASEO_*") && l.contains("*API_KEY*")), "{args}");
}

/// Poll `schedule/inspect` until the schedule has `runs` runs and none is running.
async fn schedule_settled(ws: &mut Ws, id: &str, runs: usize) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let r = rpc(ws, json!({ "type": "schedule/inspect", "requestId": "poll", "scheduleId": id }), "schedule/inspect/response").await;
        let list = r["schedule"]["runs"].as_array().expect("runs").clone();
        if list.len() >= runs && list.iter().all(|run| run["status"] != "running") {
            return r["schedule"].clone();
        }
        assert!(Instant::now() < deadline, "schedule runs did not finish: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn new_agent_schedule(request_id: &str, cwd: &std::path::Path) -> Value {
    json!({
        "type": "schedule/create",
        "requestId": request_id,
        "name": "build check",
        "prompt": "say hi",
        "cadence": { "type": "every", "everyMs": 3_600_000 },
        "target": { "type": "new-agent", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" } },
    })
}

#[tokio::test]
async fn schedules_create_list_run_now_pause_update_delete() {
    let d = start("schedules", None);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let created = rpc(&mut ws, new_agent_schedule("s1", &cwd), "schedule/create/response").await;
    assert!(created["error"].is_null(), "{created}");
    let schedule = &created["schedule"];
    assert_eq!((schedule["status"].as_str(), schedule["name"].as_str(), schedule["maxRuns"].is_null()), (Some("active"), Some("build check"), true), "{created}");
    assert!(schedule.get("runs").is_none(), "{created}");
    let id = schedule["id"].as_str().unwrap().to_string();

    // An interval schedule runs on creation: a new agent answers, and its output is recorded.
    let first = schedule_settled(&mut ws, &id, 1).await;
    let run = &first["runs"][0];
    assert_eq!(run["status"], "succeeded", "{first}");
    assert!(run["output"].as_str().unwrap().contains("hello from fake claude"), "{first}");
    assert!(run["agentId"].is_string() && run["error"].is_null() && run["endedAt"].is_string(), "{first}");
    assert!(first["lastRunAt"].is_string() && first["nextRunAt"].is_string(), "{first}");

    let listed = rpc(&mut ws, json!({ "type": "schedule/list", "requestId": "s2" }), "schedule/list/response").await;
    assert_eq!(listed["schedules"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["schedules"][0]["id"], id.as_str());
    assert!(listed["schedules"][0].get("runs").is_none());

    // Run now adds a run without moving the cadence.
    let ran = rpc(&mut ws, json!({ "type": "schedule/run-once", "requestId": "s3", "scheduleId": id }), "schedule/run-once/response").await;
    assert!(ran["error"].is_null(), "{ran}");
    assert_eq!(ran["schedule"]["runs"].as_array().unwrap().len(), 2, "{ran}");
    let both = schedule_settled(&mut ws, &id, 2).await;
    assert!(both["runs"].as_array().unwrap().iter().all(|r| r["status"] == "succeeded"), "{both}");
    assert_eq!(both["nextRunAt"], first["nextRunAt"]);

    // Each run's agent is archived once it finishes.
    let live = rpc(&mut ws, json!({ "type": "fetch_agents_request", "requestId": "s4" }), "fetch_agents_response").await;
    assert!(live["entries"].as_array().unwrap().is_empty(), "{live}");
    let history = rpc(&mut ws, json!({ "type": "fetch_agent_history_request", "requestId": "s5" }), "fetch_agent_history_response").await;
    assert_eq!(history["entries"].as_array().unwrap().len(), 2, "{history}");
    assert!(history["entries"].as_array().unwrap().iter().all(|e| e["agent"]["archivedAt"].is_string()), "{history}");

    let logs = rpc(&mut ws, json!({ "type": "schedule/logs", "requestId": "s6", "scheduleId": id }), "schedule/logs/response").await;
    assert_eq!(logs["runs"].as_array().unwrap().len(), 2, "{logs}");

    let paused = rpc(&mut ws, json!({ "type": "schedule/pause", "requestId": "s7", "scheduleId": id }), "schedule/pause/response").await;
    assert_eq!((paused["schedule"]["status"].as_str(), paused["schedule"]["nextRunAt"].is_null(), paused["schedule"]["pausedAt"].is_string()), (Some("paused"), true, true), "{paused}");
    let resumed = rpc(&mut ws, json!({ "type": "schedule/resume", "requestId": "s8", "scheduleId": id }), "schedule/resume/response").await;
    assert_eq!((resumed["schedule"]["status"].as_str(), resumed["schedule"]["nextRunAt"].is_string(), resumed["schedule"]["pausedAt"].is_null()), (Some("active"), true, true), "{resumed}");

    let updated = rpc(&mut ws, json!({ "type": "schedule/update", "requestId": "s9", "scheduleId": id, "name": "renamed", "cadence": { "type": "cron", "expression": "0 9 * * 1-5" }, "maxRuns": 5 }), "schedule/update/response").await;
    assert!(updated["error"].is_null(), "{updated}");
    assert_eq!((updated["schedule"]["name"].as_str(), updated["schedule"]["maxRuns"].as_u64()), (Some("renamed"), Some(5)), "{updated}");
    assert_eq!(updated["schedule"]["cadence"], json!({ "type": "cron", "expression": "0 9 * * 1-5" }));
    assert_eq!(updated["schedule"]["runs"].as_array().unwrap().len(), 2);

    let deleted = rpc(&mut ws, json!({ "type": "schedule/delete", "requestId": "s10", "scheduleId": id }), "schedule/delete/response").await;
    assert_eq!((deleted["scheduleId"].as_str(), deleted["error"].is_null()), (Some(id.as_str()), true), "{deleted}");
    let listed = rpc(&mut ws, json!({ "type": "schedule/list", "requestId": "s11" }), "schedule/list/response").await;
    assert!(listed["schedules"].as_array().unwrap().is_empty(), "{listed}");

    send(&mut ws, session(json!({ "type": "schedule/inspect", "requestId": "s12", "scheduleId": id }))).await;
    let err = recv_until(&mut ws, |v| (v["message"]["type"] == "rpc_error").then(|| v["message"]["payload"].clone())).await;
    assert_eq!((err["requestId"].as_str(), err["code"].as_str()), (Some("s12"), Some("schedule_request_failed")), "{err}");
    assert!(err["error"].as_str().unwrap().contains("not found"), "{err}");

    send(&mut ws, session(json!({ "type": "schedule/create", "requestId": "s13", "prompt": "x", "cadence": { "type": "cron", "expression": "nope" }, "target": { "type": "agent", "agentId": "a" } }))).await;
    let err = recv_until(&mut ws, |v| (v["message"]["type"] == "rpc_error").then(|| v["message"]["payload"].clone())).await;
    assert!(err["error"].as_str().unwrap().contains("5 fields"), "{err}");
}

#[tokio::test]
async fn schedules_persist_and_a_full_daemon_records_a_skipped_run() {
    let dir = unique_dir("schedcap");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let d = start_with(dir, None, "hang", &[("PASEO_CLAUDE_MAX_LIVE", "1")]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    // One agent occupies the only slot and never answers.
    send(&mut ws, session(json!({ "type": "create_agent_request", "requestId": "a1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy() }, "initialPrompt": "wait" }))).await;
    recv_until(&mut ws, |v| (v["message"]["payload"]["status"] == "agent_created").then_some(())).await;

    let created = rpc(&mut ws, new_agent_schedule("s1", &cwd), "schedule/create/response").await;
    let id = created["schedule"]["id"].as_str().unwrap().to_string();
    let settled = schedule_settled(&mut ws, &id, 1).await;
    let run = &settled["runs"][0];
    assert_eq!(run["status"], "failed", "{settled}");
    assert!(run["error"].as_str().unwrap().contains("at capacity"), "{settled}");
    assert!(run["agentId"].is_null(), "{settled}");
    assert_eq!(settled["status"], "active");
    assert!(settled["nextRunAt"].is_string(), "{settled}");

    // A second connection reads the same schedules, and they are on disk for a restart.
    let mut ws2 = connect(d.port).await;
    hello(&mut ws2, None).await;
    let listed = rpc(&mut ws2, json!({ "type": "schedule/list", "requestId": "s2" }), "schedule/list/response").await;
    assert_eq!(listed["schedules"][0]["id"], id.as_str());
    let saved: Value = serde_json::from_slice(&std::fs::read(d.dir.join("home").join("schedules.json")).unwrap()).unwrap();
    assert_eq!(saved[0]["id"], id.as_str());
    assert_eq!(saved[0]["runs"][0]["status"], "failed");
}
