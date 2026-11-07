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
//! under an Argon2id key, as base64(nonce || ciphertext); a row over
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
/// Rows of one pipeline request stop growing once they pass this many bytes.
const BATCH_BYTES: usize = 2 * 1024 * 1024;
const PAGE_ROWS: usize = 10;
const META: &str = "meta";
const VERIFIER: &[u8] = b"feiyi-backup-v1";
const DEFAULT_INTERVAL_SECS: u64 = 900;
const START_DELAY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE: u64 = 64 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);
/// Field names (lowercase, substring match) dropped from the config row.
const SECRET_WORDS: [&str; 6] = ["password", "passphrase", "secret", "token", "apikey", "api_key"];

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS feiyi_backup(key TEXT PRIMARY KEY, updated_at INTEGER NOT NULL, data TEXT NOT NULL)";
const UPSERT: &str = "INSERT INTO feiyi_backup(key, updated_at, data) VALUES(?, ?, ?) ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at, data = excluded.data";
const INSERT_META: &str = "INSERT INTO feiyi_backup(key, updated_at, data) VALUES(?, ?, ?) ON CONFLICT(key) DO NOTHING";
const DELETE: &str = "DELETE FROM feiyi_backup WHERE key = ?";
const SELECT_META: &str = "SELECT data FROM feiyi_backup WHERE key = 'meta'";
const SELECT_KEYS: &str = "SELECT key FROM feiyi_backup";
const SELECT_PAGE: &str = "SELECT key, data FROM feiyi_backup WHERE key > ? AND key != 'meta' ORDER BY key LIMIT 10";

type SecretKey = [u8; 32];
type Rows = Vec<Vec<String>>;

#[derive(Clone, Debug, Default)]
pub struct BackupConfig {
    url: Option<String>,
    token: Option<String>,
    passphrase: Option<String>,
    interval: Duration,
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
        BackupConfig::from_value(file.as_ref().and_then(|v| v.get("backup")), std::env::var("FEIYI_BACKUP_PASSPHRASE").ok().as_deref())
    }

    fn from_value(section: Option<&Value>, env_passphrase: Option<&str>) -> BackupConfig {
        let text = |key: &str| clean(section.and_then(|s| s.get(key)).and_then(Value::as_str));
        BackupConfig {
            url: text("tursoUrl"),
            token: text("tursoToken"),
            passphrase: text("passphrase").or_else(|| clean(env_passphrase)),
            interval: Duration::from_secs(section.and_then(|s| s.get("intervalSecs")).and_then(Value::as_u64).unwrap_or(DEFAULT_INTERVAL_SECS).max(60)),
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
/// Parameters read from the remote meta row are capped so a bad row cannot exhaust memory.
const MAX_KDF_MEMORY_KIB: u32 = 256 * 1024;

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
    let meta = Meta { v: 1, kdf: "argon2id".into(), salt: STANDARD.encode(salt), m: kdf.m, t: kdf.t, p: kdf.p, verifier: seal(&key, VERIFIER)? };
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
    match open(&key, &meta.verifier) {
        Ok(v) if v == VERIFIER => Ok(key),
        _ => Err("wrong backup passphrase".into()),
    }
}

fn seal(key: &SecretKey, plain: &[u8]) -> Result<String, String> {
    let mut nonce = [0u8; 24];
    random_fill(&mut nonce);
    let packed = miniz_oxide::deflate::compress_to_vec(plain, 6);
    let sealed = XSalsa20Poly1305::new(Key::from_slice(key)).encrypt(Nonce::from_slice(&nonce), packed.as_slice()).map_err(|_| "encryption failed".to_string())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(STANDARD.encode(out))
}

fn open(key: &SecretKey, data: &str) -> Result<Vec<u8>, String> {
    let raw = STANDARD.decode(data).map_err(|e| format!("bad base64: {e}"))?;
    if raw.len() < 24 {
        return Err("row too short".into());
    }
    let (nonce, sealed) = raw.split_at(24);
    let packed = XSalsa20Poly1305::new(Key::from_slice(key)).decrypt(Nonce::from_slice(nonce), sealed).map_err(|_| "decryption failed".to_string())?;
    miniz_oxide::inflate::decompress_to_vec(&packed).map_err(|e| format!("inflate failed: {e:?}"))
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
                !SECRET_WORDS.iter().any(|w| k.contains(w))
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

async fn origin_of(dir: &str) -> Option<String> {
    let out = crate::projects::run("git", &["remote", "get-url", "origin"], Some(Path::new(dir)), GIT_TIMEOUT).await.ok()?;
    clean(Some(out.as_str()))
}

async fn gather(home: &Path) -> Result<BTreeMap<String, Item>, String> {
    let mut projects = read_list(&home.join("projects.json"))?;
    for p in projects.iter_mut() {
        let Some(root) = p.get("rootPath").and_then(Value::as_str).map(String::from) else { continue };
        if let Some(remote) = origin_of(&root).await {
            p["remote"] = json!(remote);
        }
    }
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || items(&home, &projects)).await.map_err(|e| e.to_string())?
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
                if let Some(id) = name.strip_suffix(".jsonl") {
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
        hashes.retain(|item, _| remote.iter().any(|k| belongs(k, item)));
        State { hashes, remote }
    }

    fn plan(&self, key: &SecretKey, items: &BTreeMap<String, Item>) -> Result<Vec<Change>, String> {
        let mut changes = Vec::new();
        let mut present = BTreeSet::new();
        for (item, source) in items {
            let Some(bytes) = source.bytes()? else { continue };
            present.insert(item.as_str());
            let hash = fingerprint(&bytes);
            if self.hashes.get(item) == Some(&hash) {
                continue;
            }
            let put = split_chunks(item, seal(key, &bytes)?);
            let delete = self.remote.iter().filter(|k| belongs(k, item) && !put.iter().any(|(p, _)| p == *k)).cloned().collect();
            changes.push(Change { item: item.clone(), hash: Some(hash), put, delete });
        }
        let gone: BTreeSet<&str> = self.remote.iter().map(|k| item_of(k)).filter(|i| *i != META && !present.contains(i)).collect();
        for item in gone {
            changes.push(Change { item: item.to_string(), hash: None, put: Vec::new(), delete: self.remote.iter().filter(|k| belongs(k, item)).cloned().collect() });
        }
        Ok(changes)
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

struct Backup {
    home: PathBuf,
    ready: Ready,
    key: Option<SecretKey>,
    state: State,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Create the table, read the meta row and the remote keys; the key is None when no backup was ever written.
async fn open_remote(ready: &Ready) -> Result<(Option<SecretKey>, BTreeSet<String>), String> {
    let mut res = ready.turso.run(vec![Stmt { sql: CREATE_TABLE, args: vec![] }, Stmt { sql: SELECT_META, args: vec![] }, Stmt { sql: SELECT_KEYS, args: vec![] }]).await?;
    let remote = res.remove(2).into_iter().filter_map(|r| r.into_iter().next()).collect();
    let meta = res.remove(1).into_iter().next().and_then(|r| r.into_iter().next());
    let key = match meta {
        Some(meta) => {
            let pass = ready.passphrase.clone();
            Some(tokio::task::spawn_blocking(move || unlock(&meta, &pass)).await.map_err(|e| e.to_string())??)
        }
        None => None,
    };
    Ok((key, remote))
}

impl Backup {
    fn state_path(&self) -> PathBuf {
        self.home.join("backup-state.json")
    }

    async fn open(&mut self) -> Result<(), String> {
        let (key, mut remote) = open_remote(&self.ready).await?;
        let key = match key {
            Some(key) => key,
            None => {
                let pass = self.ready.passphrase.clone();
                let (meta, key) = tokio::task::spawn_blocking(move || make_meta(&pass, &KDF)).await.map_err(|e| e.to_string())??;
                self.ready.turso.run(vec![Stmt { sql: INSERT_META, args: vec![text(META), int(now_secs()), text(&meta)] }]).await?;
                remote.insert(META.to_string());
                key
            }
        };
        let hashes = std::fs::read(self.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        self.state = State::new(hashes, remote);
        self.key = Some(key);
        Ok(())
    }

    /// Upload what changed since the last cycle; the number of rows written.
    async fn cycle(&mut self) -> Result<usize, String> {
        if self.key.is_none() {
            self.open().await?;
        }
        let Some(key) = self.key else { return Ok(0) };
        let items = gather(&self.home).await?;
        let state = self.state.clone();
        let changes = tokio::task::spawn_blocking(move || state.plan(&key, &items)).await.map_err(|e| e.to_string())??;
        let (mut pushed, mut size, mut batch) = (0, 0, Vec::new());
        for change in changes {
            size += change.size();
            batch.push(change);
            if size >= BATCH_BYTES {
                pushed += self.send(std::mem::take(&mut batch)).await?;
                size = 0;
            }
        }
        Ok(pushed + self.send(batch).await?)
    }

    async fn send(&mut self, batch: Vec<Change>) -> Result<usize, String> {
        if batch.is_empty() {
            return Ok(0);
        }
        let now = now_secs();
        self.ready.turso.run(batch.iter().flat_map(|c| c.statements(now)).collect()).await?;
        for change in &batch {
            self.state.commit(change);
        }
        match serde_json::to_vec(&self.state.hashes) {
            Ok(bytes) => {
                if let Err(e) = crate::util::write_atomic(&self.state_path(), &bytes, true) {
                    crate::warn!("backup: cannot save {}: {e}", self.state_path().display());
                }
            }
            Err(e) => crate::warn!("backup: cannot encode backup state: {e}"),
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
    tokio::spawn(run(Backup { home: cfg.home.clone(), ready, key: None, state: State::default() }, cfg.backup.interval));
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
                (interval * 2u32.pow(failures.min(4))).min(MAX_BACKOFF)
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

#[derive(Default, Debug)]
struct Restored {
    agents: usize,
    timelines: usize,
    projects: usize,
    /// Projects whose directory is missing: (path, git remote).
    clones: Vec<(String, String)>,
}

/// Write decrypted items under `home`. Called only when there is no local
/// agent, project or workspace data; timelines already on disk and local
/// config keys are kept.
fn write_restored(home: &Path, items: &BTreeMap<String, Vec<u8>>) -> Result<Restored, String> {
    let write = |name: &str, bytes: &[u8]| crate::util::write_atomic(&home.join(name), bytes, true).map_err(|e| format!("{name}: {e}"));
    let mut done = Restored::default();
    for (key, bytes) in items {
        let Some(id) = key.strip_prefix("timeline/") else { continue };
        let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let path = home.join("timelines").join(format!("{safe}.jsonl"));
        if safe == id && !path.exists() {
            write(&format!("timelines/{safe}.jsonl"), bytes)?;
            done.timelines += 1;
        }
    }
    if let Some(bytes) = items.get("config") {
        let mut config: serde_json::Map<String, Value> = serde_json::from_slice(bytes).map_err(|e| format!("config row: {e}"))?;
        let local = std::fs::read(home.join("daemon-config.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Map<String, Value>>(&b).ok()).unwrap_or_default();
        config.extend(local);
        write("daemon-config.json", &serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?)?;
    }
    if let Some(bytes) = items.get("workspaces") {
        write("workspaces.json", bytes)?;
    }
    let agents: Vec<Value> = items.iter().filter(|(k, _)| k.starts_with("agent/")).filter_map(|(_, b)| serde_json::from_slice(b).ok()).collect();
    if !agents.is_empty() {
        write("agents.json", &serde_json::to_vec(&agents).map_err(|e| e.to_string())?)?;
    }
    done.agents = agents.len();
    if let Some(bytes) = items.get("projects") {
        let mut projects: Vec<Value> = serde_json::from_slice(bytes).map_err(|e| format!("projects row: {e}"))?;
        for p in projects.iter_mut() {
            let remote = p.as_object_mut().and_then(|o| o.remove("remote")).and_then(|r| r.as_str().map(String::from));
            let path = p.get("rootPath").and_then(Value::as_str).unwrap_or("");
            if let Some(remote) = remote.filter(|_| !path.is_empty() && !Path::new(path).exists()) {
                done.clones.push((path.to_string(), remote));
            }
        }
        done.projects = projects.len();
        write("projects.json", &serde_json::to_vec(&projects).map_err(|e| e.to_string())?)?;
    }
    Ok(done)
}

async fn fetch_items(ready: &Ready, key: &SecretKey) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut rows = BTreeMap::new();
    let mut after = String::new();
    loop {
        let page = ready.turso.run(vec![Stmt { sql: SELECT_PAGE, args: vec![text(&after)] }]).await?.remove(0);
        let more = page.len() == PAGE_ROWS;
        for row in page {
            let [k, d] = <[String; 2]>::try_from(row).map_err(|_| "unexpected row shape".to_string())?;
            after = k.clone();
            rows.insert(k, d);
        }
        if !more {
            break;
        }
    }
    let key = *key;
    tokio::task::spawn_blocking(move || {
        join_chunks(rows)?.into_iter().map(|(item, sealed)| Ok::<_, String>((item.clone(), open(&key, &sealed).map_err(|e| format!("{item}: {e}"))?))).collect()
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn restore(home: &Path, ready: &Ready) -> Result<Option<Restored>, String> {
    let (key, _) = open_remote(ready).await?;
    let Some(key) = key else { return Ok(None) };
    let items = fetch_items(ready, &key).await?;
    write_restored(home, &items).map(Some)
}

/// On a machine with no local state, pull the backup and write the files
/// before the daemon loads them. Never overwrites existing data; failures
/// are logged and the daemon starts empty.
pub async fn restore_if_empty(cfg: &crate::config::Config) {
    let ready = match cfg.backup.ready() {
        Ok(Some(ready)) => ready,
        Ok(None) => return,
        Err(e) => return crate::warn!("backup restore skipped: {e}"),
    };
    if has_local_state(&cfg.home) {
        return crate::log!("backup restore skipped: local state exists");
    }
    match restore(&cfg.home, &ready).await {
        Ok(Some(r)) => {
            crate::log!("backup restored: {} agents, {} timelines, {} projects; the app must be paired again", r.agents, r.timelines, r.projects);
            for (path, remote) in r.clones {
                tokio::spawn(clone_project(path, remote));
            }
        }
        Ok(None) => crate::log!("backup restore: the remote database has no backup yet"),
        Err(e) => crate::warn!("backup restore failed, starting empty: {e}"),
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
        let sealed = seal(&key, b"hello hello hello hello").unwrap();
        assert!(!sealed.contains("hello"));
        assert_eq!(open(&key, &sealed).unwrap(), b"hello hello hello hello");
        assert_ne!(sealed, seal(&key, b"hello hello hello hello").unwrap(), "random nonce");
        let mut other = key;
        other[0] ^= 1;
        assert!(open(&other, &sealed).is_err());
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
        let sealed = seal(&key, &vec![7u8; 10]).unwrap();
        assert_eq!(open(&key, &join_chunks(split_chunks("x", sealed).into_iter().collect()).unwrap()["x"]).unwrap(), vec![7u8; 10]);
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
        let sealed = seal(&key, b"payload payload payload").unwrap();
        let raw = sealed_bytes(&sealed);
        for at in [0, 23, 24, raw.len() - 1] {
            let mut bad = raw.clone();
            bad[at] ^= 0x01;
            assert!(open(&key, &STANDARD.encode(bad)).is_err(), "flip at byte {at}");
        }
        assert_eq!(open(&key, &STANDARD.encode(&raw[..raw.len() - 1])).unwrap_err(), "decryption failed");
        assert_eq!(open(&key, &STANDARD.encode(&raw[..23])).unwrap_err(), "row too short");
        assert!(open(&key, "").is_err());
        assert!(open(&key, "not base64!").is_err());
    }

    #[test]
    fn nonces_differ_between_encryptions_of_the_same_data() {
        let (_, key) = make_meta("pw", &FAST).unwrap();
        let nonces: BTreeSet<Vec<u8>> = (0..8).map(|_| sealed_bytes(&seal(&key, b"same").unwrap())[..24].to_vec()).collect();
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
    fn config_row_matches_secret_words_case_insensitively_as_substrings() {
        let row = config_row(br#"{"tokensPerMin":5,"Password":"x","nested":{"PASSPHRASE":"y","ok":true}}"#).unwrap();
        let kept: Value = serde_json::from_slice(&row).unwrap();
        assert_eq!(kept, json!({ "nested": { "ok": true } }), "key matching is case-insensitive and substring based");
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
        let opened = join_chunks(rows).unwrap().into_iter().map(|(item, sealed)| (item, open(key, &sealed).unwrap())).collect();
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
        for secret_file in ["daemon-keypair.json", "server-id", "backup-state.json"] {
            assert!(!fresh.join(secret_file).exists(), "{secret_file} must not be restored");
        }

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
}
