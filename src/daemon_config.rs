//! The app-editable daemon config (`get_daemon_config` / `set_daemon_config`),
//! persisted in `daemon-config.json`. feiyi acts on:
//! - `providers.<id>.additionalModels`: custom models added to that
//!   provider's model list (snapshot, model picker),
//! - `providers.<id>.enabled`: shown as the provider's enabled flag,
//! - `appendSystemPrompt`: added to every agent's instructions,
//! - `mcp.servers`: the user's HTTP MCP servers (stored as `mcpServers`, with their
//!   tokens, which `to_json` never returns), given to new Claude Code and Codex
//!   processes (`mcp.rs`).
//! Everything else is stored and echoed back so the settings screen
//! round-trips, but feiyi's own settings come from environment variables.

use crate::config::Config;
use crate::mcp::{UserServer, MAX_USER_SERVERS};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// The user's MCP servers as stored (`mcpServers`, tokens included). Entries
/// that fail the checks of a patch are skipped, so an edited file or a
/// restored backup cannot put arbitrary names, URLs or tokens into a Codex
/// command line or a request header.
fn stored_servers(stored: &Map<String, Value>) -> Vec<UserServer> {
    let text = |s: &Value, key: &str| s.get(key).and_then(Value::as_str).map(String::from);
    let flag = |s: &Value, key: &str| s.get(key).and_then(Value::as_bool).unwrap_or(true);
    let mut servers: Vec<UserServer> = Vec::new();
    for entry in stored.get("mcpServers").and_then(Value::as_array).into_iter().flatten() {
        let (Some(name), Some(url)) = (text(entry, "name"), text(entry, "url")) else { continue };
        let token = text(entry, "token").filter(|t| !t.is_empty());
        let taken: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        let valid = crate::mcp::check_user_server(&name, &url, &taken).and_then(|()| token.as_deref().map_or(Ok(()), crate::mcp::check_token));
        if let Err(why) = valid {
            crate::warn!("daemon config: skipping a stored MCP server: {why}");
            continue;
        }
        servers.push(UserServer { name, url, token, claude: flag(entry, "claude"), codex: flag(entry, "codex") });
    }
    servers
}

/// Scheme, host and port of a URL: where a bearer token would be sent.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    format!("{}://{}", scheme.to_ascii_lowercase(), rest.split('/').next().unwrap_or("").to_ascii_lowercase())
}

pub struct DaemonConfig {
    path: PathBuf,
    stored: Map<String, Value>,
}

/// Per-provider overrides applied to model lists (cheap to clone into tasks).
#[derive(Clone, Default)]
pub struct ProviderOverrides {
    stored: Map<String, Value>,
}

impl ProviderOverrides {
    fn provider(&self, id: &str) -> Option<&Value> {
        self.stored.get(id)
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.provider(id).and_then(|p| p.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(true)
    }

    /// Built-in models plus the user's `additionalModels`. A custom model with
    /// the same id replaces the built-in one; a custom default wins.
    pub fn merge_models(&self, id: &str, builtin: Value) -> Value {
        let mut models: Vec<Value> = builtin.as_array().cloned().unwrap_or_default();
        let extra: Vec<Value> = self
            .provider(id)
            .and_then(|p| p.get("additionalModels"))
            .and_then(|m| m.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|m| {
                        let model_id = m.get("id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())?;
                        let label = m.get("label").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(model_id);
                        let mut out = json!({ "provider": id, "id": model_id, "label": label });
                        if let Some(d) = m.get("description").and_then(|v| v.as_str()) {
                            out["description"] = json!(d);
                        }
                        if m.get("isDefault").and_then(|v| v.as_bool()).unwrap_or(false) {
                            out["isDefault"] = json!(true);
                        }
                        Some(out)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let custom_default = extra.iter().any(|m| m.get("isDefault").is_some());
        if custom_default {
            for m in models.iter_mut() {
                if let Some(obj) = m.as_object_mut() {
                    obj.remove("isDefault");
                }
            }
        }
        for m in extra {
            match models.iter_mut().find(|b| b["id"] == m["id"]) {
                // Keep built-in extras (thinking options, context window) under the custom label.
                Some(b) => {
                    if let (Some(b), Some(m)) = (b.as_object_mut(), m.as_object()) {
                        for (k, v) in m {
                            b.insert(k.clone(), v.clone());
                        }
                    }
                }
                None => models.push(m),
            }
        }
        // The model last picked for a chat becomes the default for new ones,
        // unless the user set a custom default.
        let last = self.provider(id).and_then(|p| p.get("lastModel")).and_then(|v| v.as_str());
        if let Some(last) = last.filter(|l| !custom_default && models.iter().any(|m| m["id"] == *l)) {
            for m in models.iter_mut().filter_map(|m| m.as_object_mut()) {
                if m.get("id").and_then(|v| v.as_str()) == Some(last) {
                    m.insert("isDefault".into(), json!(true));
                } else {
                    m.remove("isDefault");
                }
            }
        }
        Value::Array(models)
    }
}

impl DaemonConfig {
    pub fn load(home: &std::path::Path) -> DaemonConfig {
        let path = home.join("daemon-config.json");
        let stored = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        // It holds MCP tokens: files written by older versions may be world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        crate::mcp::set_user_servers(stored_servers(&stored));
        DaemonConfig { path, stored }
    }

    /// The new `mcp.servers` list from a patch: tokens kept for a name that
    /// is still listed (when the entry has no `token`), cleared by `""` or null.
    fn servers_from_patch(&self, list: &Value) -> Result<Vec<UserServer>, String> {
        let list = list.as_array().ok_or("mcp.servers must be a list")?;
        if list.len() > MAX_USER_SERVERS {
            return Err(format!("at most {MAX_USER_SERVERS} MCP servers"));
        }
        let old = stored_servers(&self.stored);
        let mut servers: Vec<UserServer> = Vec::new();
        for entry in list {
            let text = |key: &str| entry.get(key).and_then(Value::as_str);
            let (name, url) = (text("name").ok_or("an MCP server needs a name")?, text("url").unwrap_or("").trim());
            let taken: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
            crate::mcp::check_user_server(name, url, &taken)?;
            let token = match entry.get("token") {
                None => match old.iter().find(|s| s.name == name) {
                    Some(old) if old.token.is_some() && origin(&old.url) != origin(url) => return Err("url changed: re-enter or clear the token".into()),
                    Some(old) => old.token.clone(),
                    None => None,
                },
                Some(Value::String(t)) => Some(t.trim().to_string()).filter(|t| !t.is_empty()),
                Some(_) => None,
            };
            if let Some(token) = &token {
                crate::mcp::check_token(token)?;
            }
            let flag = |key: &str| entry.get(key).and_then(Value::as_bool).unwrap_or(true);
            servers.push(UserServer { name: name.to_string(), url: url.to_string(), token, claude: flag("claude"), codex: flag("codex") });
        }
        Ok(servers)
    }

    fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.stored).map_err(|e| format!("failed to encode daemon config: {e}"))?;
        crate::util::write_atomic(&self.path, &bytes, true).map_err(|e| format!("failed to save daemon config: {e}"))
    }

    pub fn overrides(&self) -> ProviderOverrides {
        ProviderOverrides { stored: self.stored.get("providers").and_then(|p| p.as_object()).cloned().unwrap_or_default() }
    }

    /// Remember `model` as the one new `provider` chats start on.
    pub fn remember_model(&mut self, provider: &str, model: &str) {
        let providers = self.stored.entry("providers").or_insert_with(|| json!({}));
        if !providers.is_object() {
            *providers = json!({});
        }
        let entry = providers.as_object_mut().expect("object").entry(provider.to_string()).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        if entry.get("lastModel").and_then(|v| v.as_str()) != Some(model) {
            entry["lastModel"] = json!(model);
            if let Err(e) = self.save() {
                crate::warn!("{e}");
            }
        }
    }

    pub fn roles(&self) -> Option<Map<String, Value>> {
        crate::company::roles(&self.stored, self.path.parent().unwrap_or(std::path::Path::new(".")))
    }

    pub fn append_system_prompt(&self) -> Option<String> {
        self.stored.get("appendSystemPrompt").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
    }

    /// The full `MutableDaemonConfig` the app expects (stored values over defaults).
    pub fn to_json(&self, cfg: &Config) -> Value {
        let mut out = self.stored.clone();
        out.remove("mcpServers");
        let servers: Vec<Value> = stored_servers(&self.stored).iter().map(|s| json!({ "name": s.name, "url": s.url, "tokenSet": s.token.is_some(), "claude": s.claude, "codex": s.codex })).collect();
        out.insert("mcp".into(), json!({ "enabled": false, "injectIntoAgents": false, "servers": servers }));
        out.insert("hostnames".into(), json!(cfg.hostnames));
        out.insert("app".into(), json!({ "baseUrl": cfg.app_base_url }));
        out.insert("relay".into(), json!({ "enabled": cfg.relay_enabled }));
        out.remove("backup");
        out.insert("backupStatus".into(), crate::backup::app_status(&cfg.backup));
        out.entry("providers").or_insert_with(|| json!({}));
        out.entry("appendSystemPrompt").or_insert_with(|| json!(""));
        Value::Object(out)
    }

    /// Apply a `MutableDaemonConfigPatch`. Provider entries merge field by
    /// field; `removeProviders` deletes entries; `mcp.servers` replaces the
    /// MCP server list; other keys replace. An invalid `mcp.servers` rejects
    /// the whole patch, nothing is applied.
    pub fn apply_patch(&mut self, patch: &Value) -> Result<(), String> {
        let Some(patch) = patch.as_object() else { return Ok(()) };
        let servers = match patch.get("mcp").and_then(|m| m.get("servers")) {
            Some(list) => Some(self.servers_from_patch(list)?),
            None => None,
        };
        let before = self.stored.clone();
        if let Some(servers) = &servers {
            let list = servers.iter().map(|s| {
                let mut entry = json!({ "name": s.name, "url": s.url, "claude": s.claude, "codex": s.codex });
                if let Some(token) = &s.token {
                    entry["token"] = json!(token);
                }
                entry
            });
            self.stored.insert("mcpServers".into(), Value::Array(list.collect()));
        }
        for (key, value) in patch {
            match key.as_str() {
                "providers" => {
                    let providers = self.stored.entry("providers").or_insert_with(|| json!({}));
                    if !providers.is_object() {
                        *providers = json!({});
                    }
                    let providers = providers.as_object_mut().expect("object");
                    for (id, fields) in value.as_object().into_iter().flatten() {
                        let entry = providers.entry(id.clone()).or_insert_with(|| json!({}));
                        if !entry.is_object() {
                            *entry = json!({});
                        }
                        let entry = entry.as_object_mut().expect("object");
                        for (k, v) in fields.as_object().into_iter().flatten() {
                            if v.is_null() {
                                entry.remove(k);
                            } else {
                                entry.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
                "removeProviders" => {
                    if let Some(providers) = self.stored.get_mut("providers").and_then(|p| p.as_object_mut()) {
                        for id in value.as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                            providers.remove(id);
                        }
                    }
                }
                // Environment-controlled in feiyi (backup: edited by hand, it holds secrets; backupStatus: read-only); not stored.
                // (mcpServers: only through the validated mcp.servers.)
                "mcp" | "mcpServers" | "hostnames" | "app" | "relay" | "backup" | "backupStatus" => {}
                _ => {
                    self.stored.insert(key.clone(), value.clone());
                }
            }
        }
        if let Err(e) = self.save() {
            self.stored = before;
            return Err(e);
        }
        if let Some(servers) = servers {
            crate::mcp::set_user_servers(servers);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DaemonConfig {
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        DaemonConfig::load(&dir)
    }

    #[test]
    fn last_picked_model_becomes_default() {
        let mut c = config();
        let builtin = json!([{ "id": "sonnet", "isDefault": true }, { "id": "opus" }]);
        c.remember_model("claude", "opus");
        let models = c.overrides().merge_models("claude", builtin.clone());
        assert_eq!(models[1]["isDefault"], true);
        assert!(models[0].get("isDefault").is_none());
        assert_eq!(DaemonConfig::load(c.path.parent().unwrap()).overrides().merge_models("claude", builtin.clone())[1]["isDefault"], true, "saved");
        c.remember_model("claude", "gone");
        assert_eq!(c.overrides().merge_models("claude", builtin)[0]["isDefault"], true, "unknown model keeps the built-in default");
    }

    #[test]
    fn additional_models_are_merged() {
        let mut c = config();
        c.apply_patch(&json!({ "providers": { "claude": { "additionalModels": [{ "id": "my-model", "label": "My Model" }] } } })).unwrap();
        let merged = c.overrides().merge_models("claude", json!([{ "provider": "claude", "id": "opus", "label": "Opus" }]));
        let ids: Vec<&str> = merged.as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["opus", "my-model"]);
    }

    #[test]
    fn custom_default_wins_and_same_id_replaces_label() {
        let mut c = config();
        c.apply_patch(&json!({ "providers": { "codex": { "additionalModels": [
            { "id": "gpt", "label": "Renamed", "isDefault": true } ] } } })).unwrap();
        let merged = c.overrides().merge_models("codex", json!([
            { "provider": "codex", "id": "gpt", "label": "GPT", "isDefault": true, "thinkingOptions": [] },
            { "provider": "codex", "id": "other", "label": "Other", "isDefault": true } ]));
        let arr = merged.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["label"], "Renamed");
        assert!(arr[0].get("thinkingOptions").is_some(), "built-in extras are kept");
        assert!(arr[1].get("isDefault").is_none(), "only the custom default remains");
    }

    #[test]
    fn provider_patches_merge_and_remove() {
        let mut c = config();
        c.apply_patch(&json!({ "providers": { "claude": { "enabled": false } } })).unwrap();
        assert!(!c.overrides().enabled("claude"));
        assert!(c.overrides().enabled("codex"));
        c.apply_patch(&json!({ "providers": { "claude": { "enabled": null } } })).unwrap();
        assert!(c.overrides().enabled("claude"));
        c.apply_patch(&json!({ "providers": { "claude": { "enabled": false } } })).unwrap();
        c.apply_patch(&json!({ "removeProviders": ["claude"] })).unwrap();
        assert!(c.overrides().enabled("claude"));
    }

    #[test]
    fn append_system_prompt_round_trip_and_persistence() {
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = DaemonConfig::load(&dir);
        assert!(c.append_system_prompt().is_none());
        c.apply_patch(&json!({ "appendSystemPrompt": "  be brief  " })).unwrap();
        assert_eq!(c.append_system_prompt().as_deref(), Some("be brief"));
        let reloaded = DaemonConfig::load(&dir);
        assert_eq!(reloaded.append_system_prompt().as_deref(), Some("be brief"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mcp_patch(servers: Value) -> Value {
        json!({ "mcp": { "servers": servers } })
    }

    fn stored_token(c: &DaemonConfig, name: &str) -> Option<String> {
        stored_servers(&c.stored).into_iter().find(|s| s.name == name).and_then(|s| s.token)
    }

    #[test]
    fn mcp_servers_keep_clear_and_replace_tokens() {
        let mut c = config();
        c.apply_patch(&mcp_patch(json!([
            { "name": "docs", "url": "https://docs.example/mcp", "claude": true, "codex": false, "token": "tok-1" },
            { "name": "other", "url": "http://pi/mcp", "claude": false, "codex": true, "token": "tok-o" } ]))).unwrap();
        assert_eq!(stored_token(&c, "docs").as_deref(), Some("tok-1"));
        c.apply_patch(&mcp_patch(json!([
            { "name": "docs", "url": "https://docs.example/v2", "claude": true, "codex": true },
            { "name": "other", "url": "http://pi/mcp", "claude": false, "codex": true, "token": "" } ]))).unwrap();
        assert_eq!(stored_token(&c, "docs").as_deref(), Some("tok-1"), "absent token keeps it");
        assert_eq!(stored_token(&c, "other"), None, "empty token clears it");
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/v2", "token": "tok-2" }]))).unwrap();
        assert_eq!(stored_token(&c, "docs").as_deref(), Some("tok-2"));
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/v2", "token": null }]))).unwrap();
        assert_eq!(stored_token(&c, "docs"), None, "null clears it");
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/v2", "token": "tok-3" }]))).unwrap();
        c.apply_patch(&mcp_patch(json!([]))).unwrap();
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/v2" }]))).unwrap();
        assert_eq!(stored_token(&c, "docs"), None, "a removed server takes its token along");
        let reloaded = DaemonConfig::load(c.path.parent().unwrap());
        assert_eq!(stored_servers(&reloaded.stored).len(), 1, "saved");
    }

    #[test]
    fn a_changed_url_does_not_inherit_the_token() {
        let mut c = config();
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/mcp", "token": "tok-1" }]))).unwrap();
        for moved in ["https://evil.example/mcp", "http://docs.example/mcp", "https://docs.example:8443/mcp"] {
            let err = c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": moved }]))).unwrap_err();
            assert_eq!(err, "url changed: re-enter or clear the token");
        }
        assert_eq!(stored_servers(&c.stored)[0].url, "https://docs.example/mcp", "nothing applied");
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://DOCS.example/other-path" }]))).unwrap();
        assert_eq!(stored_token(&c, "docs").as_deref(), Some("tok-1"), "same origin keeps it");
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://evil.example/mcp", "token": "" }]))).unwrap();
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://elsewhere.example/mcp" }]))).unwrap();
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://evil.example/mcp", "token": "tok-new" }]))).unwrap();
        assert_eq!(stored_token(&c, "docs").as_deref(), Some("tok-new"), "re-entered");
    }

    #[test]
    fn tokens_with_control_characters_are_rejected() {
        let mut c = config();
        for token in ["a\nb", "a\u{0}b", "a\rb"] {
            assert!(c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/mcp", "token": token }]))).is_err());
        }
        assert!(stored_servers(&c.stored).is_empty());
    }

    #[test]
    fn invalid_stored_servers_are_skipped_on_load() {
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stored = json!({ "mcpServers": [
            { "name": "good", "url": "https://good.example/mcp", "token": "tok-1" },
            { "name": "good", "url": "https://dup.example/mcp" },
            { "name": "x\" -c shell=\"rm", "url": "https://h/mcp" },
            { "name": "zilong-router", "url": "https://h/mcp" },
            { "name": "bad-url", "url": "https://u:p@h/mcp" },
            { "name": "bad-token", "url": "https://h/mcp", "token": "a\nb" },
            { "name": "no-url" } ] });
        std::fs::write(dir.join("daemon-config.json"), stored.to_string()).unwrap();
        let c = DaemonConfig::load(&dir);
        let names: Vec<String> = stored_servers(&c.stored).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["good"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_fails_the_patch_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(dir.join("daemon-config.json")).unwrap();
        let mut c = DaemonConfig::load(&dir);
        assert!(c.apply_patch(&json!({ "appendSystemPrompt": "x", "mcp": { "servers": [{ "name": "docs", "url": "https://docs.example/mcp" }] } })).is_err());
        assert!(c.append_system_prompt().is_none() && stored_servers(&c.stored).is_empty(), "rolled back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_servers_are_reported_without_tokens() {
        let mut c = config();
        c.apply_patch(&mcp_patch(json!([
            { "name": "docs", "url": "https://docs.example/mcp", "claude": true, "codex": false, "token": "tok-secret" },
            { "name": "open", "url": "https://open.example/mcp", "claude": false, "codex": true } ]))).unwrap();
        c.apply_patch(&json!({ "mcpServers": [{ "name": "forged", "url": "http://x", "token": "tok-forged" }] })).unwrap();
        let out = c.to_json(&crate::config::Config::load(&[]));
        assert_eq!(out["mcp"]["servers"], json!([
            { "name": "docs", "url": "https://docs.example/mcp", "tokenSet": true, "claude": true, "codex": false },
            { "name": "open", "url": "https://open.example/mcp", "tokenSet": false, "claude": false, "codex": true } ]));
        assert!(out.get("mcpServers").is_none());
        assert!(!out.to_string().contains("tok-"), "{out}");
    }

    #[test]
    fn invalid_mcp_servers_reject_the_whole_patch() {
        let mut c = config();
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/mcp", "token": "tok-1" }]))).unwrap();
        let ok = json!({ "name": "fine", "url": "https://fine.example/mcp" });
        let too_many: Vec<Value> = (0..33).map(|i| json!({ "name": format!("s{i}"), "url": "https://h/mcp" })).collect();
        for bad in [
            json!({ "name": "has space", "url": "https://h/mcp" }),
            json!({ "name": "", "url": "https://h/mcp" }),
            json!({ "name": "zilong-router", "url": "https://h/mcp" }),
            json!({ "name": "feiyi", "url": "https://h/mcp" }),
            json!({ "name": "bad-url", "url": "ftp://h/mcp" }),
            json!({ "name": "no-url" }),
        ] {
            assert!(c.apply_patch(&json!({ "appendSystemPrompt": "x", "mcp": { "servers": [ok.clone(), bad.clone()] } })).is_err(), "{bad}");
        }
        assert!(c.apply_patch(&mcp_patch(json!([ok.clone(), ok.clone()]))).is_err(), "duplicate name");
        assert!(c.apply_patch(&mcp_patch(Value::Array(too_many))).is_err());
        assert!(c.apply_patch(&mcp_patch(json!("nope"))).is_err());
        assert!(c.append_system_prompt().is_none(), "nothing applied");
        assert_eq!(stored_token(&c, "docs").as_deref(), Some("tok-1"), "the old list is untouched");
    }

    #[test]
    #[cfg(unix)]
    fn config_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("daemon-config.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut c = DaemonConfig::load(&dir);
        let mode = || std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(), 0o600, "existing file fixed on load");
        c.apply_patch(&mcp_patch(json!([{ "name": "docs", "url": "https://docs.example/mcp", "token": "tok-1" }]))).unwrap();
        assert_eq!(mode(), 0o600, "written private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn environment_controlled_keys_are_not_stored() {
        let mut c = config();
        c.apply_patch(&json!({ "relay": { "enabled": false }, "hostnames": ["x"] })).unwrap();
        assert!(c.stored.get("relay").is_none() && c.stored.get("hostnames").is_none());
    }

    #[test]
    fn the_app_cannot_write_the_backup_section_but_a_hand_edited_one_survives_saves() {
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("daemon-config.json"), r#"{"backup":{"tursoToken":"tok-keep"}}"#).unwrap();
        let mut c = DaemonConfig::load(&dir);
        c.apply_patch(&json!({ "backup": { "tursoToken": "tok-app" }, "appendSystemPrompt": "x" })).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(dir.join("daemon-config.json")).unwrap()).unwrap();
        assert_eq!(saved["backup"]["tursoToken"], "tok-keep");
        assert_eq!(saved["appendSystemPrompt"], "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_status_is_read_only_and_to_json_holds_no_backup_secrets() {
        let dir = std::env::temp_dir().join(format!("feiyi-cfg-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("daemon-config.json"), r#"{"backup":{"tursoUrl":"libsql://db-org.turso.io","tursoToken":"tok-9f3a","passphrase":"pw-7c1d"}}"#).unwrap();
        let mut c = DaemonConfig::load(&dir);
        c.apply_patch(&json!({ "backupStatus": { "enabled": true, "lastError": "forged" }, "appendSystemPrompt": "x" })).unwrap();
        assert!(c.stored.get("backupStatus").is_none());
        let saved = std::fs::read_to_string(dir.join("daemon-config.json")).unwrap();
        assert!(!saved.contains("backupStatus") && !saved.contains("forged"), "never persisted: {saved}");
        let out = c.to_json(&crate::config::Config::load(&[]));
        assert!(out["backupStatus"].get("enabled").is_some() && out["backupStatus"].get("rowsPushed").is_some());
        assert!(out.get("backup").is_none());
        let text = out.to_string();
        for secret in ["tok-9f3a", "pw-7c1d", "db-org", "libsql"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
