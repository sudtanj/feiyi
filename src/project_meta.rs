//! Project metadata the app asks for: the project icon, `paseo.json` and the
//! scripts it declares. Read-only: feiyi does not run workspace scripts or
//! services (no room on a small VM), so they are listed as stopped.

use base64::Engine;
use serde_json::{json, Value};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Icons above this are skipped, like upstream.
const MAX_ICON: u64 = 32 * 1024;
const ICON_NAMES: &[&str] = &["favicon.svg", "favicon.png", "favico.svg", "favico.png", "icon.svg", "icon.png", "app-icon.svg", "app-icon.png", "apple-touch-icon.png", "logo.svg", "logo.png"];
const ICON_DIRS: &[&str] = &["", "public", "static", "priv/static", "assets", "images", "img"];

/// ponytail: exact names in the usual folders only; upstream also globs
/// (favicon-*.png), searches monorepo packages, unpacks .ico and checks the
/// image is square. Add those if icons are missed.
pub fn icon(cwd: &str) -> Option<Value> {
    let root = Path::new(cwd);
    for dir in ICON_DIRS {
        for name in ICON_NAMES {
            let path = root.join(dir).join(name);
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if !meta.is_file() || meta.len() > MAX_ICON {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let mime = if name.ends_with(".svg") { "image/svg+xml" } else { "image/png" };
            return Some(json!({ "data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": mime }));
        }
    }
    None
}

/// The `read_project_config_response` payload fields for `repo_root`.
pub fn read_config(repo_root: &str) -> Value {
    if !Path::new(repo_root).is_dir() {
        return json!({ "ok": false, "error": { "code": "project_not_found" } });
    }
    let path = Path::new(repo_root).join("paseo.json");
    let Ok(meta) = std::fs::metadata(&path) else {
        return json!({ "ok": true, "config": null, "revision": null });
    };
    match std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object) {
        Some(config) => json!({ "ok": true, "config": config, "revision": { "mtimeMs": meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000, "size": meta.len() } }),
        None => json!({ "ok": false, "error": { "code": "invalid_project_config" } }),
    }
}

/// Scripts declared in `dir/paseo.json`, all stopped.
pub fn scripts(dir: &str) -> Vec<Value> {
    let config = read_config(dir);
    let Some(entries) = config.pointer("/config/scripts").and_then(Value::as_object) else { return Vec::new() };
    entries
        .iter()
        .map(|(name, e)| {
            let kind = if e.get("type").and_then(Value::as_str) == Some("script") { "script" } else { "service" };
            let port = e.get("port").and_then(Value::as_u64).filter(|p| (1..=65535).contains(p));
            json!({ "scriptName": name, "type": kind, "hostname": "localhost", "port": port, "proxyUrl": null, "lifecycle": "stopped", "health": null, "exitCode": null, "terminalId": null })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_config_and_scripts() {
        let dir = std::env::temp_dir().join(format!("feiyi-meta-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("public")).unwrap();
        let d = dir.to_str().unwrap();
        assert!(icon(d).is_none());
        assert_eq!(read_config(d), json!({ "ok": true, "config": null, "revision": null }));
        assert!(scripts(d).is_empty());

        std::fs::write(dir.join("public/logo.svg"), "<svg/>").unwrap();
        assert_eq!(icon(d).unwrap()["mimeType"], "image/svg+xml");

        std::fs::write(dir.join("paseo.json"), r#"{"scripts":{"web":{"command":"npm run dev","port":3000},"lint":{"type":"script"}}}"#).unwrap();
        let c = read_config(d);
        assert_eq!((c["ok"].as_bool(), c["revision"]["size"].as_u64().is_some()), (Some(true), true));
        let s = scripts(d);
        assert_eq!(s.len(), 2);
        assert!(s.iter().any(|x| x["scriptName"] == "web" && x["port"] == 3000 && x["type"] == "service"));
        assert!(s.iter().any(|x| x["scriptName"] == "lint" && x["type"] == "script" && x["port"].is_null()));

        std::fs::write(dir.join("paseo.json"), "not json").unwrap();
        assert_eq!(read_config(d)["error"]["code"], "invalid_project_config");
        assert_eq!(read_config("/nonexistent/feiyi")["error"]["code"], "project_not_found");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
