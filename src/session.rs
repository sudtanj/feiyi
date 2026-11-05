//! One client connection, independent of transport (direct WS or E2EE relay).
//! The transport adapter feeds plaintext frames in and drains frames out.

use crate::manager::Cmd;
use crate::terminal;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

pub const WS_PROTOCOL_VERSION: i64 = 1;

/// A frame from the client: JSON text, or binary (file-transfer and terminal frames).
pub enum Inbound {
    Text(String),
    Binary(Vec<u8>),
}

/// A frame to the client: JSON text, or a binary terminal frame.
pub enum Outbound {
    Text(Arc<str>),
    Binary(Vec<u8>),
}

/// Whether a client frame stays binary; anything else must be UTF-8 JSON.
pub fn is_binary_frame(bytes: &[u8]) -> bool {
    crate::uploads::is_file_frame(bytes) || terminal::is_frame(bytes)
}

pub struct Ctx {
    pub cmds: mpsc::UnboundedSender<Cmd>,
    pub bcast: broadcast::Sender<Arc<str>>,
    pub server_id: String,
    pub hostname: String,
    pub password: Option<String>,
    /// feiyi home: uploads are stored under `uploads/`.
    pub home: std::path::PathBuf,
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn server_info(ctx: &Ctx) -> Arc<str> {
    Arc::from(
        json!({ "type": "session", "message": { "type": "status", "payload": {
            "status": "server_info",
            "protocolVersion": WS_PROTOCOL_VERSION,
            "serverId": ctx.server_id,
            "hostname": ctx.hostname,
            "version": concat!("feiyi-", env!("CARGO_PKG_VERSION")),
            // The app gates screens on these flags; only list what rpc.rs implements.
            // Add Project needs both projectAdd and stableProjectIdentity.
            "features": {
                "providersSnapshot": true,
                "workspaceMultiplicity": true,
                "workspacePinning": true,
                "projectAdd": true,
                "projectList": true,
                "stableProjectIdentity": true,
                "projectCreateDirectory": true,
                "projectGithubClone": true,
                "workspaceGithubRepositorySearch": true,
                "projectRemove": true,
                "agentThinkingUpdate": true,
                "agentTimelinePromptIndex": true,
                "daemonStatusRpc": true,
                "daemonDiagnostics": true,
                "pushTokenRevocation": true,
                "providerRemoval": true,
                "checkoutRefresh": true,
                "checkoutDiscardChanges": true,
                "commitsList": true,
                "commitBaseClassification": true,
                "usageSources": true,
                "providerUsageList": true,
                "workspaceTerminals": true,
                "terminal-restore-modes": true,
            },
        } } })
        .to_string(),
    )
}

/// Returns when the client goes away or is rejected.
pub async fn run(ctx: Arc<Ctx>, mut inbound: mpsc::Receiver<Inbound>, outbound: mpsc::Sender<Outbound>, via_relay: bool) {
    // Phase 1: wait for hello. Relay sockets are already authenticated by the
    // E2EE key exchange (same policy as the Node daemon).
    loop {
        let Some(frame) = inbound.recv().await else { return };
        let Inbound::Text(text) = frame else { continue };
        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
        match msg.get("type").and_then(|t| t.as_str()) {
            Some("ping") => {
                let _ = outbound.send(Outbound::Text(Arc::from(r#"{"type":"pong"}"#))).await;
            }
            Some("hello") => {
                let version = msg.get("protocolVersion").and_then(|v| v.as_i64()).unwrap_or(0);
                let reject = |reason: &str| Outbound::Text(Arc::from(json!({ "type": "hello.rejected", "reason": reason, "accepts": ["password"] }).to_string()));
                if version != WS_PROTOCOL_VERSION {
                    let _ = outbound.send(reject("incompatible_protocol")).await;
                    return;
                }
                if let (Some(expected), false) = (&ctx.password, via_relay) {
                    match msg.pointer("/auth/password").and_then(|p| p.as_str()) {
                        None => {
                            let _ = outbound.send(reject("password_required")).await;
                            return;
                        }
                        Some(given) if !constant_time_eq(given.as_bytes(), expected.as_bytes()) => {
                            let _ = outbound.send(reject("incorrect_password")).await;
                            return;
                        }
                        _ => {}
                    }
                }
                if outbound.send(Outbound::Text(server_info(&ctx))).await.is_err() {
                    return;
                }
                break;
            }
            _ => {}
        }
    }

    // Phase 2: live session.
    let mut events = ctx.bcast.subscribe();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Arc<str>>();
    let mut uploads = crate::uploads::Uploads::default();
    let mut terminals = terminal::Attached::new();
    let mut terminal_news = terminal::watcher();
    loop {
        tokio::select! {
            frame = inbound.recv() => {
                let text = match frame {
                    None => return,
                    Some(Inbound::Text(text)) => text,
                    Some(Inbound::Binary(bytes)) if terminal::is_frame(&bytes) => {
                        terminals.binary(&bytes).await;
                        continue;
                    }
                    // File-transfer frames of an upload this session started.
                    Some(Inbound::Binary(bytes)) => {
                        if let Some(resp) = uploads.frame(&bytes).await {
                            if outbound.send(Outbound::Text(crate::manager::wrap(resp))).await.is_err() { return; }
                        }
                        continue;
                    }
                };
                let Ok(mut msg) = serde_json::from_str::<Value>(&text) else { continue };
                match msg.get("type").and_then(|t| t.as_str()) {
                    Some("ping") => {
                        if outbound.send(Outbound::Text(Arc::from(r#"{"type":"pong"}"#))).await.is_err() { return; }
                    }
                    Some("hello") => {
                        if outbound.send(Outbound::Text(server_info(&ctx))).await.is_err() { return; }
                    }
                    Some("session") => {
                        let inner = msg.get_mut("message").map(Value::take).unwrap_or(Value::Null);
                        // Uploads are per connection (the bytes follow on this socket).
                        if inner.get("type").and_then(|t| t.as_str()) == Some("file.upload.request") {
                            if let Some(resp) = uploads.begin(&ctx.home, &inner) {
                                if outbound.send(Outbound::Text(crate::manager::wrap(resp))).await.is_err() { return; }
                            }
                            continue;
                        }
                        // Terminals outlive connections; each connection tracks its own subscriptions.
                        if let Some(frames) = terminals.handle(&inner, &ctx.cmds).await {
                            for frame in frames {
                                if outbound.send(frame).await.is_err() { return; }
                            }
                            continue;
                        }
                        if ctx.cmds.send(Cmd::Rpc { msg: inner, out: reply_tx.clone() }).is_err() { return; }
                    }
                    _ => {}
                }
            }
            Some(out) = reply_rx.recv() => {
                crate::egress::count(&out);
                if outbound.send(Outbound::Text(out)).await.is_err() { return; }
            }
            Ok(()) = terminal_news.changed() => {
                for frame in terminals.pump() {
                    if outbound.send(frame).await.is_err() { return; }
                }
            }
            ev = events.recv() => match ev {
                Ok(out) => {
                    crate::egress::count(&out);
                    if outbound.send(Outbound::Text(out)).await.is_err() { return; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // A slow or just-resumed client missed events it cannot know about. End the
                    // session: the app reconnects and re-syncs everything, instead of showing a
                    // stale chat until it is killed.
                    crate::warn!("session lagged, dropped {n} events; closing so the client resyncs");
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}
