//! MCP servers given to every Claude Code and Codex agent (sub-agents inherit
//! them), next to the user's own (`~/.claude.json`, `.mcp.json`,
//! `~/.codex/config.toml`), as Streamable HTTP servers:
//!
//! - `zilong-router`: the external gateway (`PASEO_ZILONG_MCP_URL`), when set.
//!   Its API key (`PASEO_ZILONG_API_KEY`) reaches Codex through the
//!   environment it inherits (`bearer_token_env_var`).
//! - `feiyi`: the daemon's own debug server (`/mcp`, mcp_server.rs), on
//!   unless `PASEO_DEBUG_MCP=0`. Its bearer token is random per daemon start
//!   and reaches Codex as `PASEO_LITE_MCP_TOKEN` on the app-server's
//!   environment.
//!
//! Secrets never go on a command line: Claude reads them from a private
//! (0600) `--mcp-config` file, Codex from its environment.

use crate::config::Config;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::RwLock;

pub const NAME: &str = "zilong-router";
pub const KEY_ENV: &str = "PASEO_ZILONG_API_KEY";
pub const DEBUG_NAME: &str = "feiyi";
pub const DEBUG_TOKEN_ENV: &str = "PASEO_LITE_MCP_TOKEN";

/// Codex shell commands (and their snapshots) must not inherit the daemon's secrets; GH_TOKEN stays for git/gh.
const CODEX_SHELL_EXCLUDE: &str = r#"["PASEO_*", "CLAUDE_CODE_*", "ANTHROPIC_*", "*API_KEY*", "*SECRET*", "*PASSWORD*"]"#;

/// The daemon's own MCP server: where agents reach it and the token they need.
#[derive(Clone)]
pub struct DebugMcp {
    pub url: String,
    pub token: String,
    /// The address agents connect to (loopback unless the daemon listens on
    /// one specific address); requests from other peers are refused.
    pub host: IpAddr,
}

/// Never print the token (Config derives Debug).
impl std::fmt::Debug for DebugMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugMcp").field("url", &self.url).field("token", &"<redacted>").field("host", &self.host).finish()
    }
}

impl DebugMcp {
    /// The server for a daemon listening on `listen` (`PASEO_LISTEN`), with a
    /// fresh token; None (with a warning) when no local URL can be derived.
    pub fn for_listen(listen: &str) -> Option<DebugMcp> {
        match local_endpoint(listen) {
            Ok((host, port)) => Some(DebugMcp { url: format!("http://{}/mcp", SocketAddr::new(host, port)), token: new_token(), host }),
            Err(why) => {
                crate::warn!("mcp: {DEBUG_NAME} debug server disabled: {why}");
                None
            }
        }
    }

    /// Requests only from this machine.
    pub fn peer_allowed(&self, peer: IpAddr) -> bool {
        let peer = match peer {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(peer),
            v4 => v4,
        };
        peer.is_loopback() || peer == self.host
    }

    /// `Authorization: Bearer <token>`, compared in constant time.
    pub fn authorized(&self, authorization: Option<&str>) -> bool {
        let Some(value) = authorization.map(str::trim) else { return false };
        let Some((scheme, token)) = value.split_once(' ') else { return false };
        scheme.eq_ignore_ascii_case("bearer") && crate::session::constant_time_eq(token.trim().as_bytes(), self.token.as_bytes())
    }
}

/// 64 random hex characters (two v4 UUIDs: 244 random bits).
fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// The address and port agents on this machine reach the daemon at:
/// 127.0.0.1 for a wildcard or loopback listen address (or `localhost`),
/// the address itself when the daemon listens on one specific address.
fn local_endpoint(listen: &str) -> Result<(IpAddr, u16), String> {
    let listen = listen.trim();
    if listen.starts_with('/') || listen.starts_with("unix:") {
        return Err(format!("listen address {listen} is a unix socket"));
    }
    let (ip, port) = match listen.parse::<SocketAddr>() {
        Ok(addr) => (Some(addr.ip()), addr.port()),
        Err(_) => {
            let (host, port) = listen.rsplit_once(':').ok_or_else(|| format!("listen address {listen} has no port"))?;
            let port = port.parse::<u16>().map_err(|_| format!("listen address {listen} has no valid port"))?;
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return Err(format!("cannot derive a local address from listen address {listen}"));
            }
            (None, port)
        }
    };
    if port == 0 {
        return Err(format!("listen address {listen} has no fixed port"));
    }
    let host = match ip {
        Some(ip) if ip.is_loopback() => ip,
        Some(ip) if !ip.is_unspecified() => ip,
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    Ok((host, port))
}

fn claude_path(cfg: &Config) -> PathBuf {
    cfg.home.join("claude-mcp.json")
}

/// An HTTP MCP server the user added in the app (`mcp.servers` of the daemon
/// config). No `Debug`: it holds a token.
#[derive(Clone)]
pub struct UserServer {
    pub name: String,
    pub url: String,
    pub token: Option<String>,
    pub claude: bool,
    pub codex: bool,
}

pub const MAX_USER_SERVERS: usize = 32;

/// What new agent processes get; set when the daemon config loads or changes.
static USER_SERVERS: RwLock<Vec<UserServer>> = RwLock::new(Vec::new());

pub fn set_user_servers(servers: Vec<UserServer>) {
    *USER_SERVERS.write().unwrap_or_else(|e| e.into_inner()) = servers;
}

fn user_servers() -> Vec<UserServer> {
    USER_SERVERS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The variable carrying a user server's token to `codex app-server`.
fn user_token_env(name: &str) -> String {
    format!("PASEO_MCP_TOKEN_{}", name.to_ascii_uppercase().replace('-', "_"))
}

/// A valid name, not a built-in and not taken (also as an environment
/// variable suffix), and an http(s) URL with a host.
pub fn check_user_server(name: &str, url: &str, taken: &[&str]) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("MCP server name '{name}' must be 1-64 characters of letters, digits, '-' or '_'"));
    }
    if name.eq_ignore_ascii_case(NAME) || name.eq_ignore_ascii_case(DEBUG_NAME) {
        return Err(format!("MCP server name '{name}' is reserved"));
    }
    if taken.iter().any(|t| user_token_env(t) == user_token_env(name)) {
        return Err(format!("MCP server name '{name}' is used twice"));
    }
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"));
    let host = rest.and_then(|r| r.split(['/', '?', '#']).next()).and_then(|a| a.rsplit('@').next()).unwrap_or("");
    if host.is_empty() || host.starts_with(':') || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("MCP server '{name}': the URL must be http:// or https:// with a host"));
    }
    Ok(())
}

/// The `--mcp-config` contents, None when no server is configured.
fn claude_config_json(cfg: &Config, user: &[UserServer]) -> Option<Value> {
    let mut servers = serde_json::Map::new();
    if let Some(url) = &cfg.zilong_mcp_url {
        let mut server = json!({ "type": "http", "url": url });
        if let Some(key) = &cfg.zilong_api_key {
            server["headers"] = json!({ "Authorization": format!("Bearer {key}") });
        }
        servers.insert(NAME.to_string(), server);
    }
    if let Some(debug) = &cfg.debug_mcp {
        let server = json!({ "type": "http", "url": debug.url, "headers": { "Authorization": format!("Bearer {}", debug.token) } });
        servers.insert(DEBUG_NAME.to_string(), server);
    }
    for u in user.iter().filter(|u| u.claude) {
        let mut server = json!({ "type": "http", "url": u.url });
        if let Some(token) = &u.token {
            server["headers"] = json!({ "Authorization": format!("Bearer {token}") });
        }
        servers.insert(u.name.clone(), server);
    }
    (!servers.is_empty()).then(|| json!({ "mcpServers": servers }))
}

/// Writes Claude's `--mcp-config` file (at startup and when the user's
/// servers change; 0600, it holds the secrets), or removes a stale one when
/// no server is configured.
pub fn write_claude_config(cfg: &Config) {
    let path = claude_path(cfg);
    let Some(config) = claude_config_json(cfg, &user_servers()) else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    match crate::util::write_atomic(&path, config.to_string().as_bytes(), true) {
        Ok(()) => {
            if let Some(url) = &cfg.zilong_mcp_url {
                crate::log!("mcp: {NAME} at {url} for Claude Code and Codex");
            }
            if let Some(debug) = &cfg.debug_mcp {
                crate::log!("mcp: {DEBUG_NAME} debug server at {} for Claude Code and Codex", debug.url);
            }
        }
        Err(e) => crate::warn!("mcp: cannot write {}: {e}; Claude Code starts without its MCP servers", path.display()),
    }
}

/// `--mcp-config <file>` for `claude`, when any server is configured.
pub fn claude_config(cfg: &Config) -> Option<PathBuf> {
    if cfg.zilong_mcp_url.is_none() && cfg.debug_mcp.is_none() && !user_servers().iter().any(|u| u.claude) {
        return None;
    }
    Some(claude_path(cfg)).filter(|p| p.exists())
}

/// `-c` overrides for `codex app-server`: the MCP servers, the secrets kept
/// out of agent shells, and no shell snapshots (they record exported variables).
pub fn codex_args(cfg: &Config) -> Vec<String> {
    codex_args_for(cfg, &user_servers())
}

fn codex_args_for(cfg: &Config, user: &[UserServer]) -> Vec<String> {
    let mut args: Vec<String> = ["-c", "features.shell_snapshot=false", "-c", "shell_environment_policy.ignore_default_excludes=true", "-c"].map(String::from).into();
    args.push(format!("shell_environment_policy.exclude={CODEX_SHELL_EXCLUDE}"));
    if let Some(url) = &cfg.zilong_mcp_url {
        args.extend(["-c".to_string(), format!("mcp_servers.{NAME}.url={}", toml_str(url))]);
        if cfg.zilong_api_key.is_some() {
            args.extend(["-c".to_string(), format!("mcp_servers.{NAME}.bearer_token_env_var={}", toml_str(KEY_ENV))]);
        }
    }
    if let Some(debug) = &cfg.debug_mcp {
        args.extend(["-c".to_string(), format!("mcp_servers.{DEBUG_NAME}.url={}", toml_str(&debug.url))]);
        args.extend(["-c".to_string(), format!("mcp_servers.{DEBUG_NAME}.bearer_token_env_var={}", toml_str(DEBUG_TOKEN_ENV))]);
    }
    for u in user.iter().filter(|u| u.codex) {
        args.extend(["-c".to_string(), format!("mcp_servers.{}.url={}", u.name, toml_str(&u.url))]);
        if u.token.is_some() {
            args.extend(["-c".to_string(), format!("mcp_servers.{}.bearer_token_env_var={}", u.name, toml_str(&user_token_env(&u.name)))]);
        }
    }
    args
}

/// Environment for `codex app-server`: the debug server's token and the user servers' tokens.
pub fn codex_env(cfg: &Config) -> Vec<(String, String)> {
    codex_env_for(cfg, &user_servers())
}

fn codex_env_for(cfg: &Config, user: &[UserServer]) -> Vec<(String, String)> {
    let debug = cfg.debug_mcp.iter().map(|d| (DEBUG_TOKEN_ENV.to_string(), d.token.clone()));
    debug.chain(user.iter().filter(|u| u.codex).filter_map(|u| Some((user_token_env(&u.name), u.token.clone()?)))).collect()
}

/// A TOML basic string (JSON string escaping is valid TOML for these).
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn debug(url: &str) -> DebugMcp {
        DebugMcp { url: url.into(), token: "t0ken".into(), host: IpAddr::V4(Ipv4Addr::LOCALHOST) }
    }

    #[test]
    fn codex_gets_url_and_key_variable_only() {
        let mut cfg = Config::load(&[]);
        cfg.zilong_mcp_url = None;
        cfg.debug_mcp = None;
        assert_eq!(codex_args_for(&cfg, &[]).join(" "), format!("-c features.shell_snapshot=false -c shell_environment_policy.ignore_default_excludes=true -c shell_environment_policy.exclude={CODEX_SHELL_EXCLUDE}"));
        assert!(codex_env_for(&cfg, &[]).is_empty());
        cfg.zilong_mcp_url = Some("http://pi:20128/mcp".into());
        cfg.zilong_api_key = Some("sk-secret".into());
        let args = codex_args_for(&cfg, &[]).join(" ");
        assert!(args.ends_with(r#"-c mcp_servers.zilong-router.url="http://pi:20128/mcp" -c mcp_servers.zilong-router.bearer_token_env_var="PASEO_ZILONG_API_KEY""#));
        assert!(!args.contains("sk-secret"));
    }

    #[test]
    fn codex_gets_the_debug_server_with_its_token_in_the_environment() {
        let mut cfg = Config::load(&[]);
        cfg.zilong_mcp_url = None;
        cfg.debug_mcp = Some(debug("http://127.0.0.1:6768/mcp"));
        let args = codex_args_for(&cfg, &[]).join(" ");
        assert!(args.ends_with(r#"-c mcp_servers.feiyi.url="http://127.0.0.1:6768/mcp" -c mcp_servers.feiyi.bearer_token_env_var="PASEO_LITE_MCP_TOKEN""#));
        assert!(!args.contains("t0ken"));
        assert_eq!(codex_env_for(&cfg, &[]), vec![(DEBUG_TOKEN_ENV.to_string(), "t0ken".to_string())]);
    }

    #[test]
    fn claude_config_holds_both_servers_or_none() {
        let mut cfg = Config::load(&[]);
        cfg.zilong_mcp_url = None;
        cfg.debug_mcp = None;
        assert!(claude_config_json(&cfg, &[]).is_none());
        cfg.debug_mcp = Some(debug("http://127.0.0.1:6768/mcp"));
        let only_debug = claude_config_json(&cfg, &[]).unwrap();
        assert_eq!(only_debug["mcpServers"].as_object().unwrap().len(), 1);
        assert_eq!(only_debug["mcpServers"]["feiyi"], json!({ "type": "http", "url": "http://127.0.0.1:6768/mcp", "headers": { "Authorization": "Bearer t0ken" } }));
        cfg.zilong_mcp_url = Some("http://pi:20128/mcp".into());
        cfg.zilong_api_key = Some("sk-secret".into());
        let both = claude_config_json(&cfg, &[]).unwrap();
        assert_eq!(both["mcpServers"]["zilong-router"]["headers"]["Authorization"], "Bearer sk-secret");
        assert_eq!(both["mcpServers"]["feiyi"]["url"], "http://127.0.0.1:6768/mcp");
    }

    #[test]
    fn debug_url_follows_the_listen_address() {
        let url = |l: &str| DebugMcp::for_listen(l).map(|d| d.url);
        assert_eq!(url("0.0.0.0:6768").as_deref(), Some("http://127.0.0.1:6768/mcp"));
        assert_eq!(url("127.0.0.1:7000").as_deref(), Some("http://127.0.0.1:7000/mcp"));
        assert_eq!(url("[::]:6768").as_deref(), Some("http://127.0.0.1:6768/mcp"));
        assert_eq!(url("[::1]:6768").as_deref(), Some("http://[::1]:6768/mcp"));
        assert_eq!(url("localhost:6768").as_deref(), Some("http://127.0.0.1:6768/mcp"));
        assert_eq!(url("192.168.1.5:6768").as_deref(), Some("http://192.168.1.5:6768/mcp"));
        assert_eq!(url("/run/paseo.sock"), None);
        assert_eq!(url("example.com:6768"), None);
        assert_eq!(url("127.0.0.1:0"), None);
    }

    #[test]
    fn debug_auth_needs_the_exact_bearer_token() {
        let d = DebugMcp::for_listen("127.0.0.1:6768").unwrap();
        assert_eq!(d.token.len(), 64);
        assert!(d.authorized(Some(&format!("Bearer {}", d.token))));
        assert!(d.authorized(Some(&format!("bearer {}", d.token))));
        assert!(!d.authorized(None));
        assert!(!d.authorized(Some(&d.token)));
        assert!(!d.authorized(Some("Bearer wrong")));
        assert!(!d.authorized(Some(&format!("Basic {}", d.token))));
        assert!(!format!("{d:?}").contains(&d.token));
    }

    #[test]
    fn debug_server_only_answers_this_machine() {
        let d = DebugMcp::for_listen("0.0.0.0:6768").unwrap();
        assert!(d.peer_allowed("127.0.0.1".parse().unwrap()));
        assert!(d.peer_allowed("::1".parse().unwrap()));
        assert!(d.peer_allowed("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!d.peer_allowed("10.0.0.2".parse().unwrap()));
        let specific = DebugMcp::for_listen("10.0.0.2:6768").unwrap();
        assert!(specific.peer_allowed("10.0.0.2".parse().unwrap()));
        assert!(!specific.peer_allowed("10.0.0.3".parse().unwrap()));
    }

    fn user(name: &str, token: Option<&str>, claude: bool, codex: bool) -> UserServer {
        UserServer { name: name.into(), url: format!("https://{name}.example/mcp"), token: token.map(String::from), claude, codex }
    }

    #[test]
    fn claude_config_has_user_servers_with_the_bearer_header() {
        let mut cfg = Config::load(&[]);
        cfg.zilong_mcp_url = None;
        cfg.debug_mcp = None;
        let servers = [user("docs", Some("tok-1"), true, true), user("open", None, true, false), user("codex-only", Some("tok-2"), false, true)];
        let json = claude_config_json(&cfg, &servers).unwrap();
        assert_eq!(json["mcpServers"]["docs"], json!({ "type": "http", "url": "https://docs.example/mcp", "headers": { "Authorization": "Bearer tok-1" } }));
        assert_eq!(json["mcpServers"]["open"], json!({ "type": "http", "url": "https://open.example/mcp" }));
        assert!(json["mcpServers"].get("codex-only").is_none());
        assert!(claude_config_json(&cfg, &servers[2..]).is_none());
    }

    #[test]
    fn codex_gets_user_servers_with_tokens_in_the_environment() {
        let mut cfg = Config::load(&[]);
        cfg.zilong_mcp_url = None;
        cfg.debug_mcp = None;
        let servers = [user("my-docs", Some("tok-1"), true, true), user("open", None, false, true), user("claude-only", Some("tok-2"), true, false)];
        let args = codex_args_for(&cfg, &servers).join(" ");
        assert!(args.ends_with(r#"-c mcp_servers.my-docs.url="https://my-docs.example/mcp" -c mcp_servers.my-docs.bearer_token_env_var="PASEO_MCP_TOKEN_MY_DOCS" -c mcp_servers.open.url="https://open.example/mcp""#), "{args}");
        assert!(!args.contains("tok-") && !args.contains("claude-only"));
        assert_eq!(codex_env_for(&cfg, &servers), vec![("PASEO_MCP_TOKEN_MY_DOCS".to_string(), "tok-1".to_string())]);
    }

    #[test]
    fn user_server_checks() {
        assert!(check_user_server("my_docs-2", "http://pi:8080/mcp", &["other"]).is_ok());
        assert!(check_user_server("a", "https://u:p@host/mcp?x=1", &[]).is_ok());
        let long = "x".repeat(65);
        for bad in ["", "has space", "dot.name", "zilong-router", "Feiyi", long.as_str()] {
            assert!(check_user_server(bad, "https://h/mcp", &[]).is_err(), "{bad}");
        }
        assert!(check_user_server("docs", "https://h/mcp", &["docs"]).is_err());
        assert!(check_user_server("Docs", "https://h/mcp", &["docs"]).is_err(), "same environment variable");
        assert!(check_user_server("a-b", "https://h/mcp", &["a_b"]).is_err(), "same environment variable");
        for url in ["", "ftp://h/mcp", "h/mcp", "http://", "https:///mcp", "http://:80/", "https://h /mcp", "javascript:alert(1)"] {
            assert!(check_user_server("docs", url, &[]).is_err(), "{url}");
        }
    }
}
