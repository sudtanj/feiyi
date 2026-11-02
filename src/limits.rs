//! Subscription limit windows (5-hour, weekly) as the CLIs report them:
//! Claude Code's `rate_limit_event`, Codex's `account/rateLimits/updated`.
//! Shown on the usage cards. Hidden when the CLI talks to a custom endpoint
//! or an API key, where there is no subscription to run out of.
//! ponytail: only refreshed when a turn runs; polling while idle needs an
//! official usage API.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Window {
    used_pct: Option<f64>,
    /// Unix seconds.
    resets_at: Option<u64>,
    /// Claude's allowed / allowed_warning / rejected.
    status: Option<String>,
    /// Unix seconds the CLI reported it.
    seen: u64,
}

/// "provider/window" -> latest report.
static WINDOWS: Mutex<BTreeMap<String, Window>> = Mutex::new(BTreeMap::new());
static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn load(home: &Path) {
    let path = PATH.get_or_init(|| home.join("limits.json"));
    if let Some(saved) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        *WINDOWS.lock().unwrap() = saved;
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn record(provider: &str, entries: Vec<(String, Window)>) {
    let mut windows = WINDOWS.lock().unwrap();
    for (id, w) in entries {
        let key = format!("{provider}/{id}");
        // Claude names its usage share only now and then: keep the last one for the same window.
        let used = w.used_pct.or_else(|| windows.get(&key).filter(|o| o.resets_at == w.resets_at).and_then(|o| o.used_pct));
        windows.insert(key, Window { used_pct: used, ..w });
    }
    if let Some(path) = PATH.get() {
        let bytes = serde_json::to_vec(&*windows).unwrap_or_default();
        if let Err(e) = crate::util::write_atomic(path, &bytes, false) {
            crate::warn!("could not save limits: {e}");
        }
    }
}

/// A Claude Code `rate_limit_info`.
pub fn claude(info: &Value) {
    let Some(kind) = info.get("rateLimitType").and_then(|v| v.as_str()) else { return };
    // `utilization` is a 0..1 share; accept a percent too.
    let used = info.get("utilization").and_then(|v| v.as_f64()).map(|u| if u <= 1.0 { u * 100.0 } else { u });
    let w = Window {
        used_pct: used,
        resets_at: info.get("resetsAt").and_then(|v| v.as_u64()),
        status: info.get("status").and_then(|v| v.as_str()).map(String::from),
        seen: now(),
    };
    record("claude", vec![(kind.to_string(), w)]);
}

/// A Codex `rateLimits` snapshot: `primary` (5 h) and `secondary` (weekly).
pub fn codex(limits: &Value) {
    let entries = ["primary", "secondary"]
        .iter()
        .filter_map(|k| limits.get(*k).filter(|w| w.is_object()).map(|w| (k, w)))
        .map(|(k, w)| {
            let mins = w.get("windowDurationMins").and_then(|v| v.as_u64());
            let id = match mins {
                Some(300) => "five_hour".to_string(),
                Some(10080) => "seven_day".to_string(),
                Some(m) => format!("{m}m"),
                None => k.to_string(),
            };
            let used = w.get("usedPercent").and_then(|v| v.as_f64());
            (id, Window { used_pct: used, resets_at: w.get("resetsAt").and_then(|v| v.as_u64()), status: None, seen: now() })
        })
        .collect();
    record("codex", entries);
}

fn label(id: &str) -> (String, String) {
    match id {
        "five_hour" => ("5-hour limit".into(), "5h".into()),
        "seven_day" => ("Weekly limit".into(), "wk".into()),
        "seven_day_opus" => ("Weekly limit, Opus".into(), "opus".into()),
        "seven_day_sonnet" => ("Weekly limit, Sonnet".into(), "son".into()),
        "overage" => ("Extra usage".into(), "extra".into()),
        other => match other.strip_suffix('m').and_then(|m| m.parse::<u64>().ok()) {
            Some(m) if m % 1440 == 0 => (format!("{}-day limit", m / 1440), format!("{}d", m / 1440)),
            Some(m) if m % 60 == 0 => (format!("{}-hour limit", m / 60), format!("{}h", m / 60)),
            _ => (other.replace('_', " "), other.chars().take(4).collect()),
        },
    }
}

fn iso(secs: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(secs)).to_string()
}

/// Usage-card windows for `provider`, plus an "as of" detail. Empty when
/// it runs on a custom endpoint or nothing was reported yet.
pub fn windows(provider: &str) -> (Vec<Value>, Option<Value>) {
    if custom_endpoint(provider) {
        return (Vec::new(), None);
    }
    let now = now();
    let prefix = format!("{provider}/");
    let windows = WINDOWS.lock().unwrap();
    let mut seen = 0;
    let out = windows
        .iter()
        .filter_map(|(k, w)| k.strip_prefix(&prefix).map(|id| (id, w)))
        .map(|(id, w)| {
            seen = seen.max(w.seen);
            let (label, short) = label(id);
            // A window past its reset starts over; its old share no longer holds.
            let reset = w.resets_at.is_some_and(|r| r <= now);
            let used = if reset { Some(0.0) } else { w.used_pct.map(|u| (u * 10.0).round() / 10.0) };
            let tone = match (reset, w.status.as_deref(), used) {
                (true, ..) => "ok",
                (_, Some("rejected"), _) => "danger",
                (_, _, Some(u)) if u >= 100.0 => "danger",
                (_, Some("allowed_warning"), _) => "warning",
                (_, _, Some(u)) if u >= 80.0 => "warning",
                _ => "ok",
            };
            json!({
                "id": format!("limit-{id}"),
                "label": label,
                "shortLabel": short,
                "summary": id == "five_hour" || id == "seven_day",
                "usedPct": used,
                "resetsAt": if reset { None } else { w.resets_at.map(iso) },
                "tone": tone,
            })
        })
        .collect::<Vec<_>>();
    let detail = (!out.is_empty()).then(|| json!({ "id": "limits-as-of", "label": "Limits as of", "value": iso(seen) }));
    (out, detail)
}

/// Does `provider` talk to something other than its subscription (a custom
/// base URL, an API key, a cloud provider)? Read on each report so a change
/// shows without a restart.
fn custom_endpoint(provider: &str) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    match provider {
        "claude" => {
            const KEYS: [&str; 5] = ["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX"];
            let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
            let settings: Value = std::fs::read(dir.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let set = |k: &str| std::env::var(k).ok().or_else(|| settings.pointer(&format!("/env/{k}")).and_then(|v| v.as_str()).map(String::from)).filter(|v| !v.is_empty());
            // A subscription token (`claude setup-token`, sk-ant-oat…) passed as a key is still the subscription.
            KEYS.iter().filter_map(|k| set(k)).any(|v| !v.starts_with("sk-ant-oat")) || settings.get("apiKeyHelper").is_some()
        }
        "codex" => {
            let dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
            let config = std::fs::read_to_string(dir.join("config.toml")).unwrap_or_default();
            ["OPENAI_BASE_URL", "CODEX_BASE_URL"].iter().any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty())) || codex_custom(&config)
        }
        _ => true,
    }
}

/// A top-level `model_provider` other than OpenAI, or an `openai_base_url`.
/// ponytail: profiles are not followed.
fn codex_custom(config: &str) -> bool {
    config
        .lines()
        .map(str::trim)
        .take_while(|l| !l.starts_with('['))
        .filter_map(|l| l.split_once('='))
        .any(|(k, v)| {
            let v = v.trim().trim_matches('"');
            (k.trim() == "model_provider" && v != "openai") || k.trim() == "openai_base_url"
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_config_custom_provider() {
        assert!(codex_custom("model = \"x\"\nmodel_provider = \"ollama\"\n"));
        assert!(!codex_custom("model_provider = \"openai\"\n[model_providers.ollama]\nbase_url = \"http://x\"\n"));
        assert!(!codex_custom("[profiles.a]\nmodel_provider = \"ollama\"\n"));
        assert!(codex_custom("openai_base_url = \"http://proxy\"\n"));
    }

    #[test]
    fn windows_from_both_clis() {
        let later = now() + 3600;
        claude(&json!({ "status": "allowed_warning", "rateLimitType": "five_hour", "resetsAt": later, "utilization": 0.85 }));
        claude(&json!({ "status": "allowed_warning", "rateLimitType": "five_hour", "resetsAt": later }));
        codex(&json!({ "primary": { "usedPercent": 40.0, "windowDurationMins": 300, "resetsAt": later }, "secondary": { "usedPercent": 100.0, "windowDurationMins": 10080, "resetsAt": 1 } }));
        let w = WINDOWS.lock().unwrap().clone();
        assert_eq!(w["claude/five_hour"].used_pct, Some(85.0), "share kept across events of the same window");
        assert_eq!(w["codex/five_hour"].used_pct, Some(40.0));
        assert_eq!(w["codex/seven_day"].resets_at, Some(1));
        assert_eq!(label("10080m").1, "7d");
    }
}
