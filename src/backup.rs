//! Encrypted remote backup of the daemon's state to a hosted Turso (libSQL)
//! database, restored automatically on a machine with no local state.
//!
//! Configured by a `backup` section in `daemon-config.json` (edited by hand,
//! never served to the app): `tursoUrl`, `tursoToken`, `passphrase` (or env
//! `FEIYI_BACKUP_PASSPHRASE`) and `intervalSecs` (default 900).
//!
//! Table `feiyi_backup(key, updated_at, data)`, one row per item: `config`
//! (secrets removed), `projects` (with each git origin), `workspaces`,
//! `agent/<id>`, `timeline/<id>`, and `meta` (KDF salt, parameters and a
//! verifier, unencrypted). Every other row is deflate, then XSalsa20-Poly1305
//! under an Argon2id key (the item key is sealed inside, so a row moved to
//! another key is refused), as base64(nonce || ciphertext); a row over
//! `CHUNK_BYTES` is split into `<key>#0`, `<key>#1`, ... The identity keypair
//! is not backed up: after a restore the app is paired again.
//!
//! The cycle reads the state files itself, so it never touches agent turns,
//! and only uploads items whose content hash changed since the last cycle.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_secretbox::aead::{Aead, KeyInit};
use crypto_secretbox::{Key, Nonce, XSalsa20Poly1305};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const CHUNK_BYTES: usize = 512 * 1024;
/// A pipeline request carries at most this many bytes of rows (about four chunks) and statements.
const REQUEST_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_STATEMENTS: usize = 64;
/// An item whose plaintext is larger is skipped, and so is refused when opening a row.
const MAX_ITEM_BYTES: usize = 128 * 1024 * 1024;
const RESTORE_TIMEOUT: Duration = Duration::from_secs(300);
const META: &str = "meta";
const VERIFIER: &[u8] = b"feiyi-backup-v1";
const DEFAULT_INTERVAL_SECS: u64 = 900;
const START_DELAY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);
/// Config fields dropped from the config row: names (lowercase) containing one
/// of these, or ending in `token` (`authToken`, but not `maxTokens`).
const SECRET_WORDS: [&str; 7] = ["password", "passphrase", "secret", "apikey", "api_key", "privatekey", "private_key"];
/// Left next to `agents.json` when a restore failed: the loop must not
/// overwrite the remote backup with this machine's empty state.
const RESTORE_PENDING: &str = "backup-restore-pending";

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS feiyi_backup(key TEXT PRIMARY KEY, updated_at INTEGER NOT NULL, data TEXT NOT NULL)";
const UPSERT: &str = "INSERT INTO feiyi_backup(key, updated_at, data) VALUES(?, ?, ?) ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at, data = excluded.data";
const INSERT_META: &str = "INSERT INTO feiyi_backup(key, updated_at, data) VALUES(?, ?, ?) ON CONFLICT(key) DO NOTHING";
const DELETE: &str = "DELETE FROM feiyi_backup WHERE key = ?";
const SELECT_META: &str = "SELECT data FROM feiyi_backup WHERE key = 'meta'";
const SELECT_KEYS: &str = "SELECT key FROM feiyi_backup";
const SELECT_ROW: &str = "SELECT data FROM feiyi_backup WHERE key = ?";
/// Rows fetched per request during a restore (about 4 MB at most).
const ROWS_PER_REQUEST: usize = 8;

type SecretKey = [u8; 32];
type Rows = Vec<Vec<String>>;

#[derive(Clone, Default)]
pub struct BackupConfig {
    url: Option<String>,
    token: Option<String>,
    passphrase: Option<String>,
    interval: Duration,
}

/// Never prints the token or the passphrase (`Config` derives Debug).
impl std::fmt::Debug for BackupConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BackupConfig {{ url_set: {}, token_set: {}, passphrase_set: {}, interval: {:?} }}", self.url.is_some(), self.token.is_some(), self.passphrase.is_some(), self.interval)
    }
}

/// A complete, valid backup configuration.
#[derive(Clone)]
struct Ready {
    turso: Turso,
    passphrase: String,
}

#[derive(Clone)]
struct Turso {
    host: String,
    token: String,
}

fn clean(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|s| !s.is_empty() && !s.contains(char::is_control)).map(String::from)
}

impl BackupConfig {
    pub fn load(home: &Path) -> BackupConfig {
        let file = std::fs::read(home.join("daemon-config.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let env_passphrase = std::env::var("FEIYI_BACKUP_PASSPHRASE").ok();
        // Agents and terminals inherit the daemon's environment.
        std::env::remove_var("FEIYI_BACKUP_PASSPHRASE");
        BackupConfig::from_value(file.as_ref().and_then(|v| v.get("backup")), env_passphrase.as_deref())
    }

    fn from_value(section: Option<&Value>, env_passphrase: Option<&str>) -> BackupConfig {
        let text = |key: &str| clean(section.and_then(|s| s.get(key)).and_then(Value::as_str));
        BackupConfig {
            url: text("tursoUrl"),
            token: text("tursoToken"),
            passphrase: text("passphrase").or_else(|| clean(env_passphrase)),
            interval: Duration::from_secs(section.and_then(|s| s.get("intervalSecs")).and_then(Value::as_u64).unwrap_or(DEFAULT_INTERVAL_SECS).clamp(60, 86_400)),
        }
    }

    /// Ok(None) unless the URL, the token and a passphrase are all set.
    fn ready(&self) -> Result<Option<Ready>, String> {
        let (Some(url), Some(token), Some(passphrase)) = (&self.url, &self.token, &self.passphrase) else { return Ok(None) };
        Ok(Some(Ready { turso: Turso { host: host_of(url)?, token: token.clone() }, passphrase: passphrase.clone() }))
    }

    /// For `daemon_status`: no secrets, only whether each is set.
    pub fn redacted(&self) -> Value {
        json!({
            "enabled": matches!(self.ready(), Ok(Some(_))),
            "urlSet": self.url.is_some(),
            "tokenSet": self.token.is_some(),
            "passphraseSet": self.passphrase.is_some(),
            "intervalSecs": self.interval.as_secs(),
        })
    }
}

/// `libsql://db-org.turso.io` or `https://db-org.turso.io/` to the host name.
fn host_of(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("libsql://").or_else(|| url.strip_prefix("https://")).ok_or("backup.tursoUrl must start with libsql:// or https://")?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '@' || c == ':') {
        return Err("backup.tursoUrl has no valid host".into());
    }
    Ok(host.to_string())
}

// --- status ---------------------------------------------------------------

struct Status {
    enabled: bool,
    last_success: Option<String>,
    last_error: Option<String>,
    rows_pushed: u64,
}

static STATUS: Mutex<Status> = Mutex::new(Status { enabled: false, last_success: None, last_error: None, rows_pushed: 0 });

fn update_status(f: impl FnOnce(&mut Status)) {
    if let Ok(mut s) = STATUS.lock() {
        f(&mut s);
    }
}

pub fn status() -> Value {
    let Ok(s) = STATUS.lock() else { return Value::Null };
    json!({ "enabled": s.enabled, "lastSuccessAt": s.last_success, "lastError": s.last_error, "rowsPushed": s.rows_pushed })
}

// --- encryption -------------------------------------------------------------

struct Kdf {
    m: u32,
    t: u32,
    p: u32,
}

const KDF: Kdf = Kdf { m: 19456, t: 2, p: 1 };
/// Parameters read from the remote meta row are capped so a bad row cannot exhaust the machine.
const MAX_KDF_MEMORY_KIB: u32 = 64 * 1024;
const MAX_KDF_PASSES: u32 = 6;
const MAX_KDF_LANES: u32 = 4;

#[derive(Serialize, Deserialize)]
struct Meta {
    v: u8,
    kdf: String,
    salt: String,
    m: u32,
    t: u32,
    p: u32,
    verifier: String,
}

fn random_fill(buf: &mut [u8]) {
    // uuid v4 is backed by the OS CSPRNG (same trick as e2ee.rs).
    for part in buf.chunks_mut(16) {
        part.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..part.len()]);
    }
}

fn derive_key(passphrase: &str, salt: &[u8], kdf: &Kdf) -> Result<SecretKey, String> {
    if kdf.m > MAX_KDF_MEMORY_KIB {
        return Err("backup meta row asks for too much KDF memory".into());
    }
    if kdf.t > MAX_KDF_PASSES || kdf.p > MAX_KDF_LANES {
        return Err("backup meta row asks for too much KDF work".into());
    }
    let params = argon2::Params::new(kdf.m, kdf.t, kdf.p, Some(32)).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params).hash_password_into(passphrase.as_bytes(), salt, &mut key).map_err(|e| e.to_string())?;
    Ok(key)
}

/// A fresh meta row (JSON) and the key it unlocks.
fn make_meta(passphrase: &str, kdf: &Kdf) -> Result<(String, SecretKey), String> {
    let mut salt = [0u8; 16];
    random_fill(&mut salt);
    let key = derive_key(passphrase, &salt, kdf)?;
    let meta = Meta { v: 1, kdf: "argon2id".into(), salt: STANDARD.encode(salt), m: kdf.m, t: kdf.t, p: kdf.p, verifier: seal(&key, META, VERIFIER)? };
    Ok((serde_json::to_string(&meta).map_err(|e| e.to_string())?, key))
}

/// The key for `passphrase`, or an error before any data is written when it is wrong.
fn unlock(meta: &str, passphrase: &str) -> Result<SecretKey, String> {
    let meta: Meta = serde_json::from_str(meta).map_err(|e| format!("unreadable backup meta row: {e}"))?;
    if meta.v != 1 || meta.kdf != "argon2id" {
        return Err(format!("unsupported backup format (v{} {})", meta.v, meta.kdf));
    }
    let salt = STANDARD.decode(&meta.salt).map_err(|e| format!("bad salt in backup meta row: {e}"))?;
    let key = derive_key(passphrase, &salt, &Kdf { m: meta.m, t: meta.t, p: meta.p })?;
    match open(&key, META, &meta.verifier) {
        Ok(v) if v == VERIFIER => Ok(key),
        _ => Err("wrong backup passphrase".into()),
    }
}

/// Deflate and encrypt `plain`. The item key travels inside the ciphertext
/// (length-prefixed before the data), so `open` rejects a row moved to another key.
fn seal(key: &SecretKey, item: &str, plain: &[u8]) -> Result<String, String> {
    let mut nonce = [0u8; 24];
    random_fill(&mut nonce);
    let mut envelope = (item.len() as u16).to_be_bytes().to_vec();
    envelope.extend_from_slice(item.as_bytes());
    envelope.extend_from_slice(plain);
    let packed = miniz_oxide::deflate::compress_to_vec(&envelope, 6);
    let sealed = XSalsa20Poly1305::new(Key::from_slice(key)).encrypt(Nonce::from_slice(&nonce), packed.as_slice()).map_err(|_| "encryption failed".to_string())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(STANDARD.encode(out))
}

fn open(key: &SecretKey, item: &str, data: &str) -> Result<Vec<u8>, String> {
    let raw = STANDARD.decode(data).map_err(|e| format!("bad base64: {e}"))?;
    if raw.len() < 24 {
        return Err("row too short".into());
    }
    let (nonce, sealed) = raw.split_at(24);
    let packed = XSalsa20Poly1305::new(Key::from_slice(key)).decrypt(Nonce::from_slice(nonce), sealed).map_err(|_| "decryption failed".to_string())?;
    let envelope = miniz_oxide::inflate::decompress_to_vec_with_limit(&packed, MAX_ITEM_BYTES + 2 + item.len()).map_err(|e| format!("inflate failed: {e:?}"))?;
    let named = envelope.get(..2).map(|n| u16::from_be_bytes([n[0], n[1]]) as usize).and_then(|n| envelope.get(2..2 + n));
    if named != Some(item.as_bytes()) {
        return Err("row belongs to another key".into());
    }
    Ok(envelope[2 + item.len()..].to_vec())
}

// --- rows and chunks --------------------------------------------------------

/// `(row key, data)` pairs for one sealed item: the item key itself, or `<item>#<n>` chunks.
fn split_chunks(item: &str, sealed: String) -> Vec<(String, String)> {
    if sealed.len() <= CHUNK_BYTES {
        return vec![(item.to_string(), sealed)];
    }
    // base64 is ASCII, so any byte offset is a char boundary.
    sealed.as_bytes().chunks(CHUNK_BYTES).enumerate().map(|(i, c)| (format!("{item}#{i}"), String::from_utf8_lossy(c).into_owned())).collect()
}

fn chunk_of(key: &str) -> Option<(&str, usize)> {
    let (item, n) = key.rsplit_once('#')?;
    Some((item, n.parse().ok()?))
}

fn item_of(key: &str) -> &str {
    chunk_of(key).map_or(key, |(item, _)| item)
}

/// Inverse of `split_chunks` over all rows: item key to sealed data.
fn join_chunks(rows: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    let mut items = BTreeMap::new();
    let mut chunked: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    for (key, data) in rows {
        match chunk_of(&key) {
            Some((item, n)) => chunked.entry(item.to_string()).or_default().push((n, data)),
            None => {
                items.insert(key, data);
            }
        }
    }
    for (item, mut parts) in chunked {
        parts.sort_by_key(|(n, _)| *n);
        if parts.iter().enumerate().any(|(i, (n, _))| i != *n) {
            return Err(format!("{item}: a chunk is missing"));
        }
        items.insert(item, parts.into_iter().map(|(_, d)| d).collect());
    }
    Ok(items)
}

// --- Turso HTTP API -----------------------------------------------------------

struct Stmt {
    sql: &'static str,
    args: Vec<Value>,
}

fn text(s: &str) -> Value {
    json!({ "type": "text", "value": s })
}

fn int(n: i64) -> Value {
    json!({ "type": "integer", "value": n.to_string() })
}

fn pipeline_body(stmts: &[Stmt]) -> String {
    let mut requests: Vec<Value> = stmts.iter().map(|s| json!({ "type": "execute", "stmt": { "sql": s.sql, "args": s.args } })).collect();
    requests.push(json!({ "type": "close" }));
    json!({ "requests": requests }).to_string()
}

/// The rows of each statement (values as text, NULL as empty), or the first statement error.
fn parse_pipeline(body: &str, statements: usize) -> Result<Vec<Rows>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("unreadable pipeline response: {e}"))?;
    let results = v.get("results").and_then(Value::as_array).ok_or("pipeline response has no results")?;
    if results.len() < statements {
        return Err(format!("pipeline answered {} of {statements} statements", results.len()));
    }
    results
        .iter()
        .take(statements)
        .enumerate()
        .map(|(i, r)| match r.get("type").and_then(Value::as_str) {
            Some("ok") => Ok(r
                .pointer("/response/result/rows")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|row| row.as_array().map(|cells| cells.iter().map(|c| c.get("value").map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default()).collect()).unwrap_or_default())
                        .collect()
                })
                .unwrap_or_default()),
            _ => Err(format!("statement {i} failed: {}", r.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown error"))),
        })
        .collect()
}

impl Turso {
    async fn run(&self, stmts: Vec<Stmt>) -> Result<Vec<Rows>, String> {
        let auth = format!("Authorization: Bearer {}\r\n", self.token);
        let (status, body) = crate::push::request("POST", &self.host, "/v2/pipeline", Some(&pipeline_body(&stmts)), &auth, MAX_RESPONSE, REQUEST_TIMEOUT).await?;
        if status != 200 {
            return Err(format!("turso returned HTTP {status}: {}", crate::util::truncate(&body, 200)));
        }
        parse_pipeline(&body, stmts.len())
    }
}

// --- what to back up --------------------------------------------------------------

enum Item {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl Item {
    /// None when the file vanished since the directory was listed.
    fn bytes(&self) -> Result<Option<Vec<u8>>, String> {
        match self {
            Item::Bytes(b) => Ok(Some(b.clone())),
            Item::File(path) => match std::fs::read(path) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("{}: {e}", path.display())),
            },
        }
    }
}

fn read_list(path: &Path) -> Result<Vec<Value>, String> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn strip_secrets(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.retain(|k, _| {
                let k = k.to_ascii_lowercase();
                !k.ends_with("token") && !SECRET_WORDS.iter().any(|w| k.contains(w))
            });
            map.values_mut().for_each(strip_secrets);
        }
        Value::Array(list) => list.iter_mut().for_each(strip_secrets),
        _ => {}
    }
}

/// The config row: `daemon-config.json` without secret fields or the backup section.
fn config_row(raw: &[u8]) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(raw).ok()?;
    strip_secrets(&mut v);
    v.as_object_mut()?.remove("backup");
    serde_json::to_vec(&v).ok()
}

/// The git `origin` of `dir` without credentials (they would end up in the backup and the log).
async fn origin_of(dir: &str) -> Option<String> {
    let out = crate::projects::run("git", &["remote", "get-url", "origin"], Some(Path::new(dir)), GIT_TIMEOUT).await.ok()?;
    clean(Some(out.as_str())).map(|url| crate::mcp_server::redact_url(&url))
}

/// The items to back up, and each project's remote. A project whose directory
/// is gone (a clone still pending) keeps the remote known from before.
async fn gather(home: &Path, known: &BTreeMap<String, String>) -> Result<(BTreeMap<String, Item>, BTreeMap<String, String>), String> {
    let mut projects = read_list(&home.join("projects.json"))?;
    let mut remotes = BTreeMap::new();
    for p in projects.iter_mut() {
        let Some(root) = p.get("rootPath").and_then(Value::as_str).map(String::from) else { continue };
        let remote = match origin_of(&root).await {
            Some(remote) => Some(remote),
            None => known.get(&root).cloned(),
        };
        if let Some(remote) = remote {
            p["remote"] = json!(remote);
            remotes.insert(root, remote);
        }
    }
    let home = home.to_path_buf();
    let items = tokio::task::spawn_blocking(move || items(&home, &projects)).await.map_err(|e| e.to_string())??;
    Ok((items, remotes))
}

fn items(home: &Path, projects: &[Value]) -> Result<BTreeMap<String, Item>, String> {
    let mut out = BTreeMap::new();
    if let Some(row) = std::fs::read(home.join("daemon-config.json")).ok().and_then(|raw| config_row(&raw)) {
        out.insert("config".to_string(), Item::Bytes(row));
    }
    out.insert("projects".to_string(), Item::Bytes(serde_json::to_vec(projects).map_err(|e| e.to_string())?));
    if home.join("workspaces.json").exists() {
        out.insert("workspaces".to_string(), Item::File(home.join("workspaces.json")));
    }
    for agent in read_list(&home.join("agents.json"))? {
        let Some(id) = agent.get("id").and_then(Value::as_str) else { continue };
        out.insert(format!("agent/{id}"), Item::Bytes(serde_json::to_vec(&agent).map_err(|e| e.to_string())?));
    }
    match std::fs::read_dir(home.join("timelines")) {
        Ok(dir) => {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_suffix(".jsonl").filter(|id| !id.is_empty()) {
                    out.insert(format!("timeline/{id}"), Item::File(entry.path()));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("timelines: {e}")),
    }
    Ok(out)
}

// --- change detection ----------------------------------------------------------------

/// Local-only change detector (FNV-1a); never leaves the machine.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

fn belongs(key: &str, item: &str) -> bool {
    item_of(key) == item
}

/// One item to write (`put` rows, then delete the `delete` rows) or, with no
/// hash, to remove from the remote.
struct Change {
    item: String,
    hash: Option<u64>,
    put: Vec<(String, String)>,
    delete: Vec<String>,
}

impl Change {
    fn size(&self) -> usize {
        self.put.iter().map(|(_, d)| d.len()).sum()
    }

    fn statements(&self, now: i64) -> Vec<Stmt> {
        let puts = self.put.iter().map(|(k, d)| Stmt { sql: UPSERT, args: vec![text(k), int(now), text(d)] });
        let deletes = self.delete.iter().map(|k| Stmt { sql: DELETE, args: vec![text(k)] });
        puts.chain(deletes).collect()
    }
}

/// What the remote holds, as far as this process knows: the hash of each
/// item as last written, and every remote row key.
#[derive(Clone, Default)]
struct State {
    hashes: BTreeMap<String, u64>,
    remote: BTreeSet<String>,
}

impl State {
    /// Hashes survive a restart, but only for items the remote still has.
    fn new(mut hashes: BTreeMap<String, u64>, remote: BTreeSet<String>) -> State {
        let held: BTreeSet<&str> = remote.iter().map(|k| item_of(k)).collect();
        hashes.retain(|item, _| held.contains(item.as_str()));
        State { hashes, remote }
    }

    fn plan(&self, key: &SecretKey, items: &BTreeMap<String, Item>) -> Result<Vec<Change>, String> {
        let mut changes = Vec::new();
        self.plan_with(key, items, &mut |change| {
            changes.push(change);
            true
        })?;
        Ok(changes)
    }

    /// Hands each change to `sink` as soon as it is sealed, so only one item
    /// is in memory at a time; the sink returns false to stop. Only items this
    /// machine has uploaded (it holds a hash) are ever deleted remotely: rows
    /// another machine wrote are left alone.
    fn plan_with(&self, key: &SecretKey, items: &BTreeMap<String, Item>, sink: &mut dyn FnMut(Change) -> bool) -> Result<(), String> {
        let mut rows_of: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
        for k in &self.remote {
            rows_of.entry(item_of(k)).or_default().push(k);
        }
        let mut present = BTreeSet::new();
        for (item, source) in items {
            let Some(bytes) = source.bytes()? else { continue };
            present.insert(item.as_str());
            if bytes.len() > MAX_ITEM_BYTES {
                crate::warn!("backup: {item} is over {} MiB and is not backed up", MAX_ITEM_BYTES >> 20);
                continue;
            }
            let hash = fingerprint(&bytes);
            if self.hashes.get(item) == Some(&hash) {
                continue;
            }
            let put = split_chunks(item, seal(key, item, &bytes)?);
            let delete = rows_of.get(item.as_str()).into_iter().flatten().filter(|k| !put.iter().any(|(p, _)| p == **k)).map(|k| (*k).clone()).collect();
            if !sink(Change { item: item.clone(), hash: Some(hash), put, delete }) {
                return Ok(());
            }
        }
        for (item, rows) in &rows_of {
            if *item == META || present.contains(item) || !self.hashes.contains_key(*item) {
                continue;
            }
            let delete = rows.iter().map(|k| (*k).clone()).collect();
            if !sink(Change { item: item.to_string(), hash: None, put: Vec::new(), delete }) {
                break;
            }
        }
        Ok(())
    }

    fn commit(&mut self, change: &Change) {
        for key in &change.delete {
            self.remote.remove(key);
        }
        self.remote.extend(change.put.iter().map(|(k, _)| k.clone()));
        match change.hash {
            Some(h) => self.hashes.insert(change.item.clone(), h),
            None => self.hashes.remove(&change.item),
        };
    }
}

// --- the backup loop -----------------------------------------------------------------------

/// `backup-state.json`: what the last cycles left behind.
#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    /// Salt of the meta row the hashes belong to; they are dropped for another backup.
    #[serde(default)]
    salt: Option<String>,
    #[serde(default)]
    hashes: BTreeMap<String, u64>,
    /// Each project's git origin as last known. Kept while a project's
    /// directory is missing (a restore clone still pending), so it is never lost.
    #[serde(default)]
    remotes: BTreeMap<String, String>,
}

fn state_path(home: &Path) -> PathBuf {
    home.join("backup-state.json")
}

fn load_state_file(home: &Path) -> StateFile {
    std::fs::read(state_path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_state_file(home: &Path, file: &StateFile) -> Result<(), String> {
    let bytes = serde_json::to_vec(file).map_err(|e| e.to_string())?;
    crate::util::write_atomic(&state_path(home), &bytes, true).map_err(|e| format!("{}: {e}", state_path(home).display()))
}

struct Backup {
    home: PathBuf,
    ready: Ready,
    key: Option<SecretKey>,
    salt: Option<String>,
    state: State,
    remotes: BTreeMap<String, String>,
    /// This machine has had local state, or the remote held no data when it
    /// first uploaded: only then may an empty local state delete remote rows.
    owns_remote: bool,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn salt_of(meta: &str) -> Option<String> {
    serde_json::from_str::<Meta>(meta).ok().map(|m| m.salt)
}

/// Create the table, read the meta row and the remote keys; no key when no backup was ever written.
async fn open_remote(ready: &Ready) -> Result<(Option<(SecretKey, String)>, BTreeSet<String>), String> {
    let mut res = ready.turso.run(vec![Stmt { sql: CREATE_TABLE, args: vec![] }, Stmt { sql: SELECT_META, args: vec![] }, Stmt { sql: SELECT_KEYS, args: vec![] }]).await?;
    let remote = res.remove(2).into_iter().filter_map(|r| r.into_iter().next()).collect();
    let meta = res.remove(1).into_iter().next().and_then(|r| r.into_iter().next());
    let opened = match meta {
        Some(meta) => Some((unlock_blocking(&meta, &ready.passphrase).await?, meta)),
        None => None,
    };
    Ok((opened, remote))
}

async fn unlock_blocking(meta: &str, passphrase: &str) -> Result<SecretKey, String> {
    let (meta, passphrase) = (meta.to_string(), passphrase.to_string());
    tokio::task::spawn_blocking(move || unlock(&meta, &passphrase)).await.map_err(|e| e.to_string())?
}

/// An empty local state must never replace a remote backup: not after a
/// failed restore (marker file), and not on a machine that never had state
/// while the remote holds data. Passing once makes this machine the owner,
/// after which deleting everything locally is a real deletion.
fn check_may_write(home: &Path, owns_remote: &mut bool, remote: &BTreeSet<String>) -> Result<(), String> {
    let marker = home.join(RESTORE_PENDING);
    if marker.exists() {
        return Err(format!("a restore failed earlier; backup is paused so the remote copy is not overwritten (fix the cause and restart, or delete {} to back up this machine anyway)", marker.display()));
    }
    if !*owns_remote {
        if !has_local_state(home) && remote.iter().any(|k| k != META) {
            return Err("this machine has no agents or projects but the remote backup has data; backup is paused so it is not overwritten (restart the daemon to restore it)".into());
        }
        *owns_remote = true;
    }
    Ok(())
}

impl Stmt {
    /// Bytes of data the statement carries.
    fn size(&self) -> usize {
        self.args.iter().filter_map(|a| a["value"].as_str()).map(str::len).sum()
    }
}

impl Backup {
    async fn open(&mut self) -> Result<(), String> {
        let (opened, mut remote) = open_remote(&self.ready).await?;
        let (key, meta) = match opened {
            Some(opened) => opened,
            None => {
                let pass = self.ready.passphrase.clone();
                let (meta, _) = tokio::task::spawn_blocking(move || make_meta(&pass, &KDF)).await.map_err(|e| e.to_string())??;
                // Another machine may have written its meta row first: use the stored one.
                let mut res = self.ready.turso.run(vec![Stmt { sql: INSERT_META, args: vec![text(META), int(now_secs()), text(&meta)] }, Stmt { sql: SELECT_META, args: vec![] }]).await?;
                let stored = res.remove(1).into_iter().next().and_then(|r| r.into_iter().next()).ok_or("the meta row is missing after writing it")?;
                remote.insert(META.to_string());
                (unlock_blocking(&stored, &self.ready.passphrase).await?, stored)
            }
        };
        let salt = salt_of(&meta);
        let mut file = load_state_file(&self.home);
        if file.salt != salt {
            file.hashes.clear();
        }
        self.state = State::new(file.hashes, remote);
        self.remotes = file.remotes;
        self.salt = salt;
        self.key = Some(key);
        Ok(())
    }

    /// Upload what changed since the last cycle; the number of rows written.
    async fn cycle(&mut self) -> Result<usize, String> {
        if self.key.is_none() {
            self.open().await?;
        }
        let Some(key) = self.key else { return Ok(0) };
        check_may_write(&self.home, &mut self.owns_remote, &self.state.remote)?;
        let (items, remotes) = gather(&self.home, &self.remotes).await?;
        self.remotes = remotes;
        let state = self.state.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Change>(1);
        let planner = tokio::task::spawn_blocking(move || state.plan_with(&key, &items, &mut |change| tx.blocking_send(change).is_ok()));
        let (mut pushed, mut size, mut batch) = (0, 0, Vec::new());
        while let Some(change) = rx.recv().await {
            size += change.size();
            batch.push(change);
            if size >= REQUEST_BYTES {
                pushed += self.send(std::mem::take(&mut batch)).await?;
                size = 0;
            }
        }
        planner.await.map_err(|e| e.to_string())??;
        Ok(pushed + self.send(batch).await?)
    }

    /// Run the batch's statements in requests of bounded size, then remember them as written.
    async fn send(&mut self, batch: Vec<Change>) -> Result<usize, String> {
        if batch.is_empty() {
            return Ok(0);
        }
        let now = now_secs();
        let mut stmts = batch.iter().flat_map(|c| c.statements(now)).collect::<Vec<_>>().into_iter().peekable();
        while stmts.peek().is_some() {
            let mut group = Vec::new();
            let mut bytes = 0;
            while let Some(next) = stmts.next_if(|s| group.is_empty() || (group.len() < REQUEST_STATEMENTS && bytes + s.size() <= REQUEST_BYTES)) {
                bytes += next.size();
                group.push(next);
            }
            self.ready.turso.run(group).await?;
        }
        for change in &batch {
            self.state.commit(change);
        }
        let file = StateFile { salt: self.salt.clone(), hashes: self.state.hashes.clone(), remotes: self.remotes.clone() };
        if let Err(e) = save_state_file(&self.home, &file) {
            crate::warn!("backup: cannot save the backup state: {e}");
        }
        Ok(batch.iter().map(|c| c.put.len()).sum())
    }
}

/// Start the periodic backup when it is configured.
pub fn spawn(cfg: &crate::config::Config) {
    let ready = match cfg.backup.ready() {
        Ok(Some(ready)) => ready,
        Ok(None) => return crate::log!("remote backup is off (it needs backup.tursoUrl, backup.tursoToken and a passphrase)"),
        Err(e) => return crate::warn!("remote backup is off: {e}"),
    };
    update_status(|s| s.enabled = true);
    crate::log!("remote backup on: every {}s to {}", cfg.backup.interval.as_secs(), ready.turso.host);
    let backup = Backup { home: cfg.home.clone(), ready, key: None, salt: None, state: State::default(), remotes: BTreeMap::new(), owns_remote: false };
    tokio::spawn(run(backup, cfg.backup.interval));
}

async fn run(mut backup: Backup, interval: Duration) {
    tokio::time::sleep(START_DELAY).await;
    let mut failures = 0u32;
    loop {
        let wait = match backup.cycle().await {
            Ok(rows) => {
                failures = 0;
                update_status(|s| {
                    s.last_success = Some(crate::util::now_iso());
                    s.last_error = None;
                    s.rows_pushed += rows as u64;
                });
                if rows > 0 {
                    crate::log!("backup: pushed {rows} rows");
                }
                interval
            }
            Err(e) => {
                failures += 1;
                backup.key = None;
                crate::warn!("backup failed: {e}");
                update_status(|s| s.last_error = Some(e));
                interval.saturating_mul(1 << failures.min(4)).min(MAX_BACKOFF.max(interval))
            }
        };
        tokio::time::sleep(wait).await;
    }
}

// --- restore ---------------------------------------------------------------------------------

fn list_has_entries(path: &Path) -> bool {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice::<Vec<Value>>(&b).map(|l| !l.is_empty()).unwrap_or(true),
        Err(_) => false,
    }
}

fn has_local_state(home: &Path) -> bool {
    ["agents.json", "projects.json", "workspaces.json"].iter().any(|f| list_has_entries(&home.join(f)))
}

/// `has_local_state`, and a leftover restore-pending marker goes away once
/// the machine has state: the empty-local guard still protects the remote.
fn has_state_clearing_marker(home: &Path) -> bool {
    let has = has_local_state(home);
    if has {
        let _ = std::fs::remove_file(home.join(RESTORE_PENDING));
    }
    has
}

#[derive(Default, Debug)]
struct Restored {
    agents: usize,
    timelines: usize,
    projects: usize,
    /// Projects whose directory is missing: (path, git remote).
    clones: Vec<(String, String)>,
}

/// Write a restored timeline unless one is already on disk; whether it was written.
fn write_timeline(home: &Path, id: &str, bytes: &[u8]) -> Result<bool, String> {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let path = home.join("timelines").join(format!("{safe}.jsonl"));
    if safe.is_empty() || safe != id || path.exists() {
        return Ok(false);
    }
    crate::util::write_atomic(&path, bytes, true).map_err(|e| format!("timelines/{safe}.jsonl: {e}"))?;
    Ok(true)
}

/// A JSON list of objects.
fn object_list(bytes: &[u8], what: &str) -> Result<Vec<Value>, String> {
    let list: Vec<Value> = serde_json::from_slice(bytes).map_err(|e| format!("{what}: {e}"))?;
    if list.iter().all(Value::is_object) {
        Ok(list)
    } else {
        Err(format!("{what}: entries must be objects"))
    }
}

/// Write decrypted items under `home`. Called only when there is no local
/// agent, project or workspace data; timelines already on disk and local
/// config keys are kept. Everything is validated before the first write, and
/// the files that make the machine count as having state go last (removed
/// again if one fails), so a failed restore can simply be run again.
fn write_restored(home: &Path, items: &BTreeMap<String, Vec<u8>>) -> Result<Restored, String> {
    let config = items.get("config").map(|b| serde_json::from_slice::<serde_json::Map<String, Value>>(b).map_err(|e| format!("config row: {e}"))).transpose()?;
    let workspaces = items.get("workspaces").map(|b| object_list(b, "workspaces row")).transpose()?;
    let mut projects = items.get("projects").map(|b| object_list(b, "projects row")).transpose()?.unwrap_or_default();
    let mut agents = Vec::new();
    for (key, bytes) in items {
        let Some(id) = key.strip_prefix("agent/") else { continue };
        let mut agent: Value = serde_json::from_slice(bytes).map_err(|e| format!("{key}: {e}"))?;
        let record = agent.as_object_mut().filter(|o| o.get("id").and_then(Value::as_str) == Some(id)).ok_or_else(|| format!("{key}: not an agent record"))?;
        // An old prompt must not run again on the new machine.
        record.remove("pendingTurn");
        record.remove("pendingQueue");
        agents.push(agent);
    }
    let mut done = Restored::default();
    let mut remotes = BTreeMap::new();
    for p in projects.iter_mut() {
        let path = p.get("rootPath").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from).ok_or("projects row: a project has no rootPath")?;
        let remote = p.as_object_mut().and_then(|o| o.remove("remote")).and_then(|r| r.as_str().map(crate::mcp_server::redact_url));
        if let Some(remote) = remote.filter(|r| !r.is_empty()) {
            if !Path::new(&path).exists() {
                done.clones.push((path.clone(), remote.clone()));
            }
            remotes.insert(path, remote);
        }
    }
    let list_bytes = |list: &Vec<Value>| serde_json::to_vec(list).map_err(|e| e.to_string());
    let projects_bytes = items.contains_key("projects").then(|| list_bytes(&projects)).transpose()?;
    let workspaces_bytes = workspaces.as_ref().map(list_bytes).transpose()?;
    let agents_bytes = (!agents.is_empty()).then(|| list_bytes(&agents)).transpose()?;
    let config_bytes = match config {
        Some(mut config) => {
            let local = std::fs::read(home.join("daemon-config.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Map<String, Value>>(&b).ok()).unwrap_or_default();
            config.extend(local);
            Some(serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?)
        }
        None => None,
    };

    let write = |name: &str, bytes: &[u8]| crate::util::write_atomic(&home.join(name), bytes, true).map_err(|e| format!("{name}: {e}"));
    for (key, bytes) in items {
        if let Some(id) = key.strip_prefix("timeline/") {
            done.timelines += usize::from(write_timeline(home, id, bytes)?);
        }
    }
    if let Some(bytes) = &config_bytes {
        write("daemon-config.json", bytes)?;
    }
    if !remotes.is_empty() {
        save_state_file(home, &StateFile { remotes, ..StateFile::default() })?;
    }
    let files = [("workspaces.json", workspaces_bytes), ("projects.json", projects_bytes), ("agents.json", agents_bytes)];
    let written = files.iter().filter_map(|(name, bytes)| bytes.as_ref().map(|b| write(name, b))).collect::<Result<Vec<_>, _>>();
    if let Err(e) = written {
        for (name, _) in &files {
            let _ = std::fs::remove_file(home.join(name));
        }
        return Err(e);
    }
    done.agents = agents.len();
    done.projects = projects.len();
    Ok(done)
}

/// The remote rows of each item, without `meta`.
struct Group {
    item: String,
    keys: Vec<String>,
}

fn item_groups(remote: &BTreeSet<String>) -> Vec<Group> {
    let mut by_item: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for key in remote.iter().filter(|k| *k != META) {
        by_item.entry(item_of(key)).or_default().push(key.clone());
    }
    by_item.into_iter().map(|(item, keys)| Group { item: item.to_string(), keys }).collect()
}

/// The data of each row key, in order, in requests of at most `ROWS_PER_REQUEST` rows.
async fn fetch_rows(turso: &Turso, keys: &[String]) -> Result<Vec<String>, String> {
    let mut data = Vec::with_capacity(keys.len());
    for part in keys.chunks(ROWS_PER_REQUEST) {
        let res = turso.run(part.iter().map(|k| Stmt { sql: SELECT_ROW, args: vec![text(k)] }).collect()).await?;
        for (key, rows) in part.iter().zip(res) {
            data.push(rows.into_iter().next().and_then(|r| r.into_iter().next()).ok_or_else(|| format!("{key}: the row is gone"))?);
        }
    }
    Ok(data)
}

/// Decrypt one item from its rows (same order as `group.keys`). A timeline is
/// written to its file now; any other item is small and kept in `small` to be
/// validated and written together.
fn absorb(home: &Path, key: &SecretKey, group: &Group, rows: Vec<String>, small: &mut BTreeMap<String, Vec<u8>>, timelines: &mut usize) -> Result<(), String> {
    let sealed = join_chunks(group.keys.iter().cloned().zip(rows).collect())?.remove(&group.item).ok_or_else(|| format!("{}: unexpected rows", group.item))?;
    let plain = open(key, &group.item, &sealed).map_err(|e| format!("{}: {e}", group.item))?;
    match group.item.strip_prefix("timeline/") {
        Some(id) => *timelines += usize::from(write_timeline(home, id, &plain)?),
        None => {
            small.insert(group.item.clone(), plain);
        }
    }
    Ok(())
}

/// Memory stays at about one item: rows are fetched a few at a time and each
/// item is decrypted and, for a timeline, written before the next is read.
async fn restore(home: &Path, ready: &Ready) -> Result<Option<Restored>, String> {
    let (opened, remote) = open_remote(ready).await?;
    let Some((key, _)) = opened else { return Ok(None) };
    let groups = item_groups(&remote);
    let (mut small, mut timelines) = (BTreeMap::new(), 0);
    let mut start = 0;
    while start < groups.len() {
        let mut end = start + 1;
        let mut rows = groups[start].keys.len();
        while end < groups.len() && rows + groups[end].keys.len() <= ROWS_PER_REQUEST {
            rows += groups[end].keys.len();
            end += 1;
        }
        if let Some(g) = groups[start..end].iter().find(|g| g.keys.len() * CHUNK_BYTES > MAX_ITEM_BYTES + CHUNK_BYTES) {
            return Err(format!("{}: too large to restore", g.item));
        }
        let keys: Vec<String> = groups[start..end].iter().flat_map(|g| g.keys.clone()).collect();
        let mut data = fetch_rows(&ready.turso, &keys).await?.into_iter();
        for group in &groups[start..end] {
            absorb(home, &key, group, data.by_ref().take(group.keys.len()).collect(), &mut small, &mut timelines)?;
        }
        start = end;
    }
    let mut done = write_restored(home, &small)?;
    done.timelines += timelines;
    Ok(Some(done))
}

/// On a machine with no local state, pull the backup and write the files
/// before the daemon loads them. Never overwrites existing data. On failure
/// the daemon starts empty and a marker file pauses the backup loop, which
/// would otherwise replace the remote copy with this empty state.
pub async fn restore_if_empty(cfg: &crate::config::Config) {
    let ready = match cfg.backup.ready() {
        Ok(Some(ready)) => ready,
        Ok(None) => return,
        Err(e) => return crate::warn!("backup restore skipped: {e}"),
    };
    if has_state_clearing_marker(&cfg.home) {
        return crate::log!("backup restore skipped: local state exists");
    }
    let marker = cfg.home.join(RESTORE_PENDING);
    let outcome = tokio::time::timeout(RESTORE_TIMEOUT, restore(&cfg.home, &ready)).await.unwrap_or_else(|_| Err("timed out".into()));
    match outcome {
        Ok(found) => {
            let _ = std::fs::remove_file(&marker);
            match found {
                Some(r) => {
                    crate::log!("backup restored: {} agents, {} timelines, {} projects; the app must be paired again", r.agents, r.timelines, r.projects);
                    for (path, remote) in r.clones {
                        tokio::spawn(clone_project(path, remote));
                    }
                }
                None => crate::log!("backup restore: the remote database has no backup yet"),
            }
        }
        Err(e) => {
            crate::warn!("backup restore failed, starting empty and pausing the backup: {e}");
            if let Err(we) = std::fs::write(&marker, "") {
                crate::warn!("backup: cannot write {}: {we}", marker.display());
            }
        }
    }
}

async fn clone_project(path: String, remote: String) {
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match crate::projects::run("git", &["clone", "--", &remote, &path], None, CLONE_TIMEOUT).await {
        Ok(_) => crate::log!("backup restore: cloned {remote} into {path}"),
        Err(e) => crate::warn!("backup restore: clone of {remote} into {path} failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Kdf = Kdf { m: 8, t: 1, p: 1 };

    fn temp_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("feiyi-backup-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bytes_item(s: &str) -> Item {
        Item::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let sealed = seal(&key, "x", b"hello hello hello hello").unwrap();
        assert!(!sealed.contains("hello"));
        assert_eq!(open(&key, "x", &sealed).unwrap(), b"hello hello hello hello");
        assert_ne!(sealed, seal(&key, "x", b"hello hello hello hello").unwrap(), "random nonce");
        let mut other = key;
        other[0] ^= 1;
        assert!(open(&other, "x", &sealed).is_err());
    }

    #[test]
    fn wrong_passphrase_is_rejected_by_the_verifier() {
        let (meta, key) = make_meta("right", &FAST).unwrap();
        assert_eq!(unlock(&meta, "right").unwrap(), key);
        assert_eq!(unlock(&meta, "wrong").unwrap_err(), "wrong backup passphrase");
    }

    #[test]
    fn large_items_split_into_chunks_and_join_back() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let small = split_chunks("agent/a", "abc".into());
        assert_eq!(small, vec![("agent/a".to_string(), "abc".to_string())]);
        let big: String = (0..CHUNK_BYTES * 2 + 10).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let rows = split_chunks("timeline/t", big.clone());
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["timeline/t#0", "timeline/t#1", "timeline/t#2"]);
        assert!(rows.iter().all(|(_, d)| d.len() <= CHUNK_BYTES));
        let mut all: BTreeMap<String, String> = rows.into_iter().collect();
        all.insert("agent/a".into(), "abc".into());
        let joined = join_chunks(all.clone()).unwrap();
        assert_eq!(joined["timeline/t"], big);
        assert_eq!(joined["agent/a"], "abc");
        all.remove("timeline/t#1");
        assert!(join_chunks(all).is_err());
        let sealed = seal(&key, "x", &vec![7u8; 10]).unwrap();
        assert_eq!(open(&key, "x", &join_chunks(split_chunks("x", sealed).into_iter().collect()).unwrap()["x"]).unwrap(), vec![7u8; 10]);
    }

    #[test]
    fn config_row_drops_secrets_and_the_backup_section() {
        let raw = json!({
            "appendSystemPrompt": "be brief",
            "password": "p1",
            "zilongApiKey": "k1",
            "providers": { "claude": { "enabled": false, "apiKey": "k2", "authToken": "t1" } },
            "list": [{ "secret": "s1", "keep": 1 }],
            "backup": { "tursoUrl": "libsql://x", "tursoToken": "t2", "passphrase": "pass-zz9" },
        });
        let row = String::from_utf8(config_row(raw.to_string().as_bytes()).unwrap()).unwrap();
        for leaked in ["p1", "k1", "k2", "t1", "s1", "t2", "pass-zz9", "libsql", "backup"] {
            assert!(!row.contains(leaked), "{leaked} in {row}");
        }
        let kept: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(kept["appendSystemPrompt"], "be brief");
        assert_eq!(kept["providers"]["claude"]["enabled"], false);
        assert_eq!(kept["list"][0]["keep"], 1);
    }

    #[test]
    fn second_cycle_without_changes_writes_nothing() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let mut state = State::new(BTreeMap::new(), BTreeSet::from([META.to_string()]));
        let mut items = BTreeMap::from([("config".to_string(), bytes_item("{}")), ("agent/a".to_string(), bytes_item("one"))]);
        let first = state.plan(&key, &items).unwrap();
        assert_eq!(first.len(), 2);
        first.iter().for_each(|c| state.commit(c));
        assert!(state.plan(&key, &items).unwrap().is_empty());

        items.insert("agent/a".into(), bytes_item("two"));
        let changed = state.plan(&key, &items).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].item, "agent/a");
        changed.iter().for_each(|c| state.commit(c));

        items.remove("agent/a");
        let removed = state.plan(&key, &items).unwrap();
        assert_eq!((removed.len(), removed[0].hash, removed[0].delete.clone()), (1, None, vec!["agent/a".to_string()]));
        removed.iter().for_each(|c| state.commit(c));
        assert!(state.plan(&key, &items).unwrap().is_empty());
        assert!(!state.remote.contains("agent/a") && state.remote.contains(META));
    }

    #[test]
    fn shrinking_an_item_deletes_its_stale_chunks() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let state = State::new(BTreeMap::from([("timeline/t".to_string(), 1)]), BTreeSet::from(["timeline/t#0".to_string(), "timeline/t#1".to_string()]));
        let items = BTreeMap::from([("timeline/t".to_string(), bytes_item("short"))]);
        let changes = state.plan(&key, &items).unwrap();
        assert_eq!(changes[0].put[0].0, "timeline/t");
        assert_eq!(changes[0].delete, ["timeline/t#0", "timeline/t#1"]);
    }

    #[test]
    fn restore_is_skipped_when_local_state_exists() {
        let home = temp_home();
        assert!(!has_local_state(&home));
        std::fs::write(home.join("agents.json"), "[]").unwrap();
        assert!(!has_local_state(&home), "an empty list is no data");
        std::fs::write(home.join("projects.json"), r#"[{"rootPath":"/p"}]"#).unwrap();
        assert!(has_local_state(&home));
        std::fs::write(home.join("projects.json"), "{broken").unwrap();
        assert!(has_local_state(&home), "unreadable data is never overwritten");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn restore_writes_files_and_keeps_local_config() {
        let home = temp_home();
        std::fs::write(home.join("daemon-config.json"), r#"{"backup":{"tursoUrl":"libsql://x"},"appendSystemPrompt":"local"}"#).unwrap();
        std::fs::create_dir_all(home.join("timelines")).unwrap();
        std::fs::write(home.join("timelines/t2.jsonl"), "keep\n").unwrap();
        let missing = home.join("gone");
        let items = BTreeMap::from([
            ("config".to_string(), br#"{"appendSystemPrompt":"remote","providers":{}}"#.to_vec()),
            ("projects".to_string(), json!([{ "rootPath": missing.to_string_lossy(), "remote": "git@h:o/r.git" }, { "rootPath": home.to_string_lossy() }]).to_string().into_bytes()),
            ("agent/a1".to_string(), br#"{"id":"a1","provider":"claude"}"#.to_vec()),
            ("timeline/t1".to_string(), b"row\n".to_vec()),
            ("timeline/t2".to_string(), b"overwritten\n".to_vec()),
            ("timeline/../evil".to_string(), b"x".to_vec()),
        ]);
        let done = write_restored(&home, &items).unwrap();
        assert_eq!((done.agents, done.timelines, done.projects), (1, 1, 2));
        assert_eq!(done.clones, vec![(missing.to_string_lossy().into_owned(), "git@h:o/r.git".to_string())]);
        assert_eq!(std::fs::read_to_string(home.join("timelines/t1.jsonl")).unwrap(), "row\n");
        assert_eq!(std::fs::read_to_string(home.join("timelines/t2.jsonl")).unwrap(), "keep\n");
        let config: Value = serde_json::from_slice(&std::fs::read(home.join("daemon-config.json")).unwrap()).unwrap();
        assert_eq!(config["appendSystemPrompt"], "local");
        assert!(config["providers"].is_object() && config["backup"].is_object());
        let projects = std::fs::read_to_string(home.join("projects.json")).unwrap();
        assert!(!projects.contains("remote"));
        assert!(has_local_state(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pipeline_request_and_response() {
        let body = pipeline_body(&[Stmt { sql: UPSERT, args: vec![text("k"), int(5), text("d")] }]);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["requests"][0]["type"], "execute");
        assert_eq!(v["requests"][0]["stmt"]["args"][1], json!({ "type": "integer", "value": "5" }));
        assert_eq!(v["requests"][0]["stmt"]["args"][2], json!({ "type": "text", "value": "d" }));
        assert_eq!(v["requests"][1], json!({ "type": "close" }));

        let ok = r#"{"baton":null,"results":[{"type":"ok","response":{"type":"execute","result":{"cols":[],"rows":[[{"type":"text","value":"a"},{"type":"null"}]]}}},{"type":"ok","response":{"type":"close"}}]}"#;
        assert_eq!(parse_pipeline(ok, 1).unwrap(), vec![vec![vec!["a".to_string(), String::new()]]]);
        let failed = r#"{"results":[{"type":"ok","response":{"type":"execute","result":{"rows":[]}}},{"type":"error","error":{"message":"no such table","code":"SQL"}},{"type":"ok","response":{"type":"close"}}]}"#;
        assert!(parse_pipeline(failed, 2).unwrap_err().contains("statement 1 failed: no such table"));
        assert!(parse_pipeline(r#"{"results":[]}"#, 1).is_err());
        assert!(parse_pipeline("nope", 1).is_err());
    }

    #[test]
    fn config_needs_url_token_and_passphrase() {
        let section = json!({ "tursoUrl": "libsql://db-org.turso.io", "tursoToken": "tok", "intervalSecs": 5 });
        assert!(BackupConfig::from_value(Some(&section), None).ready().unwrap().is_none());
        let cfg = BackupConfig::from_value(Some(&section), Some("pw"));
        assert_eq!(cfg.ready().unwrap().unwrap().turso.host, "db-org.turso.io");
        assert_eq!(cfg.interval, Duration::from_secs(60), "floor");
        assert_eq!(BackupConfig::from_value(None, None).interval, Duration::from_secs(900));
        let shown = cfg.redacted().to_string();
        assert!(shown.contains("\"tokenSet\":true") && !shown.contains("tok\"") && !shown.contains("pw"));
        assert_eq!(host_of("https://h.turso.io/").unwrap(), "h.turso.io");
        assert!(host_of("http://h.turso.io").is_err() && host_of("libsql://").is_err());
    }

    fn sealed_bytes(sealed: &str) -> Vec<u8> {
        STANDARD.decode(sealed).unwrap()
    }

    #[test]
    fn tampered_or_truncated_rows_are_rejected() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let sealed = seal(&key, "x", b"payload payload payload").unwrap();
        let raw = sealed_bytes(&sealed);
        for at in [0, 23, 24, raw.len() - 1] {
            let mut bad = raw.clone();
            bad[at] ^= 0x01;
            assert!(open(&key, "x", &STANDARD.encode(bad)).is_err(), "flip at byte {at}");
        }
        assert_eq!(open(&key, "x", &STANDARD.encode(&raw[..raw.len() - 1])).unwrap_err(), "decryption failed");
        assert_eq!(open(&key, "x", &STANDARD.encode(&raw[..23])).unwrap_err(), "row too short");
        assert!(open(&key, "x", "").is_err());
        assert!(open(&key, "x", "not base64!").is_err());
    }

    #[test]
    fn nonces_differ_between_encryptions_of_the_same_data() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let nonces: BTreeSet<Vec<u8>> = (0..8).map(|_| sealed_bytes(&seal(&key, "x", b"same").unwrap())[..24].to_vec()).collect();
        assert_eq!(nonces.len(), 8);
    }

    #[test]
    fn bad_meta_rows_are_rejected_before_any_write() {
        let (meta, _) = make_meta("pw", &FAST).unwrap();
        let edit = |f: &dyn Fn(&mut Value)| {
            let mut v: Value = serde_json::from_str(&meta).unwrap();
            f(&mut v);
            v.to_string()
        };
        assert!(unlock(&edit(&|v| v["m"] = json!(MAX_KDF_MEMORY_KIB + 1)), "pw").unwrap_err().contains("too much KDF memory"));
        assert!(unlock(&edit(&|v| v["v"] = json!(2)), "pw").unwrap_err().contains("unsupported backup format"));
        assert!(unlock(&edit(&|v| v["kdf"] = json!("scrypt")), "pw").unwrap_err().contains("unsupported backup format"));
        assert!(unlock(&edit(&|v| v["salt"] = json!("***")), "pw").unwrap_err().contains("bad salt"));
        assert_eq!(unlock(&edit(&|v| v["verifier"] = json!("AAAA")), "pw").unwrap_err(), "wrong backup passphrase");
        assert!(unlock("{not json", "pw").unwrap_err().contains("unreadable backup meta row"));
        let (other, _) = make_meta("pw", &FAST).unwrap();
        let swapped = edit(&|v| v["verifier"] = serde_json::from_str::<Value>(&other).unwrap()["verifier"].clone());
        assert_eq!(unlock(&swapped, "pw").unwrap_err(), "wrong backup passphrase", "a verifier sealed under another salt");
    }

    #[test]
    fn chunking_at_the_size_boundary() {
        assert_eq!(split_chunks("x", String::new()), vec![("x".to_string(), String::new())]);
        let exact = "a".repeat(CHUNK_BYTES);
        assert_eq!(split_chunks("x", exact.clone()), vec![("x".to_string(), exact.clone())]);
        let over = format!("{exact}b");
        let rows = split_chunks("x", over.clone());
        assert_eq!(rows.iter().map(|(k, d)| (k.as_str(), d.len())).collect::<Vec<_>>(), [("x#0", CHUNK_BYTES), ("x#1", 1)]);
        let two = "c".repeat(CHUNK_BYTES * 2);
        assert_eq!(split_chunks("x", two.clone()).len(), 2);
        for text in [String::new(), exact, over, two] {
            let joined = join_chunks(split_chunks("x", text.clone()).into_iter().collect()).unwrap();
            assert_eq!(joined["x"], text);
        }
    }

    #[test]
    fn chunks_sort_numerically_when_there_are_more_than_ten() {
        let text: String = (0..CHUNK_BYTES * 11 + 5).map(|i| (b'a' + (i % 7) as u8) as char).collect();
        let rows = split_chunks("timeline/t", text.clone());
        assert_eq!(rows.len(), 12);
        assert_eq!(join_chunks(rows.into_iter().collect()).unwrap()["timeline/t"], text);
    }

    #[test]
    fn a_deleted_item_becomes_a_remote_delete_and_the_meta_row_is_kept() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let remote = BTreeSet::from([META.to_string(), "agent/a".to_string(), "timeline/t#0".to_string(), "timeline/t#1".to_string()]);
        let state = State::new(BTreeMap::from([("agent/a".to_string(), 1), ("timeline/t".to_string(), 2)]), remote);
        let changes = state.plan(&key, &BTreeMap::new()).unwrap();
        let mut deleted: Vec<String> = changes.iter().flat_map(|c| c.delete.clone()).collect();
        deleted.sort();
        assert_eq!(deleted, ["agent/a", "timeline/t#0", "timeline/t#1"]);
        assert!(changes.iter().all(|c| c.put.is_empty() && c.hash.is_none()));
        let sql: Vec<&str> = changes.iter().flat_map(|c| c.statements(0)).map(|s| s.sql).collect();
        assert!(sql.iter().all(|s| *s == DELETE));
    }

    #[test]
    fn a_file_that_vanished_since_listing_is_skipped() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let state = State::new(BTreeMap::new(), BTreeSet::from([META.to_string()]));
        let items = BTreeMap::from([("timeline/t".to_string(), Item::File(temp_home().join("missing.jsonl")))]);
        assert!(state.plan(&key, &items).unwrap().is_empty());
    }

    #[test]
    fn status_and_redacted_config_hold_no_secrets() {
        let section = json!({ "tursoUrl": "libsql://db-org.turso.io", "tursoToken": "TOKEN-9f3a", "passphrase": "PASS-7c1d" });
        let cfg = BackupConfig::from_value(Some(&section), Some("ENV-PASS-5b"));
        let shown = format!("{}{}", cfg.redacted(), status());
        for secret in ["TOKEN-9f3a", "PASS-7c1d", "ENV-PASS-5b", "db-org"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        let redacted = cfg.redacted();
        assert_eq!((&redacted["enabled"], &redacted["urlSet"], &redacted["tokenSet"], &redacted["passphraseSet"]), (&json!(true), &json!(true), &json!(true), &json!(true)));
        let bad_url = BackupConfig::from_value(Some(&json!({ "tursoUrl": "http://x", "tursoToken": "t", "passphrase": "p" })), None);
        assert_eq!(bad_url.redacted()["enabled"], false);
        assert!(bad_url.ready().is_err());
    }

    #[test]
    fn passphrase_in_the_file_wins_over_the_environment_and_blank_values_count_as_unset() {
        let section = json!({ "tursoUrl": " ", "tursoToken": "t\nx", "passphrase": "file" });
        let cfg = BackupConfig::from_value(Some(&section), Some("env"));
        assert_eq!(cfg.passphrase.as_deref(), Some("file"));
        assert!(cfg.url.is_none() && cfg.token.is_none());
        assert!(BackupConfig::from_value(Some(&json!({ "passphrase": "" })), Some("  ")).passphrase.is_none());
    }

    #[test]
    fn config_row_drops_secret_names_but_keeps_token_limits() {
        let row = config_row(br#"{"maxTokens":5,"tokensPerMin":6,"authToken":"a","Password":"x","nested":{"PASSPHRASE":"y","ok":true,"private_key":"k"}}"#).unwrap();
        let kept: Value = serde_json::from_slice(&row).unwrap();
        assert_eq!(kept, json!({ "maxTokens": 5, "tokensPerMin": 6, "nested": { "ok": true } }), "key matching is case-insensitive");
        assert!(config_row(b"[1]").is_none());
        assert!(config_row(b"{broken").is_none());
    }

    /// An in-memory stand-in for the Turso table: runs the statements a cycle sends.
    fn apply(store: &mut BTreeMap<String, String>, stmts: Vec<Stmt>) {
        let key = |s: &Stmt| s.args[0]["value"].as_str().unwrap().to_string();
        for s in stmts {
            match s.sql {
                UPSERT => {
                    store.insert(key(&s), s.args[2]["value"].as_str().unwrap().to_string());
                }
                DELETE => {
                    store.remove(&key(&s));
                }
                other => panic!("unexpected statement {other}"),
            }
        }
    }

    fn cycle(state: &mut State, key: &SecretKey, store: &mut BTreeMap<String, String>, home: &Path, projects: &[Value]) -> usize {
        let changes = state.plan(key, &items(home, projects).unwrap()).unwrap();
        let rows = changes.iter().map(|c| c.put.len() + c.delete.len()).sum();
        for c in &changes {
            apply(store, c.statements(0));
            state.commit(c);
        }
        rows
    }

    fn restore_from(store: &BTreeMap<String, String>, key: &SecretKey, home: &Path) -> Restored {
        let rows = store.iter().filter(|(k, _)| k.as_str() != META).map(|(k, d)| (k.clone(), d.clone())).collect();
        let opened = join_chunks(rows).unwrap().into_iter().map(|(item, sealed)| { let plain = open(key, &item, &sealed).unwrap(); (item, plain) }).collect();
        write_restored(home, &opened).unwrap()
    }

    #[test]
    fn full_backup_then_restore_into_an_empty_home_matches_the_original_minus_secrets() {
        let home = temp_home();
        let project_dir = temp_home();
        let config = json!({ "appendSystemPrompt": "be brief", "providers": { "claude": { "enabled": false, "apiKey": "K-1" } }, "password": "P-1",
            "backup": { "tursoUrl": "libsql://db.turso.io", "tursoToken": "T-1", "passphrase": "PASS-1" } });
        std::fs::write(home.join("daemon-config.json"), config.to_string()).unwrap();
        std::fs::write(home.join("daemon-keypair.json"), r#"{"secretKey":"KEYPAIR-1"}"#).unwrap();
        std::fs::write(home.join("server-id"), "SERVER-ID-1").unwrap();
        std::fs::write(home.join("backup-state.json"), "{}").unwrap();
        std::fs::write(home.join("workspaces.json"), r#"[{"id":"w1"}]"#).unwrap();
        let agents = json!([{ "id": "a1", "provider": "claude", "title": "one" }, { "id": "a2", "provider": "codex" }]);
        std::fs::write(home.join("agents.json"), agents.to_string()).unwrap();
        std::fs::create_dir_all(home.join("timelines")).unwrap();
        let mut seed = 0x2545F491u32;
        let big: Vec<u8> = (0..600_000).map(|_| { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); (seed >> 24) as u8 }).collect();
        std::fs::write(home.join("timelines/a1.jsonl"), &big).unwrap();
        std::fs::write(home.join("timelines/a2.jsonl"), "{\"n\":1}\n").unwrap();
        std::fs::write(home.join("timelines/notes.txt"), "not a timeline").unwrap();
        let gone = home.join("elsewhere");
        let projects = [json!({ "rootPath": project_dir.to_string_lossy(), "name": "here" }), json!({ "rootPath": gone.to_string_lossy(), "remote": "git@h:o/r.git" })];
        std::fs::write(home.join("projects.json"), json!(projects).to_string()).unwrap();

        let (meta, key) = make_meta("PASS-1", &FAST).unwrap();
        let mut store = BTreeMap::from([(META.to_string(), meta.clone())]);
        let mut state = State::new(BTreeMap::new(), BTreeSet::from([META.to_string()]));
        let written = cycle(&mut state, &key, &mut store, &home, &projects);
        assert!(written > 0);
        assert!(store.contains_key("timeline/a1#0") && store.contains_key("timeline/a1#1"), "600 KB of noise needs two chunks: {:?}", store.keys().collect::<Vec<_>>());
        let everything = format!("{}{}", store.keys().cloned().collect::<String>(), store.values().cloned().collect::<String>());
        for plain in ["be brief", "K-1", "P-1", "T-1", "PASS-1", "KEYPAIR-1", "SERVER-ID-1"] {
            assert!(!everything.contains(plain), "{plain} readable in the remote rows");
        }
        assert_eq!(unlock(&meta, "PASS-1").unwrap(), key);
        assert_eq!(cycle(&mut state, &key, &mut store, &home, &projects), 0, "nothing changed");

        let fresh = temp_home();
        assert!(!has_local_state(&fresh));
        let done = restore_from(&store, &key, &fresh);
        assert_eq!((done.agents, done.timelines, done.projects), (2, 2, 2));
        assert_eq!(done.clones, vec![(gone.to_string_lossy().into_owned(), "git@h:o/r.git".to_string())]);
        assert!(has_local_state(&fresh));
        assert_eq!(std::fs::read(fresh.join("timelines/a1.jsonl")).unwrap(), big);
        assert_eq!(std::fs::read(fresh.join("timelines/a2.jsonl")).unwrap(), b"{\"n\":1}\n");
        assert!(!fresh.join("timelines/notes.txt").exists());
        let read = |name: &str| serde_json::from_slice::<Value>(&std::fs::read(fresh.join(name)).unwrap()).unwrap();
        assert_eq!(read("agents.json"), agents);
        assert_eq!(read("workspaces.json"), json!([{ "id": "w1" }]));
        assert_eq!(read("daemon-config.json"), json!({ "appendSystemPrompt": "be brief", "providers": { "claude": { "enabled": false } } }));
        assert_eq!(read("projects.json"), json!([{ "rootPath": project_dir.to_string_lossy(), "name": "here" }, { "rootPath": gone.to_string_lossy() }]));
        for secret_file in ["daemon-keypair.json", "server-id"] {
            assert!(!fresh.join(secret_file).exists(), "{secret_file} must not be restored");
        }
        let restored_state = load_state_file(&fresh);
        assert_eq!(restored_state.remotes, BTreeMap::from([(gone.to_string_lossy().into_owned(), "git@h:o/r.git".to_string())]), "the origin of a project still to clone is kept");
        assert!(restored_state.hashes.is_empty() && restored_state.salt.is_none());

        std::fs::remove_file(home.join("timelines/a1.jsonl")).unwrap();
        let rows = cycle(&mut state, &key, &mut store, &home, &projects);
        assert_eq!(rows, 2);
        assert!(!store.keys().any(|k| k.starts_with("timeline/a1")) && store.contains_key("timeline/a2"));
        for dir in [&home, &project_dir, &fresh] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn restore_skips_unsafe_timeline_ids_and_rejects_a_corrupt_config_row() {
        let home = temp_home();
        let items = BTreeMap::from([("timeline/a b".to_string(), b"x".to_vec()), ("timeline/ok-1".to_string(), b"y".to_vec())]);
        let done = write_restored(&home, &items).unwrap();
        assert_eq!(done.timelines, 1);
        assert_eq!(std::fs::read_dir(home.join("timelines")).unwrap().count(), 1);
        let bad = BTreeMap::from([("config".to_string(), b"[1]".to_vec())]);
        assert!(write_restored(&home, &bad).unwrap_err().starts_with("config row"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pipeline_errors_and_odd_responses() {
        let rows = r#"{"results":[{"type":"ok","response":{"type":"execute","result":{"rows":[[{"type":"integer","value":"7"},{"type":"text","value":"é"}]]}}},{"type":"ok","response":{"type":"close"}}]}"#;
        assert_eq!(parse_pipeline(rows, 1).unwrap(), vec![vec![vec!["7".to_string(), "é".to_string()]]]);
        let auth = r#"{"error":"Unauthorized"}"#;
        assert!(parse_pipeline(auth, 1).is_err());
        let no_message = r#"{"results":[{"type":"error"}]}"#;
        assert!(parse_pipeline(no_message, 1).unwrap_err().contains("unknown error"));
        let first_error = r#"{"results":[{"type":"error","error":{"message":"first"}},{"type":"error","error":{"message":"second"}}]}"#;
        assert!(parse_pipeline(first_error, 2).unwrap_err().contains("first"));
        assert_eq!(parse_pipeline(r#"{"results":[{"type":"ok","response":{"type":"execute","result":{"rows":[]}}}]}"#, 1).unwrap(), vec![Rows::new()]);
        let body = pipeline_body(&[]);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["requests"], json!([{ "type": "close" }]));
    }

    #[test]
    fn pipeline_body_escapes_data_so_it_cannot_inject_statements() {
        let evil = "x\"}],\"requests\":[{\"type\":\"close\"}";
        let body = pipeline_body(&[Stmt { sql: DELETE, args: vec![text(evil)] }]);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["requests"].as_array().unwrap().len(), 2);
        assert_eq!(v["requests"][0]["stmt"]["args"][0]["value"], evil);
    }

    #[test]
    fn host_of_rejects_credentials_ports_and_other_schemes() {
        for bad in ["http://h", "libsql://", "libsql://u:p@h.turso.io", "libsql://h:8080", "libsql://a b", "ftp://h", "h.turso.io", "https:///path"] {
            assert!(host_of(bad).is_err(), "{bad}");
        }
        assert_eq!(host_of("libsql://h.turso.io/v2/pipeline").unwrap(), "h.turso.io");
    }

    #[test]
    fn a_row_moved_to_another_key_is_rejected() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let sealed = seal(&key, "agent/a", b"{}").unwrap();
        assert_eq!(open(&key, "agent/a", &sealed).unwrap(), b"{}");
        assert_eq!(open(&key, "agent/b", &sealed).unwrap_err(), "row belongs to another key");
        assert!(open(&key, "", &sealed).is_err());
        let big_name = "x".repeat(70_000);
        assert!(open(&key, &big_name, &sealed).is_err());
    }

    #[test]
    fn meta_kdf_cost_is_capped() {
        let (meta, _) = make_meta("pw", &FAST).unwrap();
        let edit = |field: &str, n: u32| {
            let mut v: Value = serde_json::from_str(&meta).unwrap();
            v[field] = json!(n);
            v.to_string()
        };
        assert!(unlock(&edit("t", MAX_KDF_PASSES + 1), "pw").unwrap_err().contains("too much KDF work"));
        assert!(unlock(&edit("p", MAX_KDF_LANES + 1), "pw").unwrap_err().contains("too much KDF work"));
        assert!(unlock(&edit("m", 64 * 1024 + 1), "pw").unwrap_err().contains("too much KDF memory"));
    }

    #[test]
    fn an_empty_machine_never_replaces_a_remote_backup() {
        let home = temp_home();
        let with_data = BTreeSet::from([META.to_string(), "agent/a".to_string()]);
        let only_meta = BTreeSet::from([META.to_string()]);
        let mut owns = false;
        assert!(check_may_write(&home, &mut owns, &with_data).unwrap_err().contains("remote backup has data"));
        assert!(!owns);
        std::fs::write(home.join(RESTORE_PENDING), "").unwrap();
        assert!(check_may_write(&home, &mut owns, &only_meta).unwrap_err().contains("restore failed earlier"));
        std::fs::remove_file(home.join(RESTORE_PENDING)).unwrap();
        assert!(check_may_write(&home, &mut owns, &only_meta).is_ok(), "nothing remote to lose");
        assert!(owns);
        assert!(check_may_write(&home, &mut owns, &with_data).is_ok(), "once owned, deleting everything locally is a deletion");

        let mut owns = false;
        std::fs::write(home.join("agents.json"), r#"[{"id":"a"}]"#).unwrap();
        assert!(check_may_write(&home, &mut owns, &with_data).is_ok(), "a cycle that starts with local state may write");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invalid_items_fail_before_anything_is_written() {
        let home = temp_home();
        let bad_agent = BTreeMap::from([("timeline/t1".to_string(), b"row\n".to_vec()), ("agent/a1".to_string(), br#"{"id":"other"}"#.to_vec())]);
        assert!(write_restored(&home, &bad_agent).unwrap_err().contains("not an agent record"));
        let bad_list = BTreeMap::from([("timeline/t1".to_string(), b"row\n".to_vec()), ("workspaces".to_string(), b"[1]".to_vec())]);
        assert!(write_restored(&home, &bad_list).unwrap_err().contains("workspaces row"));
        let no_root = BTreeMap::from([("projects".to_string(), br#"[{"name":"x"}]"#.to_vec())]);
        assert!(write_restored(&home, &no_root).unwrap_err().contains("rootPath"));
        assert!(!home.join("timelines").exists() || std::fs::read_dir(home.join("timelines")).unwrap().count() == 0);
        assert!(!has_local_state(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn restored_agents_lose_pending_prompts_and_remotes_lose_credentials() {
        let home = temp_home();
        let missing = home.join("not-cloned");
        let items = BTreeMap::from([
            ("agent/a1".to_string(), br#"{"id":"a1","pendingTurn":"rm -rf","pendingQueue":["x"],"model":"opus"}"#.to_vec()),
            ("projects".to_string(), json!([{ "rootPath": missing.to_string_lossy(), "remote": "https://user:tok-123@github.com/o/r.git" }]).to_string().into_bytes()),
        ]);
        let done = write_restored(&home, &items).unwrap();
        let agents: Value = serde_json::from_slice(&std::fs::read(home.join("agents.json")).unwrap()).unwrap();
        assert_eq!(agents, json!([{ "id": "a1", "model": "opus" }]));
        assert_eq!(done.clones, vec![(missing.to_string_lossy().into_owned(), "https://github.com/o/r.git".to_string())]);
        assert!(!std::fs::read_to_string(state_path(&home)).unwrap().contains("tok-123"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn debug_output_of_the_config_hides_the_secrets() {
        let section = json!({ "tursoUrl": "libsql://db-org.turso.io", "tursoToken": "TOKEN-9f3a", "passphrase": "PASS-7c1d" });
        let shown = format!("{:?}", BackupConfig::from_value(Some(&section), None));
        assert!(!shown.contains("TOKEN-9f3a") && !shown.contains("PASS-7c1d") && !shown.contains("db-org"), "{shown}");
    }

    #[test]
    fn the_state_file_round_trips_and_defaults() {
        let home = temp_home();
        assert!(load_state_file(&home).hashes.is_empty());
        let file = StateFile { salt: Some("s".into()), hashes: BTreeMap::from([("config".to_string(), 7)]), remotes: BTreeMap::from([("/p".to_string(), "u".to_string())]) };
        save_state_file(&home, &file).unwrap();
        let back = load_state_file(&home);
        assert_eq!((back.salt.as_deref(), back.hashes["config"], back.remotes["/p"].as_str()), (Some("s"), 7, "u"));
        std::fs::write(state_path(&home), r#"{"hashes":{"a":1}}"#).unwrap();
        assert!(load_state_file(&home).remotes.is_empty(), "an older file without the new fields still loads");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_failed_write_removes_the_files_that_mark_the_machine_as_having_state() {
        let home = temp_home();
        std::fs::create_dir(home.join("agents.json")).unwrap();
        let items = BTreeMap::from([
            ("workspaces".to_string(), br#"[{"id":"w1"}]"#.to_vec()),
            ("projects".to_string(), br#"[{"rootPath":"/nowhere-p"}]"#.to_vec()),
            ("agent/a1".to_string(), br#"{"id":"a1"}"#.to_vec()),
        ]);
        assert!(write_restored(&home, &items).unwrap_err().starts_with("agents.json"));
        assert!(!home.join("workspaces.json").exists() && !home.join("projects.json").exists(), "a half restore must not count as local state");
        std::fs::remove_dir(home.join("agents.json")).unwrap();
        assert!(!has_local_state(&home));
        assert!(write_restored(&home, &items).is_ok(), "the restore can run again");
        assert!(has_local_state(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn restored_remotes_are_stored_for_projects_whose_directory_exists_too() {
        let home = temp_home();
        let items = BTreeMap::from([("projects".to_string(), json!([{ "rootPath": home.to_string_lossy(), "remote": "git@h:o/r.git" }]).to_string().into_bytes())]);
        let done = write_restored(&home, &items).unwrap();
        assert!(done.clones.is_empty(), "nothing to clone when the directory exists");
        assert_eq!(load_state_file(&home).remotes[&*home.to_string_lossy()], "git@h:o/r.git");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_project_keeps_its_known_origin_while_its_directory_is_missing_or_not_a_repository() {
        let home = temp_home();
        let not_a_repo = temp_home();
        let missing = home.join("not-cloned-yet");
        let (m, n) = (missing.to_string_lossy().into_owned(), not_a_repo.to_string_lossy().into_owned());
        std::fs::write(home.join("projects.json"), json!([{ "rootPath": m }, { "rootPath": n }, { "rootPath": "/unknown-dir-for-backup-test" }]).to_string()).unwrap();
        let known = BTreeMap::from([(m.clone(), "https://h/o/a.git".to_string()), (n.clone(), "https://h/o/b.git".to_string())]);
        let (items, remotes) = gather(&home, &known).await.unwrap();
        assert_eq!(remotes, known, "a project with no known origin gets none");
        let Some(Item::Bytes(projects)) = items.get("projects") else { panic!("projects item") };
        let projects: Value = serde_json::from_slice(projects).unwrap();
        assert_eq!((projects[0]["remote"].as_str(), projects[1]["remote"].as_str()), (Some("https://h/o/a.git"), Some("https://h/o/b.git")));
        assert!(projects[2].get("remote").is_none());
        let (_, forgotten) = gather(&home, &BTreeMap::new()).await.unwrap();
        assert!(forgotten.is_empty());
        for dir in [&home, &not_a_repo] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_failed_restore_leaves_the_remote_untouched_until_it_is_fixed() {
        let source = temp_home();
        std::fs::write(source.join("agents.json"), r#"[{"id":"a1","provider":"claude"}]"#).unwrap();
        std::fs::create_dir_all(source.join("timelines")).unwrap();
        std::fs::write(source.join("timelines/a1.jsonl"), "turn\n").unwrap();
        let projects = [json!({ "rootPath": "/nowhere-p", "name": "p" })];
        let (meta, key) = make_meta("pw", &FAST).unwrap();
        let mut store = BTreeMap::from([(META.to_string(), meta)]);
        let mut owner = State::new(BTreeMap::new(), BTreeSet::from([META.to_string()]));
        assert!(cycle(&mut owner, &key, &mut store, &source, &projects) > 0);
        let before = store.clone();

        // The restore fails: a row sealed under the right key but not an agent record.
        let broken = BTreeMap::from([("agent/a1".to_string(), b"[1]".to_vec())]);
        let fresh = temp_home();
        assert!(write_restored(&fresh, &broken).is_err());
        assert!(!has_local_state(&fresh));
        std::fs::write(fresh.join(RESTORE_PENDING), "").unwrap();

        let remote_keys = |store: &BTreeMap<String, String>| store.keys().cloned().collect::<BTreeSet<_>>();
        for owns in [false, true] {
            let mut owns_remote = owns;
            let paused = check_may_write(&fresh, &mut owns_remote, &remote_keys(&store));
            assert!(paused.is_err(), "owns_remote={owns} must not matter while the marker exists");
            assert_eq!(owns_remote, owns);
        }
        assert_eq!(store, before, "the paused loop wrote and deleted nothing");

        // Marker gone (write failed, or deleted) but still no local state: still paused.
        std::fs::remove_file(fresh.join(RESTORE_PENDING)).unwrap();
        let mut owns_remote = false;
        assert!(check_may_write(&fresh, &mut owns_remote, &remote_keys(&store)).is_err());

        // The cause is fixed and the restore succeeds: the loop may run, and an unchanged machine writes nothing.
        let opened: BTreeMap<String, Vec<u8>> = join_chunks(store.iter().filter(|(k, _)| k.as_str() != META).map(|(k, d)| (k.clone(), d.clone())).collect())
            .unwrap()
            .into_iter()
            .map(|(item, sealed)| { let plain = open(&key, &item, &sealed).unwrap(); (item, plain) })
            .collect();
        write_restored(&fresh, &opened).unwrap();
        assert!(check_may_write(&fresh, &mut owns_remote, &remote_keys(&store)).is_ok());
        assert!(owns_remote);
        let mut restored_state = State::new(BTreeMap::new(), remote_keys(&store));
        cycle(&mut restored_state, &key, &mut store, &fresh, &projects);
        assert_eq!(remote_keys(&store), remote_keys(&before), "the restored machine neither adds nor deletes rows");
        for dir in [&source, &fresh] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn the_first_backup_into_an_empty_remote_proceeds_from_an_empty_machine() {
        let home = temp_home();
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let mut store = BTreeMap::new();
        let remote = BTreeSet::from([META.to_string()]);
        let mut owns = false;
        check_may_write(&home, &mut owns, &remote).unwrap();
        assert!(owns);
        let mut state = State::new(BTreeMap::new(), remote);
        assert!(cycle(&mut state, &key, &mut store, &home, &[]) > 0);
        assert!(store.contains_key("projects"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_items_this_machine_uploaded_are_deleted_remotely() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let remote = BTreeSet::from([META.to_string(), "agent/mine".to_string(), "agent/theirs".to_string(), "timeline/theirs#0".to_string(), "timeline/theirs#1".to_string()]);
        let state = State::new(BTreeMap::from([("agent/mine".to_string(), 1)]), remote);
        let changes = state.plan(&key, &BTreeMap::new()).unwrap();
        assert_eq!(changes.iter().map(|c| c.item.as_str()).collect::<Vec<_>>(), ["agent/mine"]);
        assert_eq!(changes[0].delete, ["agent/mine"]);
    }

    #[test]
    fn a_stale_restore_marker_goes_away_once_the_machine_has_state() {
        let home = temp_home();
        let marker = home.join(RESTORE_PENDING);
        std::fs::write(&marker, "").unwrap();
        assert!(!has_state_clearing_marker(&home));
        assert!(marker.exists(), "an empty machine keeps the marker");
        std::fs::write(home.join("agents.json"), r#"[{"id":"a"}]"#).unwrap();
        assert!(has_state_clearing_marker(&home));
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Run the streaming restore against rows held in memory instead of Turso.
    fn stream(home: &Path, key: &SecretKey, store: &BTreeMap<String, String>, mut after_group: impl FnMut(&Group)) -> Result<Restored, String> {
        let (mut small, mut timelines) = (BTreeMap::new(), 0);
        for group in item_groups(&store.keys().cloned().collect()) {
            let rows = group.keys.iter().map(|k| store[k].clone()).collect();
            absorb(home, key, &group, rows, &mut small, &mut timelines)?;
            after_group(&group);
        }
        let mut done = write_restored(home, &small)?;
        done.timelines += timelines;
        Ok(done)
    }

    fn rows_of(key: &SecretKey, item: &str, plain: &[u8]) -> Vec<(String, String)> {
        split_chunks(item, seal(key, item, plain).unwrap())
    }

    #[test]
    fn timelines_are_written_one_item_at_a_time_and_a_bad_state_item_leaves_no_state_files() {
        let (meta, key) = make_meta("pw", &FAST).unwrap();
        let big = vec![b'x'; 3 * CHUNK_BYTES];
        let mut seed = 7u32;
        let noise: Vec<u8> = big.iter().map(|_| { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); (seed >> 24) as u8 }).collect();
        let mut store: BTreeMap<String, String> = BTreeMap::from([(META.to_string(), meta)]);
        for (item, plain) in [("timeline/t1", noise.as_slice()), ("timeline/t2", b"two\n"), ("projects", br#"[{"rootPath":"/p"}]"#), ("agent/a1", br#"{"id":"someone-else"}"#)] {
            store.extend(rows_of(&key, item, plain));
        }
        assert!(store.contains_key("timeline/t1#0"), "the big timeline is chunked");

        let home = temp_home();
        let mut written = Vec::new();
        let err = stream(&home, &key, &store, |g| written.push((g.item.clone(), home.join("timelines/t1.jsonl").exists(), home.join("projects.json").exists()))).unwrap_err();
        assert!(err.contains("not an agent record"), "{err}");
        assert_eq!(std::fs::read(home.join("timelines/t1.jsonl")).unwrap(), noise, "written as soon as it was decrypted");
        assert_eq!(std::fs::read(home.join("timelines/t2.jsonl")).unwrap(), b"two\n");
        assert!(written.iter().all(|(_, _, projects_written)| !projects_written), "state files come last");
        assert!(written.iter().any(|(item, t1, _)| item == "timeline/t1" && *t1), "t1 existed before the next group was read");
        for state_file in ["agents.json", "projects.json", "workspaces.json"] {
            assert!(!home.join(state_file).exists(), "{state_file}");
        }
        assert!(!has_local_state(&home), "a failed restore can run again");

        store.extend(rows_of(&key, "agent/a1", br#"{"id":"a1"}"#));
        let done = stream(&home, &key, &store, |_| {}).unwrap();
        assert_eq!((done.agents, done.projects, done.timelines), (1, 1, 0), "timelines already on disk are kept");
        assert!(has_local_state(&home));
        let _ = std::fs::remove_dir_all(&home);
    }
}
