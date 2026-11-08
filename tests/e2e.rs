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
        // Like "ok", and appends every user line it receives to `claude-stdin`.
        "stdin" => script.replace("n=$((n+1))", &format!("n=$((n+1)); printf '%s\\n' \"$line\" >> '{}'", dir.join("claude-stdin").display())),
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
    send(&mut ws, session(json!({ "type": "voice_not_in_feiyi_request", "requestId": "r1" }))).await;
    let err = recv_until(&mut ws, |v| (v["message"]["type"] == "rpc_error").then(|| v["message"]["payload"].clone())).await;
    assert_eq!(err["requestId"], "r1");
}

#[tokio::test]
async fn session_ping_gets_a_pong() {
    let d = start("ping", None);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let pong = rpc(&mut ws, json!({ "type": "ping", "requestId": "p1", "clientSentAt": 42 }), "pong").await;
    assert_eq!(pong["requestId"], "p1", "{pong}");
    assert_eq!(pong["clientSentAt"], 42, "{pong}");
    assert!(pong["serverSentAt"].as_u64().unwrap() > 0, "{pong}");
}

#[tokio::test]
async fn file_subscription_pushes_updates_and_entries_duplicate() {
    let d = start("filesub", None);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("a.txt"), "one").unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    let info = hello(&mut ws, None).await;
    assert_eq!(info["message"]["payload"]["features"]["fsEntryDuplicate"], true);

    let sub = rpc(&mut ws, json!({ "type": "fs.file.subscribe.request", "requestId": "s1", "cwd": cwd, "path": "a.txt", "subscriptionId": "sub1" }), "fs.file.subscribe.response").await;
    assert_eq!(sub["subscriptionId"], "sub1");
    assert_eq!(sub["initial"]["status"], "ready");
    assert_eq!(sub["initial"]["size"], 3);

    std::fs::write(cwd.clone() + "/a.txt", "changed").unwrap();
    let update = recv_until(&mut ws, |v| (v["message"]["type"] == "fs.file.update").then(|| v["message"]["payload"].clone())).await;
    assert_eq!(update["subscriptionId"], "sub1");
    assert_eq!(update["version"]["size"], 7);

    let un = rpc(&mut ws, json!({ "type": "fs.file.unsubscribe.request", "requestId": "s2", "subscriptionId": "sub1" }), "fs.file.unsubscribe.response").await;
    assert_eq!(un["subscriptionId"], "sub1");

    let dup = rpc(&mut ws, json!({ "type": "fs.entry.duplicate.request", "requestId": "d1", "cwd": cwd, "path": "a.txt" }), "fs.entry.duplicate.response").await;
    assert_eq!(dup["success"], true, "{dup}");
    assert_eq!(dup["duplicatedPath"], "a copy.txt");
    assert_eq!(std::fs::read_to_string(cwd.clone() + "/a copy.txt").unwrap(), "changed");
    let outside = rpc(&mut ws, json!({ "type": "fs.entry.duplicate.request", "requestId": "d2", "cwd": cwd, "path": "../x" }), "fs.entry.duplicate.response").await;
    assert_eq!(outside["success"], false);
    assert!(outside["duplicatedPath"].is_null());
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
    for m in models.as_array().unwrap() {
        let id = m["id"].as_str().unwrap();
        if id.contains("opus") || id.contains("sonnet") {
            assert_eq!(m["defaultThinkingOptionId"], "medium", "{id}");
        } else if id.contains("haiku") {
            assert!(m.get("defaultThinkingOptionId").is_none(), "{id}");
        }
    }

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

    // Clearing attention answers with the agents' snapshots, as the app's schema requires,
    // and still broadcasts the agent_update.
    send(&mut ws, session(json!({ "type": "clear_agent_attention", "requestId": "c1", "agentId": [agent_id] }))).await;
    let mut updated = false;
    let cleared = recv_until(&mut ws, |v| {
        let m = &v["message"];
        updated |= m["type"] == "agent_update" && m["payload"]["kind"] == "upsert" && m["payload"]["agent"]["id"] == agent_id && m["payload"]["agent"]["requiresAttention"] == false;
        (m["type"] == "clear_agent_attention_response").then(|| m["payload"].clone())
    })
    .await;
    // The update rides the broadcast channel and the response the connection's own, so either can arrive first.
    if !updated {
        recv_until(&mut ws, |v| {
            let m = &v["message"];
            (m["type"] == "agent_update" && m["payload"]["kind"] == "upsert" && m["payload"]["agent"]["id"] == agent_id && m["payload"]["agent"]["requiresAttention"] == false).then_some(())
        })
        .await;
    }
    assert_eq!(cleared["requestId"], "c1", "{cleared}");
    assert_eq!(cleared["agents"].as_array().unwrap().len(), 1, "{cleared}");
    assert_eq!(cleared["agents"][0]["id"], agent_id, "{cleared}");
    assert_eq!(cleared["agents"][0]["requiresAttention"], false, "{cleared}");
    assert!(cleared["agents"][0]["attentionReason"].is_null(), "{cleared}");
    let single = rpc(&mut ws, json!({ "type": "clear_agent_attention", "requestId": "c2", "agentId": agent_id }), "clear_agent_attention_response").await;
    assert_eq!(single["agentId"], agent_id, "{single}");
    assert_eq!(single["agents"][0]["id"], agent_id, "{single}");
    let unknown = rpc(&mut ws, json!({ "type": "clear_agent_attention", "requestId": "c3", "agentId": "no-such-agent" }), "clear_agent_attention_response").await;
    assert_eq!(unknown["agents"], json!([]), "{unknown}");
    assert_eq!(unknown["requestId"], "c3", "{unknown}");

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

/// A turn whose "finished" notice was held for background work that died with
/// the daemon gets the notice on the next start, once.
#[tokio::test]
async fn held_finished_notice_is_sent_after_restart() {
    let dir = unique_dir("held-finish");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut first = start_at(dir.clone(), None, "ok");
    let mut ws = connect(first.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "r1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "say hi" })),
    )
    .await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    let agents_file = dir.join("home").join("agents.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&agents_file).unwrap_or_default().contains("\"attentionReason\":\"finished\"") {
        assert!(Instant::now() < deadline, "finished turn never saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    first.child.kill().unwrap();
    let _ = first.child.wait();
    std::mem::forget(first);

    // As if the turn had ended with a background shell running: notice held, not sent.
    let mut agents: Value = serde_json::from_slice(&std::fs::read(&agents_file).unwrap()).unwrap();
    agents[0]["finishDeferred"] = json!(true);
    agents[0]["attentionReason"] = Value::Null;
    agents[0]["attentionAt"] = Value::Null;
    std::fs::write(&agents_file, serde_json::to_vec(&agents).unwrap()).unwrap();

    let _second = start_at(dir, None, "ok");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let saved: Value = serde_json::from_slice(&std::fs::read(&agents_file).unwrap()).unwrap();
        if saved[0]["attentionReason"] == "finished" && saved[0]["finishDeferred"] == false {
            break;
        }
        assert!(Instant::now() < deadline, "held notice not sent: {saved}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run one agent through a finished turn on a throwaway daemon, kill the
/// daemon, and return its directory and `agents.json` for the test to edit.
async fn finished_agent_dir(tag: &str) -> (PathBuf, PathBuf) {
    let dir = unique_dir(tag);
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut first = start_at(dir.clone(), None, "ok");
    let mut ws = connect(first.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "r1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "say hi" })),
    )
    .await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    let agents_file = dir.join("home").join("agents.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&agents_file).unwrap_or_default().contains("\"attentionReason\":\"finished\"") {
        assert!(Instant::now() < deadline, "finished turn never saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    first.child.kill().unwrap();
    let _ = first.child.wait();
    std::mem::forget(first);
    (dir, agents_file)
}

fn edit_agent(agents_file: &std::path::Path, edit: impl FnOnce(&mut Value)) {
    let mut agents: Value = serde_json::from_slice(&std::fs::read(agents_file).unwrap()).unwrap();
    edit(&mut agents[0]);
    std::fs::write(agents_file, serde_json::to_vec(&agents).unwrap()).unwrap();
}

fn saved_agent(agents_file: &std::path::Path) -> Value {
    let saved: Value = serde_json::from_slice(&std::fs::read(agents_file).unwrap()).unwrap();
    saved[0].clone()
}

/// An agent that still has a turn or follow-ups to run is not idle with lost
/// background work: the restart resumes it and sends no "finished" notice.
#[tokio::test]
async fn held_finished_notice_waits_for_pending_work_after_restart() {
    for (tag, pending_turn, pending_queue) in [("held-pt", Some("do the thing"), json!([])), ("held-pq", None, json!(["next one"]))] {
        let (dir, agents_file) = finished_agent_dir(tag).await;
        edit_agent(&agents_file, |a| {
            a["finishDeferred"] = json!(true);
            a["attentionReason"] = Value::Null;
            a["attentionAt"] = Value::Null;
            a["pendingTurn"] = pending_turn.map(|t| json!(t)).unwrap_or(Value::Null);
            a["pendingQueue"] = pending_queue.clone();
        });
        // The fake CLI hangs on the resumed prompt, so the turn stays running.
        let mut d = start_at(dir, None, "hang");
        tokio::time::sleep(Duration::from_secs(4)).await;
        let a = saved_agent(&agents_file);
        assert!(a["attentionReason"].is_null(), "{tag}: notified while work pending: {a}");
        if pending_turn.is_none() {
            // The follow-up started as a new user turn, which drops the held notice.
            assert_eq!(a["finishDeferred"], false, "{tag}: {a}");
        }
        d.child.kill().unwrap();
        let _ = d.child.wait();
    }
}

/// An `agents.json` written before the flag existed loads, keeps its saved
/// attention, and is not notified again.
#[tokio::test]
async fn agents_file_without_finish_deferred_loads() {
    let (dir, agents_file) = finished_agent_dir("held-old").await;
    edit_agent(&agents_file, |a| {
        a.as_object_mut().unwrap().remove("finishDeferred");
    });
    let before = saved_agent(&agents_file);
    assert!(before.get("finishDeferred").is_none());
    let d = start_at(dir, None, "ok");
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let listed = rpc(&mut ws, json!({ "type": "fetch_agents_request", "requestId": "f1" }), "fetch_agents_response").await;
    assert_eq!(listed["entries"].as_array().map(|e| e.len()), Some(1), "{listed}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = saved_agent(&agents_file);
    assert_eq!(after["attentionReason"], "finished", "{after}");
    assert_eq!(after["attentionAt"], before["attentionAt"], "no new notice for an old record: {after}");
    assert_ne!(after["finishDeferred"], true, "{after}");
}

/// The held notice goes out once: later restarts leave it alone.
#[tokio::test]
async fn held_finished_notice_is_not_repeated() {
    let (dir, agents_file) = finished_agent_dir("held-once").await;
    edit_agent(&agents_file, |a| {
        a["finishDeferred"] = json!(true);
        a["attentionReason"] = Value::Null;
        a["attentionAt"] = Value::Null;
    });
    let mut second = start_at(dir.clone(), None, "ok");
    let deadline = Instant::now() + Duration::from_secs(20);
    let sent = loop {
        let a = saved_agent(&agents_file);
        if a["attentionReason"] == "finished" && a["finishDeferred"] == false {
            break a;
        }
        assert!(Instant::now() < deadline, "held notice not sent: {a}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    second.child.kill().unwrap();
    let _ = second.child.wait();
    let _third = start_at(dir, None, "ok");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = saved_agent(&agents_file);
    assert_eq!(after["attentionAt"], sent["attentionAt"], "notice repeated: {after}");
    assert_eq!(after["finishDeferred"], false, "{after}");
}

/// An archived agent is not worth a push: its held notice is dropped at startup.
#[tokio::test]
async fn archived_agent_is_not_notified_of_held_finish() {
    let (dir, agents_file) = finished_agent_dir("held-arch").await;
    edit_agent(&agents_file, |a| {
        a["finishDeferred"] = json!(true);
        a["attentionReason"] = Value::Null;
        a["attentionAt"] = Value::Null;
        a["archivedAt"] = json!("2026-01-01T00:00:00.000Z");
    });
    let _d = start_at(dir, None, "ok");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let a = saved_agent(&agents_file);
    assert!(a["attentionReason"].is_null(), "archived agent notified: {a}");
}

/// An unloaded timeline is read from disk off the manager task; fetching it
/// and listing its prompts must still answer with the saved rows.
#[tokio::test]
async fn cold_timeline_answers_fetch_and_list_prompts() {
    let d = start_with(unique_dir("cold"), None, "ok", &[("PASEO_TIMELINE_UNLOAD_SECS", "1")]);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(
        &mut ws,
        session(json!({ "type": "create_agent_request", "requestId": "k1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "say hi" })),
    )
    .await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;

    // Housekeeping ticks every 2 s and unloads idle timelines older than the limit.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "k2", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    assert!(tl["error"].is_null(), "{tl}");
    let items: Vec<&str> = tl["entries"].as_array().unwrap().iter().map(|e| e["item"]["type"].as_str().unwrap()).collect();
    assert!(items.contains(&"user_message") && items.contains(&"assistant_message"), "{items:?}");

    tokio::time::sleep(Duration::from_secs(6)).await;
    let prompts = rpc(&mut ws, json!({ "type": "agent.timeline.list_prompts.request", "requestId": "k3", "agentId": agent_id }), "agent.timeline.list_prompts.response").await;
    assert!(prompts["error"].is_null(), "{prompts}");
    let previews: Vec<&str> = prompts["prompts"].as_array().unwrap().iter().map(|p| p["preview"].as_str().unwrap()).collect();
    assert_eq!(previews, ["say hi"], "{prompts}");
}

/// Create an agent, let its first turn finish, and wait until housekeeping has
/// unloaded its timeline (PASEO_TIMELINE_UNLOAD_SECS=1, tick every 2 s).
async fn cold_agent(ws: &mut Ws, cwd: &std::path::Path) -> String {
    send(
        ws,
        session(json!({ "type": "create_agent_request", "requestId": "c1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "say hi" })),
    )
    .await;
    let agent_id = recv_until(ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    recv_until(ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    agent_id
}

/// Several cold requests for one agent, sent back to back with a ping among
/// them: each gets exactly one response, the same rows a warm fetch returns,
/// and the manager answers the ping without waiting for the disk read.
#[tokio::test]
async fn concurrent_cold_requests_each_get_one_correct_response() {
    let d = start_with(unique_dir("coldmany"), None, "ok", &[("PASEO_TIMELINE_UNLOAD_SECS", "1")]);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = cold_agent(&mut ws, &cwd).await;

    for i in 0..3 {
        send(&mut ws, session(json!({ "type": "fetch_agent_timeline_request", "requestId": format!("f{i}"), "agentId": agent_id, "direction": "tail", "limit": 50 }))).await;
        send(&mut ws, session(json!({ "type": "agent.timeline.list_prompts.request", "requestId": format!("l{i}"), "agentId": agent_id }))).await;
    }
    send(&mut ws, session(json!({ "type": "ping", "requestId": "pp", "clientSentAt": 1 }))).await;

    let mut seen: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut pong = false;
    while seen.len() < 6 || !pong {
        let v = recv(&mut ws).await;
        let m = &v["message"];
        let ty = m["type"].as_str().unwrap_or("");
        if ty == "pong" {
            assert_eq!(m["payload"]["requestId"], "pp", "{m}");
            pong = true;
        } else if ty == "fetch_agent_timeline_response" || ty == "agent.timeline.list_prompts.response" {
            let rid = m["payload"]["requestId"].as_str().unwrap().to_string();
            assert!(seen.insert(rid.clone(), m["payload"].clone()).is_none(), "duplicate response for {rid}");
        }
    }
    // A late duplicate would show up as one more response to a request already answered.
    let warm = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "w", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let rows = |p: &Value| -> Vec<(u64, u64, String)> {
        p["entries"].as_array().unwrap().iter().map(|e| (e["seqStart"].as_u64().unwrap(), e["seqEnd"].as_u64().unwrap(), e["item"]["type"].as_str().unwrap().to_string())).collect()
    };
    let expected = rows(&warm);
    assert!(expected.len() >= 2, "{warm}");
    for i in 0..3 {
        let f = &seen[&format!("f{i}")];
        assert!(f["error"].is_null(), "{f}");
        assert_eq!(rows(f), expected, "{f}");
        assert_eq!(f["window"], warm["window"], "{f}");
        let l = &seen[&format!("l{i}")];
        assert!(l["error"].is_null(), "{l}");
        let previews: Vec<&str> = l["prompts"].as_array().unwrap().iter().map(|p| p["preview"].as_str().unwrap()).collect();
        assert_eq!(previews, ["say hi"], "{l}");
    }
    let seqs: Vec<u64> = expected.iter().map(|r| r.0).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
}

/// A turn started after a cold load appends rows with seq continuing from the
/// saved ones: nothing is overwritten and nothing collides, in memory or on disk.
#[tokio::test]
async fn new_turn_after_a_cold_load_continues_the_seq() {
    let d = start_with(unique_dir("coldseq"), None, "ok", &[("PASEO_TIMELINE_UNLOAD_SECS", "1")]);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = cold_agent(&mut ws, &cwd).await;

    let fetch = |rid: &'static str| json!({ "type": "fetch_agent_timeline_request", "requestId": rid, "agentId": agent_id, "direction": "tail", "limit": 50 });
    let before = rpc(&mut ws, fetch("s1"), "fetch_agent_timeline_response").await;
    let old: Vec<(u64, u64)> = before["entries"].as_array().unwrap().iter().map(|e| (e["seqStart"].as_u64().unwrap(), e["seqEnd"].as_u64().unwrap())).collect();
    let next_before = before["window"]["nextSeq"].as_u64().unwrap();
    assert!(!old.is_empty() && old.iter().all(|r| r.1 < next_before), "{before}");

    send(&mut ws, session(json!({ "type": "send_agent_message_request", "requestId": "m1", "agentId": agent_id, "text": "and again" }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;

    let after = rpc(&mut ws, fetch("s2"), "fetch_agent_timeline_response").await;
    let rows: Vec<(u64, u64, String)> = after["entries"].as_array().unwrap().iter().map(|e| (e["seqStart"].as_u64().unwrap(), e["seqEnd"].as_u64().unwrap(), e["item"]["type"].as_str().unwrap().to_string())).collect();
    assert_eq!(&rows[..old.len()].iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), &old, "saved rows changed: {after}");
    let new = &rows[old.len()..];
    assert!(new.iter().any(|r| r.2 == "user_message") && new.iter().any(|r| r.2 == "assistant_message"), "{rows:?}");
    assert!(new.iter().all(|r| r.0 >= next_before), "new rows reuse old seqs: {rows:?}");
    let starts: Vec<u64> = rows.iter().map(|r| r.0).collect();
    assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
    let prompts = rpc(&mut ws, json!({ "type": "agent.timeline.list_prompts.request", "requestId": "s3", "agentId": agent_id }), "agent.timeline.list_prompts.response").await;
    let previews: Vec<&str> = prompts["prompts"].as_array().unwrap().iter().map(|p| p["preview"].as_str().unwrap()).collect();
    assert_eq!(previews, ["say hi", "and again"], "{prompts}");
}

/// Unknown agents fail the same way whether or not the cold path is involved.
#[tokio::test]
async fn timeline_requests_for_an_unknown_agent_report_not_found() {
    let d = start("coldnone", None);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "n1", "agentId": "no-such-agent", "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    assert_eq!(tl["error"], "agent not found", "{tl}");
    assert_eq!(tl["requestId"], "n1");
    assert!(tl["entries"].as_array().unwrap().is_empty(), "{tl}");
    let pr = rpc(&mut ws, json!({ "type": "agent.timeline.list_prompts.request", "requestId": "n2", "agentId": "no-such-agent" }), "agent.timeline.list_prompts.response").await;
    assert_eq!(pr["error"], "agent not found", "{pr}");
    assert!(pr["prompts"].as_array().unwrap().is_empty(), "{pr}");
}

/// The first thing to touch a cold agent is a new prompt, not a fetch: its
/// rows must still continue after the saved ones.
#[tokio::test]
async fn prompt_to_a_cold_agent_then_fetch_keeps_every_row() {
    let d = start_with(unique_dir("coldprompt"), None, "ok", &[("PASEO_TIMELINE_UNLOAD_SECS", "1")]);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = cold_agent(&mut ws, &cwd).await;
    send(&mut ws, session(json!({ "type": "send_agent_message_request", "requestId": "m1", "agentId": agent_id, "text": "second" }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "q1", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let entries = tl["entries"].as_array().unwrap();
    let users: Vec<&str> = entries.iter().filter(|e| e["item"]["type"] == "user_message").map(|e| e["item"]["text"].as_str().unwrap()).collect();
    assert_eq!(users, ["say hi", "second"], "{tl}");
    let starts: Vec<u64> = entries.iter().map(|e| e["seqStart"].as_u64().unwrap()).collect();
    assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
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
    assert_eq!(agents["writer"]["model"], "haiku");
    assert_eq!(agents["engineer"]["model"], "sonnet");
    assert_eq!(agents["qa"]["model"], "sonnet");
    assert!(agents["tester"].get("model").is_none(), "custom role without a model is not forced");
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

#[tokio::test]
async fn worktree_workspace_create_list_and_guarded_archive() {
    let d = start("worktree", None);
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

    let r = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "w1", "source": { "kind": "worktree", "cwd": cwd, "worktreeSlug": "brave-fox" } }), "workspace.create.response").await;
    assert!(r["error"].is_null(), "{r}");
    let workspace = &r["workspace"];
    assert_eq!(workspace["workspaceKind"], "worktree");
    assert_eq!(workspace["worktreeSlug"], "brave-fox");
    assert_eq!(workspace["name"], "brave-fox");
    assert_eq!(workspace["gitRuntime"]["isPaseoOwnedWorktree"], true);
    let dir = std::path::PathBuf::from(workspace["workspaceDirectory"].as_str().unwrap());
    assert!(dir.starts_with(d.dir.join("home").join("worktrees")), "{dir:?}");
    assert_eq!(git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "brave-fox");
    let id = workspace["id"].as_str().unwrap().to_string();

    let main_root = std::fs::canonicalize(&repo).unwrap();
    let r = rpc(&mut ws, json!({ "type": "checkout_status_request", "requestId": "s1", "cwd": dir.to_string_lossy() }), "checkout_status_response").await;
    assert_eq!(r["isPaseoOwnedWorktree"], true, "{r}");
    assert_eq!(std::fs::canonicalize(r["mainRepoRoot"].as_str().unwrap()).unwrap(), main_root, "{r}");
    let r = rpc(&mut ws, json!({ "type": "checkout_status_request", "requestId": "s2", "cwd": cwd }), "checkout_status_response").await;
    assert_eq!(r["isPaseoOwnedWorktree"], false, "{r}");
    assert!(r["mainRepoRoot"].is_null(), "{r}");
    let linked = d.dir.join("linked");
    git(&repo, &["worktree", "add", "-q", "-b", "elsewhere", &linked.to_string_lossy()]);
    let r = rpc(&mut ws, json!({ "type": "checkout_status_request", "requestId": "s3", "cwd": linked.to_string_lossy() }), "checkout_status_response").await;
    assert_eq!(r["isPaseoOwnedWorktree"], false, "{r}");
    assert_eq!(std::fs::canonicalize(r["mainRepoRoot"].as_str().unwrap()).unwrap(), main_root, "{r}");
    git(&repo, &["worktree", "remove", &linked.to_string_lossy()]);

    let r = rpc(&mut ws, json!({ "type": "fetch_workspaces_request", "requestId": "w2" }), "fetch_workspaces_response").await;
    assert!(r["entries"].as_array().unwrap().iter().any(|e| e["id"] == id.as_str()), "{r}");
    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_list_request", "requestId": "w3", "cwd": cwd }), "paseo_worktree_list_response").await;
    assert!(r["error"].is_null(), "{r}");
    assert_eq!(r["worktrees"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["worktrees"][0]["branchName"], "brave-fox");

    let r = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "w4", "source": { "kind": "worktree", "cwd": cwd, "branchName": "bad..name" } }), "workspace.create.response").await;
    assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("Invalid branch name"), "{r}");
    let r = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "w5", "source": { "kind": "worktree", "cwd": cwd, "action": "checkout", "refName": "nope" } }), "workspace.create.response").await;
    assert_eq!(r["errorCode"], "unknown_branch", "{r}");

    // Uncommitted work blocks the archive and nothing changes; once clean the directory goes, the branch stays.
    std::fs::write(dir.join("draft.txt"), "wip\n").unwrap();
    let r = rpc(&mut ws, json!({ "type": "archive_workspace_request", "requestId": "w6", "workspaceId": id }), "archive_workspace_response").await;
    assert!(r["error"].as_str().unwrap().contains("uncommitted"), "{r}");
    assert!(dir.join("draft.txt").exists());
    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_archive_request", "requestId": "w7", "worktreePath": dir.to_string_lossy(), "scope": "worktree" }), "paseo_worktree_archive_response").await;
    assert_eq!(r["success"], false, "{r}");
    assert_eq!(r["error"]["code"], "NOT_ALLOWED");

    std::fs::remove_file(dir.join("draft.txt")).unwrap();
    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_archive_request", "requestId": "w8", "worktreePath": dir.to_string_lossy(), "scope": "worktree" }), "paseo_worktree_archive_response").await;
    assert_eq!(r["success"], true, "{r}");
    assert!(!dir.exists());
    assert!(git(&repo, &["branch", "--list", "brave-fox"]).contains("brave-fox"));
    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_list_request", "requestId": "w9", "cwd": cwd }), "paseo_worktree_list_response").await;
    assert!(r["worktrees"].as_array().unwrap().is_empty(), "{r}");
}

/// Next frame of either kind: JSON text or a binary terminal frame.
async fn recv_frame(ws: &mut Ws) -> Message {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), ws.next()).await.expect("timed out waiting for a frame") {
            Some(Ok(m @ (Message::Text(_) | Message::Binary(_)))) => return m,
            Some(Ok(_)) => continue,
            other => panic!("socket closed: {other:?}"),
        }
    }
}

/// Collect terminal output/restore frames for `slot` until they contain `needle`.
async fn terminal_output_until(ws: &mut Ws, slot: u8, needle: &str) {
    let mut seen = Vec::new();
    loop {
        if let Message::Binary(b) = recv_frame(ws).await {
            if b.len() >= 2 && (b[0] == 0x01 || b[0] == 0x05) && b[1] == slot {
                seen.extend_from_slice(&b[2..]);
            }
        }
        if String::from_utf8_lossy(&seen).contains(needle) {
            return;
        }
    }
}

async fn terminal_payload(ws: &mut Ws, kind: &str) -> Value {
    loop {
        if let Message::Text(t) = recv_frame(ws).await {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["message"]["type"] == kind {
                return v["message"]["payload"].clone();
            }
        }
    }
}

#[tokio::test]
async fn terminal_runs_a_shell_streams_output_and_reattaches() {
    let d = start_with(unique_dir("term"), None, "ok", &[("SHELL", "/bin/sh")]);
    let proj = d.dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let cwd = proj.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(&mut ws, session(json!({ "type": "project.add.request", "requestId": "p1", "cwd": cwd }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "project.add.response").then_some(())).await;

    // Outside every project: refused.
    send(&mut ws, session(json!({ "type": "create_terminal_request", "requestId": "t0", "cwd": "/" }))).await;
    let refused = terminal_payload(&mut ws, "create_terminal_response").await;
    assert!(refused["terminal"].is_null() && refused["error"].is_string(), "{refused}");

    send(&mut ws, session(json!({ "type": "create_terminal_request", "requestId": "t1", "cwd": cwd }))).await;
    let created = terminal_payload(&mut ws, "create_terminal_response").await;
    assert!(created["error"].is_null(), "{created}");
    let id = created["terminal"]["id"].as_str().unwrap().to_string();

    send(&mut ws, session(json!({ "type": "list_terminals_request", "requestId": "l1", "cwd": cwd }))).await;
    let listed = terminal_payload(&mut ws, "list_terminals_response").await;
    assert_eq!(listed["terminals"][0]["id"], id.as_str());

    send(&mut ws, session(json!({ "type": "subscribe_terminal_request", "requestId": "s1", "terminalId": id, "restore": { "mode": "visible-snapshot" } }))).await;
    let sub = terminal_payload(&mut ws, "subscribe_terminal_response").await;
    assert!(sub["error"].is_null(), "{sub}");
    let slot = sub["slot"].as_u64().unwrap() as u8;

    // The command's own echo says `20+1`; only the shell's answer says `21`.
    send(&mut ws, session(json!({ "type": "terminal_input", "terminalId": id, "message": { "type": "input", "data": "echo hi-$((20+1))\n" } }))).await;
    terminal_output_until(&mut ws, slot, "hi-21").await;

    // The terminal outlives the connection: a new one gets the scrollback back.
    drop(ws);
    let mut ws2 = connect(d.port).await;
    hello(&mut ws2, None).await;
    send(&mut ws2, session(json!({ "type": "subscribe_terminal_request", "requestId": "s2", "terminalId": id, "restore": { "mode": "visible-snapshot" } }))).await;
    let sub = terminal_payload(&mut ws2, "subscribe_terminal_response").await;
    let slot = sub["slot"].as_u64().unwrap() as u8;
    terminal_output_until(&mut ws2, slot, "hi-21").await;

    // Binary input frame: opcode 0x02, slot, bytes.
    let mut frame = vec![0x02, slot];
    frame.extend_from_slice(b"exit\n");
    ws2.send(Message::Binary(frame)).await.unwrap();
    let exit = terminal_payload(&mut ws2, "terminal_stream_exit").await;
    assert_eq!(exit["terminalId"], id.as_str());
    assert_eq!(exit["subscriptionId"], sub["subscriptionId"]);

    send(&mut ws2, session(json!({ "type": "list_terminals_request", "requestId": "l2", "cwd": cwd }))).await;
    let listed = terminal_payload(&mut ws2, "list_terminals_response").await;
    assert!(listed["terminals"].as_array().unwrap().is_empty(), "{listed}");
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

#[tokio::test]
async fn entries_duplicate_copies_a_directory_tree() {
    let d = start("dupdir", None);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(cwd.join("src/sub")).unwrap();
    std::fs::write(cwd.join("src/sub/b.txt"), "two").unwrap();
    let cwd_s = cwd.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let dup = rpc(&mut ws, json!({ "type": "fs.entry.duplicate.request", "requestId": "d1", "cwd": cwd_s, "path": "src" }), "fs.entry.duplicate.response").await;
    assert_eq!((dup["success"].as_bool(), dup["duplicatedPath"].as_str()), (Some(true), Some("src copy")), "{dup}");
    assert_eq!(std::fs::read_to_string(cwd.join("src copy/sub/b.txt")).unwrap(), "two");
    let again = rpc(&mut ws, json!({ "type": "fs.entry.duplicate.request", "requestId": "d2", "cwd": cwd_s, "path": "src" }), "fs.entry.duplicate.response").await;
    assert_eq!(again["duplicatedPath"], "src copy 2", "{again}");
    let root = rpc(&mut ws, json!({ "type": "fs.entry.duplicate.request", "requestId": "d3", "cwd": cwd_s, "path": "." }), "fs.entry.duplicate.response").await;
    assert_eq!(root["success"], false, "{root}");
}

#[tokio::test]
async fn worktree_archive_succeeds_when_its_directory_was_deleted_by_hand() {
    let d = start("wtgone", None);
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

    let r = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "w1", "source": { "kind": "worktree", "cwd": cwd, "worktreeSlug": "gone-fox" } }), "workspace.create.response").await;
    assert!(r["error"].is_null(), "{r}");
    let dir = std::path::PathBuf::from(r["workspace"]["workspaceDirectory"].as_str().unwrap());
    let id = r["workspace"]["id"].as_str().unwrap().to_string();
    std::fs::remove_dir_all(&dir).unwrap();

    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_archive_request", "requestId": "w2", "worktreePath": dir.to_string_lossy(), "scope": "worktree" }), "paseo_worktree_archive_response").await;
    assert_eq!(r["success"], true, "{r}");
    assert!(!git(&repo, &["worktree", "list", "--porcelain"]).contains("gone-fox"), "stale worktree entry was not pruned");
    assert!(git(&repo, &["branch", "--list", "gone-fox"]).contains("gone-fox"));
    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_list_request", "requestId": "w3", "cwd": cwd }), "paseo_worktree_list_response").await;
    assert!(r["worktrees"].as_array().unwrap().is_empty(), "{r}");
    let r = rpc(&mut ws, json!({ "type": "fetch_workspaces_request", "requestId": "w4" }), "fetch_workspaces_response").await;
    assert!(!r["entries"].as_array().unwrap().iter().any(|e| e["id"] == id.as_str()), "{r}");
}

#[tokio::test]
async fn worktree_checks_out_a_pull_request_head_from_origin() {
    let d = start("wtpr", None);
    let remote = d.dir.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let repo = d.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    git(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
    git(&repo, &["checkout", "-q", "-b", "scratch"]);
    std::fs::write(repo.join("b.txt"), "from the pr\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "pr work"]);
    git(&repo, &["push", "-q", "origin", "HEAD:refs/pull/1/head"]);
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["branch", "-q", "-D", "scratch"]);
    let cwd = repo.to_string_lossy().to_string();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let request = |id: &str, number: i64, ref_name: Option<&str>| {
        let mut source = json!({ "kind": "worktree", "cwd": cwd, "action": "checkout", "checkoutSource": { "kind": "change_request", "forge": "github", "number": number } });
        if let Some(name) = ref_name {
            source["refName"] = json!(name);
        }
        json!({ "type": "workspace.create.request", "requestId": id, "source": source })
    };
    let branch_of = |r: &Value| git(std::path::Path::new(r["workspace"]["workspaceDirectory"].as_str().unwrap()), &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();

    let r = rpc(&mut ws, request("p1", 1, Some("feature/from-pr")), "workspace.create.response").await;
    assert!(r["error"].is_null(), "{r}");
    assert_eq!(branch_of(&r), "feature/from-pr");
    let dir = std::path::PathBuf::from(r["workspace"]["workspaceDirectory"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "from the pr\n");

    // The head branch name is taken, so the same PR lands on a numbered branch; no refName falls back to pr-<n>.
    let r = rpc(&mut ws, request("p2", 1, Some("feature/from-pr")), "workspace.create.response").await;
    assert_eq!(branch_of(&r), "feature/from-pr-2", "{r}");
    let r = rpc(&mut ws, request("p3", 1, None), "workspace.create.response").await;
    assert_eq!(branch_of(&r), "pr-1", "{r}");

    let r = rpc(&mut ws, request("p4", 99, None), "workspace.create.response").await;
    assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("Cannot fetch pull request 99"), "{r}");
    let r = rpc(&mut ws, request("p5", 0, None), "workspace.create.response").await;
    assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("positive integer"), "{r}");
}

#[tokio::test]
async fn pull_request_checkout_rejects_bad_input_cleanly_and_archive_keeps_the_branch() {
    let d = start("wtpr2", None);
    let remote = d.dir.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let repo = d.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    git(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
    git(&repo, &["checkout", "-q", "-b", "taken"]);
    std::fs::write(repo.join("b.txt"), "local work\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "local work"]);
    git(&repo, &["push", "-q", "origin", "HEAD:refs/pull/1/head"]);
    git(&repo, &["checkout", "-q", "main"]);
    let cwd = repo.to_string_lossy().to_string();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let request = |id: &str, checkout_source: Value, ref_name: Option<&str>| {
        let mut source = json!({ "kind": "worktree", "cwd": cwd, "action": "checkout", "checkoutSource": checkout_source });
        if let Some(name) = ref_name {
            source["refName"] = json!(name);
        }
        json!({ "type": "workspace.create.request", "requestId": id, "source": source })
    };
    let github = |number: Value| json!({ "kind": "change_request", "forge": "github", "number": number });
    let state = || (git(&repo, &["branch", "--format=%(refname:short)"]), git(&repo, &["worktree", "list", "--porcelain"]));
    let before = state();

    let r = rpc(&mut ws, request("f1", github(json!(99)), None), "workspace.create.response").await;
    assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("Cannot fetch pull request 99"), "{r}");
    assert_eq!(state(), before, "a failed fetch left a branch or worktree behind");

    for (id, number) in [("n0", json!(0)), ("n1", json!(-1)), ("n2", json!("1")), ("n3", json!(1.5))] {
        let r = rpc(&mut ws, request(id, github(number.clone()), None), "workspace.create.response").await;
        assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("positive integer"), "{number}: {r}");
        assert_eq!(state(), before, "{number} changed the repo");
    }
    let r = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "n5", "source": { "kind": "worktree", "cwd": cwd, "action": "checkout", "githubPrNumber": 0 } }), "workspace.create.response").await;
    assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("positive integer"), "{r}");

    let gitlab = json!({ "kind": "change_request", "forge": "gitlab", "number": 1 });
    let r = rpc(&mut ws, request("g1", gitlab, None), "workspace.create.response").await;
    assert!(r["workspace"].is_null() && r["error"].as_str().unwrap().contains("GitHub"), "{r}");
    assert_eq!(state(), before, "a non-github forge changed the repo");

    // A ref name git would read as an option, or that is not a valid ref, falls back to pr-<n>.
    for (id, name) in [("o1", "--upload-pack=touch pwned"), ("o2", "-x"), ("o3", "a..b")] {
        let r = rpc(&mut ws, request(id, github(json!(1)), Some(name)), "workspace.create.response").await;
        assert!(r["error"].is_null(), "{name}: {r}");
        let dir = std::path::PathBuf::from(r["workspace"]["workspaceDirectory"].as_str().unwrap());
        let branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
        assert!(branch.starts_with("pr-1"), "{name} gave {branch}");
        assert!(!repo.join("pwned").exists());
    }

    // An unrelated local branch has the name: the PR takes the next free one and `taken` does not move.
    let taken_head = git(&repo, &["rev-parse", "taken"]);
    let main_head = git(&repo, &["rev-parse", "main"]);
    let r = rpc(&mut ws, request("c1", github(json!(1)), Some("taken")), "workspace.create.response").await;
    assert!(r["error"].is_null(), "{r}");
    let dir = std::path::PathBuf::from(r["workspace"]["workspaceDirectory"].as_str().unwrap());
    assert_eq!(git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "taken-2");
    assert_eq!(git(&repo, &["rev-parse", "taken"]), taken_head);
    assert_eq!(git(&repo, &["rev-parse", "taken-2"]), taken_head);
    assert_eq!(git(&repo, &["rev-parse", "main"]), main_head);

    let r = rpc(&mut ws, json!({ "type": "paseo_worktree_archive_request", "requestId": "a1", "worktreePath": dir.to_string_lossy(), "scope": "worktree" }), "paseo_worktree_archive_response").await;
    assert_eq!(r["success"], true, "{r}");
    assert!(!dir.exists(), "archive left the directory");
    assert!(git(&repo, &["branch", "--format=%(refname:short)"]).lines().any(|b| b == "taken-2"), "archive deleted the PR branch");
}

#[tokio::test]
async fn archiving_a_workspace_ends_its_terminals() {
    let d = start_with(unique_dir("termarch"), None, "ok", &[("SHELL", "/bin/sh")]);
    let proj = d.dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let cwd = proj.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let r = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "w1", "source": { "kind": "directory", "path": cwd } }), "workspace.create.response").await;
    assert!(r["error"].is_null(), "{r}");
    let workspace_id = r["workspace"]["id"].as_str().unwrap().to_string();

    send(&mut ws, session(json!({ "type": "create_terminal_request", "requestId": "t1", "cwd": cwd, "workspaceId": workspace_id }))).await;
    let created = terminal_payload(&mut ws, "create_terminal_response").await;
    assert!(created["error"].is_null(), "{created}");
    let id = created["terminal"]["id"].as_str().unwrap().to_string();
    send(&mut ws, session(json!({ "type": "subscribe_terminal_request", "requestId": "s1", "terminalId": id, "restore": { "mode": "visible-snapshot" } }))).await;
    let sub = terminal_payload(&mut ws, "subscribe_terminal_response").await;
    assert!(sub["error"].is_null(), "{sub}");

    send(&mut ws, session(json!({ "type": "archive_workspace_request", "requestId": "a1", "workspaceId": workspace_id }))).await;
    let exit = terminal_payload(&mut ws, "terminal_stream_exit").await;
    assert_eq!(exit["terminalId"], id.as_str());

    send(&mut ws, session(json!({ "type": "list_terminals_request", "requestId": "l1", "workspaceId": workspace_id }))).await;
    let listed = terminal_payload(&mut ws, "list_terminals_response").await;
    assert!(listed["terminals"].as_array().unwrap().is_empty(), "{listed}");

    send(&mut ws, session(json!({ "type": "create_terminal_request", "requestId": "t2", "cwd": cwd, "workspaceId": workspace_id }))).await;
    let refused = terminal_payload(&mut ws, "create_terminal_response").await;
    assert!(refused["terminal"].is_null() && refused["error"].is_string(), "{refused}");
}

#[tokio::test]
async fn schedule_create_rejects_an_interval_under_one_minute() {
    let d = start("schedmin", None);
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    for (request_id, every_ms) in [("s1", 59_999u64), ("s2", 0)] {
        let mut msg = new_agent_schedule(request_id, &cwd);
        msg["cadence"]["everyMs"] = json!(every_ms);
        send(&mut ws, session(msg)).await;
        let err = recv_until(&mut ws, |v| (v["message"]["type"] == "rpc_error").then(|| v["message"]["payload"].clone())).await;
        assert_eq!(err["requestId"], request_id, "{err}");
        assert!(err["error"].as_str().unwrap().contains("everyMs must be between 1 minute"), "{err}");
    }
    let listed = rpc(&mut ws, json!({ "type": "schedule/list", "requestId": "s3" }), "schedule/list/response").await;
    assert!(listed["schedules"].as_array().unwrap().is_empty(), "{listed}");

    let ok = rpc(&mut ws, { let mut m = new_agent_schedule("s4", &cwd); m["cadence"]["everyMs"] = json!(60_000); m }, "schedule/create/response").await;
    assert!(ok["error"].is_null(), "{ok}");
}

/// A stand-in for `codex app-server`: starts a thread, saves every `turn/start`
/// request to `codex-turns` and completes the turn.
fn fake_codex(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-codex.sh");
    let script = format!(
        concat!(
            "#!/bin/sh\nwhile IFS= read -r line; do\n",
            "  id=$(printf '%s' \"$line\" | sed -n 's/^{{\"id\":\\([0-9]*\\),.*/\\1/p')\n",
            "  [ -n \"$id\" ] || continue\n  case \"$line\" in\n",
            "    *'\"method\":\"thread/start\"'*) printf '{{\"id\":%s,\"result\":{{\"thread\":{{\"id\":\"fake-thread\"}}}}}}\\n' \"$id\" ;;\n",
            "    *'\"method\":\"turn/start\"'*)\n      printf '%s\\n' \"$line\" >> '{log}'\n",
            "      printf '{{\"id\":%s,\"result\":{{\"turn\":{{\"id\":\"t1\"}}}}}}\\n' \"$id\"\n",
            "      printf '%s\\n' '{{\"method\":\"turn/completed\",\"params\":{{\"threadId\":\"fake-thread\",\"turn\":{{\"id\":\"t1\",\"status\":\"completed\"}}}}}}' ;;\n",
            "    *) printf '{{\"id\":%s,\"result\":{{}}}}\\n' \"$id\" ;;\n  esac\ndone\n"
        ),
        log = dir.join("codex-turns").display()
    );
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

async fn wait_for_saved_agent(agents_file: &std::path::Path, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(saved) = std::fs::read(agents_file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).map(|a| a[0].clone()).filter(|a| ready(a)) {
            return saved;
        }
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Picking the other provider's model on an idle agent restarts the chat on
/// that tool: session dropped, and the first prompt (only) carries the earlier
/// conversation.
#[tokio::test]
async fn switching_provider_starts_a_fresh_session_with_the_earlier_chat() {
    let dir = unique_dir("switch");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = fake_codex(&dir);
    let d = start_with(dir.clone(), None, "ok", &[("PASEO_CODEX_BIN", codex.to_str().unwrap())]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(&mut ws, session(json!({ "type": "create_agent_request", "requestId": "r1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "first question" }))).await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    let agents_file = dir.join("home").join("agents.json");
    wait_for_saved_agent(&agents_file, "claude session never saved", |a| a["sessionId"].is_string()).await;

    let switched = rpc(&mut ws, json!({ "type": "set_agent_model_request", "requestId": "s1", "agentId": agent_id, "modelId": "codex:gpt-test" }), "set_agent_model_response").await;
    assert_eq!(switched["accepted"], true, "{switched}");
    let saved = wait_for_saved_agent(&agents_file, "switch never saved", |a| a["provider"] == "codex").await;
    assert_eq!(saved["model"], "gpt-test", "{saved}");
    assert!(saved["sessionId"].is_null(), "{saved}");
    assert_eq!(saved["handoffPending"], true, "{saved}");

    let turns = dir.join("codex-turns");
    for (i, text) in ["second question", "third question"].into_iter().enumerate() {
        send(&mut ws, session(json!({ "type": "send_agent_message_request", "requestId": format!("m{i}"), "agentId": agent_id, "text": text }))).await;
        recv_until(&mut ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    }
    let sent: Vec<String> = std::fs::read_to_string(&turns).unwrap().lines().map(String::from).collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    for earlier in ["earlier session", "User: first question", "Assistant: hello from fake claude", "second question"] {
        assert!(sent[0].contains(earlier), "{earlier}: {}", sent[0]);
    }
    assert!(sent[1].contains("third question") && !sent[1].contains("earlier session"), "{}", sent[1]);
    let saved = saved_agent(&agents_file);
    assert_eq!(saved["handoffPending"], false, "{saved}");
}

#[tokio::test]
async fn switching_provider_is_refused_while_the_agent_runs() {
    let d = start_at(unique_dir("switchbusy"), None, "hang");
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(&mut ws, session(json!({ "type": "create_agent_request", "requestId": "r1", "config": { "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }, "initialPrompt": "work" }))).await;
    let agent_id = recv_until(&mut ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    let refused = rpc(&mut ws, json!({ "type": "set_agent_model_request", "requestId": "s1", "agentId": agent_id, "modelId": "codex:gpt-test" }), "set_agent_model_response").await;
    assert_eq!(refused["accepted"], false, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("running"), "{refused}");
    let saved = wait_for_saved_agent(&d.dir.join("home").join("agents.json"), "agent never saved", |_| true).await;
    assert_eq!(saved["provider"], "claude", "{saved}");
}

async fn create_agent_with(ws: &mut Ws, config: Value, prompt: &str) -> String {
    send(ws, session(json!({ "type": "create_agent_request", "requestId": "c0", "config": config, "initialPrompt": prompt }))).await;
    let id = recv_until(ws, |v| {
        let p = &v["message"]["payload"];
        (v["message"]["type"] == "status" && p["status"] == "agent_created").then(|| p["agentId"].as_str().unwrap().to_string())
    })
    .await;
    recv_until(ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
    id
}

async fn set_model(ws: &mut Ws, agent_id: &str, model_id: &str) -> Value {
    rpc(ws, json!({ "type": "set_agent_model_request", "requestId": "sm", "agentId": agent_id, "modelId": model_id }), "set_agent_model_response").await
}

async fn send_and_wait(ws: &mut Ws, agent_id: &str, text: &str) {
    send(ws, session(json!({ "type": "send_agent_message_request", "requestId": "sw", "agentId": agent_id, "text": text }))).await;
    recv_until(ws, |v| (v["message"]["type"] == "agent_stream" && v["message"]["payload"]["event"]["type"] == "turn_completed").then_some(())).await;
}

/// Codex to Claude Code: the new Claude process gets the earlier chat once,
/// and the mode and thinking level map to their Claude equivalents.
#[tokio::test]
async fn switching_from_codex_to_claude_carries_the_chat_mode_and_thinking() {
    let dir = unique_dir("switch-back");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = fake_codex(&dir);
    let d = start_with(dir.clone(), None, "stdin", &[("PASEO_CODEX_BIN", codex.to_str().unwrap())]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = create_agent_with(&mut ws, json!({ "provider": "codex", "cwd": cwd.to_string_lossy(), "modeId": "full-access", "thinkingOptionId": "xhigh" }), "codex question").await;

    let switched = set_model(&mut ws, &agent_id, "claude:claude-test").await;
    assert_eq!(switched["accepted"], true, "{switched}");
    let agents_file = dir.join("home").join("agents.json");
    let saved = wait_for_saved_agent(&agents_file, "switch never saved", |a| a["provider"] == "claude").await;
    assert_eq!(saved["model"], "claude-test", "{saved}");
    assert_eq!(saved["modeId"], "bypassPermissions", "{saved}");
    assert_eq!(saved["thinkingOptionId"], "xhigh", "{saved}");
    assert!(saved["sessionId"].is_null() && saved["handoffPending"] == true, "{saved}");

    for text in ["claude one", "claude two"] {
        send_and_wait(&mut ws, &agent_id, text).await;
    }
    let sent: Vec<String> = std::fs::read_to_string(dir.join("claude-stdin")).unwrap().lines().map(String::from).collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].contains("User: codex question") && sent[0].contains("claude one"), "{}", sent[0]);
    assert!(sent[1].contains("claude two") && !sent[1].contains("earlier session"), "{}", sent[1]);
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "t", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    let notes: Vec<&str> = tl["entries"].as_array().unwrap().iter().filter(|e| e["item"]["type"] == "notification").filter_map(|e| e["item"]["message"].as_str()).collect();
    assert!(notes.contains(&"Switched to Claude Code"), "{notes:?}");
}

/// Bypass maps to full access and `max` to `xhigh` on Codex, and back.
/// Switching back and forth keeps the agent usable.
#[tokio::test]
async fn switching_to_codex_maps_mode_and_thinking() {
    let dir = unique_dir("switch-reset");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = fake_codex(&dir);
    let d = start_with(dir.clone(), None, "ok", &[("PASEO_CODEX_BIN", codex.to_str().unwrap())]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = create_agent_with(&mut ws, json!({ "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions", "thinkingOptionId": "max" }), "hi").await;
    let agents_file = dir.join("home").join("agents.json");
    wait_for_saved_agent(&agents_file, "claude session never saved", |a| a["sessionId"].is_string()).await;
    assert_eq!(set_model(&mut ws, &agent_id, "codex:gpt-test").await["accepted"], true);
    let saved = wait_for_saved_agent(&agents_file, "switch never saved", |a| a["provider"] == "codex").await;
    assert!(saved["modeId"] == "full-access" && saved["thinkingOptionId"] == "xhigh", "{saved}");

    send_and_wait(&mut ws, &agent_id, "on codex").await;
    assert_eq!(set_model(&mut ws, &agent_id, "claude:claude-test").await["accepted"], true);
    let saved = wait_for_saved_agent(&agents_file, "switch back never saved", |a| a["provider"] == "claude").await;
    assert_eq!(saved["model"], "claude-test", "{saved}");
    assert!(saved["modeId"] == "bypassPermissions" && saved["thinkingOptionId"] == "xhigh", "{saved}");
    assert!(saved["sessionId"].is_null() && saved["handoffPending"] == true, "{saved}");
}

/// A thinking level the target model doesn't list is dropped on a switch
/// (Haiku has no effort levels); a model missing from the catalog keeps it
/// (see `switching_to_codex_maps_mode_and_thinking`, which ends on `claude-test`).
#[tokio::test]
async fn switching_to_a_model_without_the_thinking_level_drops_it() {
    let dir = unique_dir("switch-drop-thinking");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = fake_codex(&dir);
    let d = start_with(dir.clone(), None, "ok", &[("PASEO_CODEX_BIN", codex.to_str().unwrap())]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = create_agent_with(&mut ws, json!({ "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions", "thinkingOptionId": "max" }), "hi").await;
    let agents_file = dir.join("home").join("agents.json");
    wait_for_saved_agent(&agents_file, "claude session never saved", |a| a["sessionId"].is_string()).await;
    assert_eq!(set_model(&mut ws, &agent_id, "codex:gpt-test").await["accepted"], true);
    let saved = wait_for_saved_agent(&agents_file, "switch never saved", |a| a["provider"] == "codex").await;
    assert_eq!(saved["thinkingOptionId"], "xhigh", "{saved}");

    send_and_wait(&mut ws, &agent_id, "on codex").await;
    assert_eq!(set_model(&mut ws, &agent_id, "claude:claude-haiku-4-5").await["accepted"], true);
    let saved = wait_for_saved_agent(&agents_file, "switch back never saved", |a| a["provider"] == "claude").await;
    assert_eq!(saved["model"], "claude-haiku-4-5", "{saved}");
    assert!(saved["thinkingOptionId"].is_null(), "{saved}");
    assert_eq!(saved["modeId"], "bypassPermissions", "{saved}");
}

/// A model id with no provider prefix, or with the current provider's prefix,
/// only changes the model: session kept, no handoff, no notice. An unknown
/// prefix is part of the model id.
#[tokio::test]
async fn model_change_without_a_provider_change_keeps_the_session() {
    let d = start_at(unique_dir("switch-same"), None, "ok");
    let cwd = d.dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = create_agent_with(&mut ws, json!({ "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions", "thinkingOptionId": "max" }), "hi").await;
    let agents_file = d.dir.join("home").join("agents.json");
    let before = wait_for_saved_agent(&agents_file, "session never saved", |a| a["sessionId"].is_string()).await;

    for (requested, expected) in [("claude-plain", "claude-plain"), ("claude:claude-prefixed", "claude-prefixed"), ("ollama:llama", "ollama:llama")] {
        assert_eq!(set_model(&mut ws, &agent_id, requested).await["accepted"], true);
        let saved = wait_for_saved_agent(&agents_file, "model never saved", |a| a["model"] == expected).await;
        assert_eq!(saved["provider"], "claude", "{saved}");
        assert_eq!(saved["sessionId"], before["sessionId"], "{saved}");
        assert_eq!(saved["modeId"], "bypassPermissions", "{saved}");
        assert_eq!(saved["thinkingOptionId"], "max", "{saved}");
        assert_ne!(saved["handoffPending"], true, "{saved}");
    }
    let tl = rpc(&mut ws, json!({ "type": "fetch_agent_timeline_request", "requestId": "t", "agentId": agent_id, "direction": "tail", "limit": 50 }), "fetch_agent_timeline_response").await;
    assert!(!tl["entries"].as_array().unwrap().iter().any(|e| e["item"]["type"] == "notification"), "{tl}");
}

/// The handoff flag is on disk: a daemon killed right after the switch still
/// sends the earlier chat with the first prompt after it comes back.
#[tokio::test]
async fn handoff_survives_a_daemon_restart() {
    let dir = unique_dir("switch-restart");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = fake_codex(&dir);
    let daemon_vars = [("PASEO_CODEX_BIN", codex.to_str().unwrap())];
    let mut first = start_with(dir.clone(), None, "ok", &daemon_vars);
    let mut ws = connect(first.port).await;
    hello(&mut ws, None).await;
    let agent_id = create_agent_with(&mut ws, json!({ "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }), "before the restart").await;
    let agents_file = dir.join("home").join("agents.json");
    wait_for_saved_agent(&agents_file, "claude session never saved", |a| a["sessionId"].is_string()).await;
    assert_eq!(set_model(&mut ws, &agent_id, "codex:gpt-test").await["accepted"], true);
    wait_for_saved_agent(&agents_file, "switch never saved", |a| a["provider"] == "codex" && a["handoffPending"] == true).await;
    first.child.kill().unwrap();
    let _ = first.child.wait();
    std::mem::forget(first);

    let second = start_with(dir.clone(), None, "ok", &daemon_vars);
    let mut ws = connect(second.port).await;
    hello(&mut ws, None).await;
    send_and_wait(&mut ws, &agent_id, "after the restart").await;
    let sent = std::fs::read_to_string(dir.join("codex-turns")).unwrap();
    assert_eq!(sent.lines().count(), 1, "{sent}");
    for earlier in ["earlier session", "User: before the restart", "Assistant: hello from fake claude", "after the restart"] {
        assert!(sent.contains(earlier), "{earlier}: {sent}");
    }
    assert_eq!(saved_agent(&agents_file)["handoffPending"], false);
}

#[tokio::test]
async fn switching_an_unknown_agent_is_refused() {
    let d = start("switch-unknown", None);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let r = set_model(&mut ws, "no-such-agent", "codex:gpt-test").await;
    assert_eq!(r["accepted"], false, "{r}");
}

/// A slash command is not wrapped in the handoff (it would stop being a
/// command): the earlier chat waits for the first ordinary message.
#[tokio::test]
async fn slash_command_after_a_switch_keeps_the_handoff_for_the_next_message() {
    let dir = unique_dir("switch-slash");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = fake_codex(&dir);
    let d = start_with(dir.clone(), None, "ok", &[("PASEO_CODEX_BIN", codex.to_str().unwrap())]);
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let agent_id = create_agent_with(&mut ws, json!({ "provider": "claude", "cwd": cwd.to_string_lossy(), "modeId": "bypassPermissions" }), "first question").await;
    assert_eq!(set_model(&mut ws, &agent_id, "codex:gpt-test").await["accepted"], true);
    send_and_wait(&mut ws, &agent_id, "/init").await;
    send_and_wait(&mut ws, &agent_id, "real question").await;
    let sent: Vec<String> = std::fs::read_to_string(dir.join("codex-turns")).unwrap().lines().map(String::from).collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].contains("\"text\":\"/init\"") && !sent[0].contains("earlier session"), "{}", sent[0]);
    assert!(sent[1].contains("User: first question") && sent[1].contains("real question"), "{}", sent[1]);
}

async fn daemon_config(d: &Daemon) -> Value {
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    send(&mut ws, session(json!({ "type": "get_daemon_config_request", "requestId": "b1" }))).await;
    recv_until(&mut ws, |v| (v["message"]["type"] == "get_daemon_config_response").then(|| v["message"]["payload"]["config"].clone())).await
}

#[tokio::test]
async fn daemon_config_reports_backup_off_without_any_secret_fields() {
    let off = [("FEIYI_BACKUP_TURSO_URL", ""), ("FEIYI_BACKUP_TURSO_TOKEN", ""), ("FEIYI_BACKUP_PASSPHRASE", "")];
    let d = start_with(unique_dir("backup-off"), None, "ok", &off);
    let cfg = daemon_config(&d).await;
    assert_eq!(cfg["backupStatus"]["enabled"], false);
    assert!(cfg["backupStatus"]["lastError"].is_null() && cfg["backupStatus"]["lastAttemptAt"].is_null());
    assert_eq!(cfg["backupStatus"]["rowsPushed"], 0);
    assert!(cfg.get("backup").is_none());
    let text = cfg["backupStatus"].to_string();
    for word in ["url", "Url", "oken", "assphrase", "turso"] {
        assert!(!text.contains(word), "{word} in {text}");
    }
}

#[tokio::test]
async fn daemon_config_reports_backup_on_with_the_interval_and_no_secrets() {
    let on = [("FEIYI_BACKUP_TURSO_URL", "libsql://backup.invalid"), ("FEIYI_BACKUP_TURSO_TOKEN", "tok-9f3a"), ("FEIYI_BACKUP_PASSPHRASE", "pw-7c1d"), ("FEIYI_BACKUP_INTERVAL_SECS", "120")];
    let d = start_with(unique_dir("backup-on"), None, "ok", &on);
    let cfg = daemon_config(&d).await;
    assert_eq!(cfg["backupStatus"]["enabled"], true);
    assert_eq!(cfg["backupStatus"]["intervalSecs"], 120);
    let text = cfg.to_string();
    for secret in ["tok-9f3a", "pw-7c1d", "backup.invalid", "libsql"] {
        assert!(!text.contains(secret), "{secret} in the config");
    }
}

#[tokio::test]
async fn repeated_directory_workspace_create_returns_the_same_workspace() {
    let d = start("wsdup", None);
    let dir = d.dir.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let first = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "d1", "source": { "kind": "directory", "path": cwd } }), "workspace.create.response").await;
    assert!(first["error"].is_null(), "{first}");
    let second = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "d2", "source": { "kind": "directory", "path": cwd } }), "workspace.create.response").await;
    assert_eq!(second["workspace"]["id"], first["workspace"]["id"], "{second}");

    let list = rpc(&mut ws, json!({ "type": "fetch_workspaces_request", "requestId": "l1" }), "fetch_workspaces_response").await;
    let n = list["entries"].as_array().map(|e| e.len()).unwrap_or(0);
    assert_eq!(n, 1, "{list}");
}

async fn create_dir_workspace(ws: &mut Ws, request_id: &str, path: &str) -> Value {
    rpc(ws, json!({ "type": "workspace.create.request", "requestId": request_id, "source": { "kind": "directory", "path": path } }), "workspace.create.response").await
}

#[tokio::test]
async fn directory_workspace_create_is_per_directory_and_normalizes_the_path() {
    let d = start("wsdirs", None);
    let a = d.dir.join("a");
    let b = d.dir.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let a1 = create_dir_workspace(&mut ws, "a1", &a.to_string_lossy()).await;
    let b1 = create_dir_workspace(&mut ws, "b1", &b.to_string_lossy()).await;
    assert!(a1["error"].is_null() && b1["error"].is_null(), "{a1} {b1}");
    assert_ne!(a1["workspace"]["id"], b1["workspace"]["id"]);

    // normalize_dir canonicalizes, so these spellings all name the same directory.
    let slash = create_dir_workspace(&mut ws, "a2", &format!("{}/", a.to_string_lossy())).await;
    assert_eq!(slash["workspace"]["id"], a1["workspace"]["id"], "trailing slash: {slash}");
    let dotted = create_dir_workspace(&mut ws, "a3", &format!("{}/../a/.", a.to_string_lossy())).await;
    assert_eq!(dotted["workspace"]["id"], a1["workspace"]["id"], "dot segments: {dotted}");
    let spaced = create_dir_workspace(&mut ws, "a4", &format!("  {}  ", a.to_string_lossy())).await;
    assert_eq!(spaced["workspace"]["id"], a1["workspace"]["id"], "padded: {spaced}");

    let list = rpc(&mut ws, json!({ "type": "fetch_workspaces_request", "requestId": "l1" }), "fetch_workspaces_response").await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 2, "{list}");
}

#[tokio::test]
async fn directory_workspace_create_after_archive_makes_a_new_workspace() {
    let d = start("wsarch", None);
    let dir = d.dir.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let first = create_dir_workspace(&mut ws, "c1", &cwd).await;
    let id = first["workspace"]["id"].clone();
    let r = rpc(&mut ws, json!({ "type": "archive_workspace_request", "requestId": "c2", "workspaceId": id }), "archive_workspace_response").await;
    assert!(r["error"].is_null(), "{r}");

    let again = create_dir_workspace(&mut ws, "c3", &cwd).await;
    assert!(again["error"].is_null(), "{again}");
    assert_ne!(again["workspace"]["id"], id, "an archived workspace must not be handed back: {again}");
    assert!(again["workspace"]["archivedAt"].is_null(), "{again}");
    let repeat = create_dir_workspace(&mut ws, "c4", &cwd).await;
    assert_eq!(repeat["workspace"]["id"], again["workspace"]["id"], "{repeat}");
}

#[tokio::test]
async fn directory_workspace_create_for_a_missing_directory_still_fails() {
    let d = start("wsmissing", None);
    let gone = d.dir.join("nope").to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;
    let r = create_dir_workspace(&mut ws, "m1", &gone).await;
    assert_eq!(r["errorCode"], "directory_not_found", "{r}");
    assert!(r["workspace"].is_null(), "{r}");
}

#[tokio::test]
async fn worktree_creates_stay_distinct_and_are_not_deduplicated() {
    let d = start("wtdup", None);
    let repo = d.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let cwd = repo.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    let root = create_dir_workspace(&mut ws, "r1", &cwd).await;
    let w1 = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "t1", "source": { "kind": "worktree", "cwd": cwd, "worktreeSlug": "one-fox" } }), "workspace.create.response").await;
    let w2 = rpc(&mut ws, json!({ "type": "workspace.create.request", "requestId": "t2", "source": { "kind": "worktree", "cwd": cwd, "worktreeSlug": "two-fox" } }), "workspace.create.response").await;
    assert!(w1["error"].is_null() && w2["error"].is_null(), "{w1} {w2}");
    let ids = [&root["workspace"]["id"], &w1["workspace"]["id"], &w2["workspace"]["id"]];
    assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2], "{ids:?}");
    assert_eq!(w1["workspace"]["workspaceKind"], "worktree");
    assert_eq!(w2["workspace"]["worktreeSlug"], "two-fox");
}

/// Simulates a double tap / reconnect retry: both requests are on the wire before either reply is read.
#[tokio::test]
async fn rapid_double_directory_create_yields_one_workspace() {
    let d = start("wsrapid", None);
    let dir = d.dir.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.to_string_lossy().into_owned();
    let mut ws = connect(d.port).await;
    hello(&mut ws, None).await;

    for rid in ["x1", "x2", "x3"] {
        send(&mut ws, session(json!({ "type": "workspace.create.request", "requestId": rid, "source": { "kind": "directory", "path": cwd } }))).await;
    }
    let mut ids = Vec::new();
    while ids.len() < 3 {
        let r = recv_until(&mut ws, |v| (v["message"]["type"] == "workspace.create.response").then(|| v["message"]["payload"].clone())).await;
        assert!(r["error"].is_null(), "{r}");
        ids.push(r["workspace"]["id"].clone());
    }
    assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");

    // A second socket (the reconnected client) asking again sees the same one.
    let mut ws2 = connect(d.port).await;
    hello(&mut ws2, None).await;
    let again = create_dir_workspace(&mut ws2, "x4", &cwd).await;
    assert_eq!(again["workspace"]["id"], ids[0], "{again}");
    let list = rpc(&mut ws2, json!({ "type": "fetch_workspaces_request", "requestId": "l1" }), "fetch_workspaces_response").await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 1, "{list}");
}
