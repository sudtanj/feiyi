//! `fs.file.subscribe` / `fs.file.unsubscribe`: the app's open file pane gets an
//! `fs.file.update` whenever the file's version (size, mtime, inode) changes.
//! Upstream uses `fs.watch`; feiyi polls the metadata once a second instead, only
//! while subscribed. Subscriptions belong to one connection and die with it.

use crate::manager::{reply, Out};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;

const POLL: Duration = Duration::from_secs(1);
const MAX_PER_CONNECTION: usize = 32;

#[derive(Default)]
pub struct Watches(HashMap<String, JoinHandle<()>>);

impl Drop for Watches {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

async fn version(cwd: &str, path: &str) -> Value {
    let (c, p) = (cwd.to_string(), path.to_string());
    tokio::task::spawn_blocking(move || crate::files::file_version(&c, &p))
        .await
        .unwrap_or_else(|e| json!({ "status": "error", "cwd": cwd, "path": path, "error": format!("file request failed: {e}") }))
}

fn subscribed(id: &str, initial: Value, rid: &str) -> Value {
    json!({ "type": "fs.file.subscribe.response", "payload": { "subscriptionId": id, "initial": initial, "requestId": rid } })
}

impl Watches {
    /// Handles the two subscription request types. Returns false for any other `kind`.
    pub fn handle(&mut self, kind: &str, msg: &Value, out: &Out) -> bool {
        let s = |k: &str| msg.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let rid = s("requestId");
        match kind {
            "fs.file.subscribe.request" => {
                let (cwd, path) = (s("cwd"), s("path"));
                let id = Some(s("subscriptionId")).filter(|i| !i.is_empty()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                if let Some(old) = self.0.remove(&id) {
                    old.abort();
                }
                if self.0.len() >= MAX_PER_CONNECTION {
                    let error = format!("Too many file subscriptions (max {MAX_PER_CONNECTION})");
                    reply(out, subscribed(&id, json!({ "status": "error", "cwd": cwd, "path": path, "error": error }), &rid));
                    return true;
                }
                let (out, task_id) = (out.clone(), id.clone());
                let task = tokio::spawn(async move {
                    let mut last = version(&cwd, &path).await;
                    reply(&out, subscribed(&task_id, last.clone(), &rid));
                    let mut tick = tokio::time::interval(POLL);
                    loop {
                        tick.tick().await;
                        if out.is_closed() {
                            return;
                        }
                        let now = version(&cwd, &path).await;
                        if now != last {
                            reply(&out, json!({ "type": "fs.file.update", "payload": { "subscriptionId": task_id, "version": now } }));
                            last = now;
                        }
                    }
                });
                self.0.insert(id, task);
            }
            "fs.file.unsubscribe.request" => {
                let id = s("subscriptionId");
                if let Some(task) = self.0.remove(&id) {
                    task.abort();
                }
                reply(out, json!({ "type": "fs.file.unsubscribe.response", "payload": { "subscriptionId": id, "requestId": rid } }));
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    async fn next(rx: &mut mpsc::UnboundedReceiver<Arc<str>>) -> Value {
        let text = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["message"].clone()
    }

    #[tokio::test]
    async fn subscription_reports_changes_until_unsubscribed_and_is_capped() {
        let dir = std::env::temp_dir().join(format!("feiyi-watch-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut watches = Watches::default();
        let sub = |id: &str| json!({ "cwd": dir.to_string_lossy(), "path": "a.txt", "subscriptionId": id, "requestId": "r" });

        assert!(watches.handle("fs.file.subscribe.request", &sub("s1"), &tx));
        let first = next(&mut rx).await;
        assert_eq!(first["type"], "fs.file.subscribe.response");
        assert_eq!(first["payload"]["initial"]["status"], "ready");

        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let update = next(&mut rx).await;
        assert_eq!(update["type"], "fs.file.update");
        assert_eq!(update["payload"]["subscriptionId"], "s1");
        assert_eq!(update["payload"]["version"]["size"], 7);

        assert!(watches.handle("fs.file.unsubscribe.request", &json!({ "subscriptionId": "s1", "requestId": "u" }), &tx));
        assert_eq!(next(&mut rx).await["type"], "fs.file.unsubscribe.response");
        assert!(watches.0.is_empty());

        for i in 0..=MAX_PER_CONNECTION {
            watches.handle("fs.file.subscribe.request", &sub(&format!("c{i}")), &tx);
        }
        assert_eq!(watches.0.len(), MAX_PER_CONNECTION);
        assert!(!watches.handle("fs.entry.create.request", &json!({}), &tx));
        drop(watches);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
