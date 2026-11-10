//! Code knowledge graph: files and symbols as nodes, calls and imports as
//! edges, kept as plain rows in the user's Turso database (the one backup.rs
//! talks to) so an agent asks for a scoped subgraph (graph_query and friends
//! in mcp_server.rs) instead of grepping and reading whole files.
//!
//! Built lazily on the first query of a project and re-scanned when the last
//! scan is older than `FRESH`: a scan stats every file and hashes the ones
//! that changed; when any file changed, every file is parsed again (a new
//! symbol can change what other files resolve to) and only the files whose
//! content or resolved edges differ are rewritten. No file watcher.
//! ponytail: communities are the first two directories of a path, not
//! Leiden; resolution is by unique name and path suffix, not by scope.

mod parse;

use crate::backup::{fingerprint, int, text, Stmt, Turso};
use parse::Parsed;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_BUDGET: usize = 8000;
pub const DEFAULT_DEPTH: usize = 2;
const MAX_DEPTH: usize = 5;
const IMPACT_DEPTH: usize = 8;
const PATH_DEPTH: usize = 6;
const MAX_FILES: usize = 4000;
const MAX_FILE_BYTES: u64 = 200 * 1024;
const CHUNK_FILES: usize = 50;
const BATCH: usize = 200;
const FRESH: Duration = Duration::from_secs(30);
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const SKIP_DIRS: [&str; 5] = ["node_modules", "target", "dist", "build", "vendor"];
const NO_DB: &str = "code graph needs backup.tursoUrl and backup.tursoToken";

const CREATE: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS feiyi_graph_files(project TEXT NOT NULL, path TEXT NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(project, path))",
    "CREATE TABLE IF NOT EXISTS feiyi_graph_nodes(project TEXT NOT NULL, id TEXT NOT NULL, kind TEXT NOT NULL, name TEXT NOT NULL, path TEXT NOT NULL, start INTEGER NOT NULL, \"end\" INTEGER NOT NULL, PRIMARY KEY(project, id))",
    "CREATE TABLE IF NOT EXISTS feiyi_graph_edges(project TEXT NOT NULL, src TEXT NOT NULL, dst TEXT NOT NULL, kind TEXT NOT NULL, PRIMARY KEY(project, src, dst, kind))",
    "CREATE INDEX IF NOT EXISTS feiyi_graph_nodes_name ON feiyi_graph_nodes(project, name)",
    "CREATE INDEX IF NOT EXISTS feiyi_graph_nodes_path ON feiyi_graph_nodes(project, path)",
    "CREATE INDEX IF NOT EXISTS feiyi_graph_edges_dst ON feiyi_graph_edges(project, dst)",
];
const SELECT_FILES: &str = "SELECT path, hash FROM feiyi_graph_files WHERE project = ?";
// Edges of a file leave from its path or from `path::...`: a key range, so the primary key serves it.
const DELETE_EDGES: &str = "DELETE FROM feiyi_graph_edges WHERE project = ? AND (src = ? OR (src >= ? AND src < ?))";
const DELETE_NODES: &str = "DELETE FROM feiyi_graph_nodes WHERE project = ? AND path = ?";
const DELETE_FILE: &str = "DELETE FROM feiyi_graph_files WHERE project = ? AND path = ?";
const INSERT_NODE: &str = "INSERT OR REPLACE INTO feiyi_graph_nodes(project, id, kind, name, path, start, \"end\") VALUES(?, ?, ?, ?, ?, ?, ?)";
const INSERT_EDGE: &str = "INSERT OR IGNORE INTO feiyi_graph_edges(project, src, dst, kind) VALUES(?, ?, ?, ?)";
const INSERT_FILE: &str = "INSERT OR REPLACE INTO feiyi_graph_files(project, path, hash) VALUES(?, ?, ?)";

/// Every node within `depth` edges (either direction) of the nodes whose name or path matches.
macro_rules! reach {
    () => {
        "WITH RECURSIVE reach(id, d) AS (\
           SELECT id, 0 FROM (SELECT id FROM feiyi_graph_nodes WHERE project = ? AND (name LIKE ? ESCAPE '\\' OR path LIKE ? ESCAPE '\\') ORDER BY kind = 'file', length(name) LIMIT 12) \
           UNION SELECT e.dst, r.d + 1 FROM reach r JOIN feiyi_graph_edges e ON e.project = ? AND e.src = r.id WHERE r.d < ? \
           UNION SELECT e.src, r.d + 1 FROM reach r JOIN feiyi_graph_edges e ON e.project = ? AND e.dst = r.id WHERE r.d < ?) "
    };
}
const QUERY_NODES: &str = concat!(
    reach!(),
    "SELECT n.id, n.kind, n.name, n.path, n.start, n.\"end\" FROM reach r JOIN feiyi_graph_nodes n ON n.project = ? AND n.id = r.id GROUP BY n.id ORDER BY MIN(r.d), n.path, n.start LIMIT 400"
);
const QUERY_EDGES: &str = concat!(
    reach!(),
    "SELECT e.src, e.dst, e.kind FROM feiyi_graph_edges e WHERE e.project = ? AND e.src IN (SELECT id FROM reach) AND e.dst IN (SELECT id FROM reach) ORDER BY e.src, e.dst LIMIT 1000"
);
const IMPACT: &str = "WITH RECURSIVE up(id, d) AS (\
    SELECT id, 0 FROM feiyi_graph_nodes WHERE project = ? AND kind <> 'file' AND (name = ? OR id = ?) \
    UNION SELECT e.src, u.d + 1 FROM up u JOIN feiyi_graph_edges e ON e.project = ? AND e.dst = u.id AND e.kind = 'calls' WHERE u.d < ?) \
    SELECT n.path, n.kind, n.name, n.start, n.\"end\", MIN(u.d) FROM up u JOIN feiyi_graph_nodes n ON n.project = ? AND n.id = u.id GROUP BY n.id ORDER BY n.path, n.start LIMIT 1000";
const FIND_NODE: &str = "SELECT id FROM feiyi_graph_nodes WHERE project = ? AND (id = ? OR name = ? OR path = ?) ORDER BY kind = 'file', id LIMIT 20";
const NEIGHBOURS: &str = "SELECT src, dst, kind FROM feiyi_graph_edges WHERE project = ? AND (src IN (SELECT value FROM json_each(?)) OR dst IN (SELECT value FROM json_each(?))) LIMIT 5000";
const COUNT_FILES: &str = "SELECT COUNT(*) FROM feiyi_graph_files WHERE project = ?";
const COUNT_SYMBOLS: &str = "SELECT path, COUNT(*) FROM feiyi_graph_nodes WHERE project = ? AND kind <> 'file' GROUP BY path";
const COUNT_EDGES: &str = "SELECT kind, COUNT(*) FROM feiyi_graph_edges WHERE project = ? GROUP BY kind";
const GOD_NODES: &str = "SELECT n.kind, n.name, n.path, n.start, n.\"end\", c.k FROM (SELECT dst, COUNT(*) AS k FROM feiyi_graph_edges WHERE project = ? GROUP BY dst ORDER BY k DESC LIMIT 15) c JOIN feiyi_graph_nodes n ON n.project = ? AND n.id = c.dst ORDER BY c.k DESC, n.id";

static DB: OnceLock<Turso> = OnceLock::new();
static PROJECTS: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Project>>>> = Mutex::new(BTreeMap::new());
static INFOS: Mutex<BTreeMap<String, Info>> = Mutex::new(BTreeMap::new());

/// What the last stored row of a file says: the stat it was hashed at and
/// `hash` = 16 hex of the content, 16 hex of the resolved edges.
#[derive(Clone)]
struct Known {
    stamp: (u64, u64),
    hash: String,
}

#[derive(Default)]
struct Project {
    loaded: bool,
    files: BTreeMap<String, Known>,
    scanned: Option<Instant>,
}

#[derive(Clone)]
struct Info {
    at: SystemTime,
    millis: u64,
    files: usize,
    symbols: Option<usize>,
    edges: Option<usize>,
    truncated: bool,
}

/// Use the backup's Turso database for the graph, when it is configured.
pub fn configure(cfg: &crate::config::Config) {
    match cfg.backup.turso() {
        Ok(Some(turso)) => {
            let _ = DB.set(turso);
            crate::log!("code graph on: graph rows go to the backup Turso database");
        }
        Ok(None) => crate::log!("code graph is off (it needs backup.tursoUrl and backup.tursoToken)"),
        Err(e) => crate::warn!("code graph is off: {e}"),
    }
}

/// For `daemon_status`: per project the last scan, no secrets.
pub fn status() -> Value {
    let infos = INFOS.lock().map(|i| i.clone()).unwrap_or_default();
    let projects: Vec<Value> = infos
        .iter()
        .map(|(dir, i)| json!({ "dir": dir, "files": i.files, "symbols": i.symbols, "edges": i.edges, "builtAt": humantime::format_rfc3339_seconds(i.at).to_string(), "buildMs": i.millis, "truncated": i.truncated }))
        .collect();
    json!({ "enabled": DB.get().is_some(), "projects": projects })
}

// --- parsing and resolving -----------------------------------------------------------

struct Node {
    id: String,
    kind: &'static str,
    name: String,
    path: String,
    start: usize,
    end: usize,
}

type Edge = (String, String, &'static str);

/// One file's rows and a hash of its edges.
struct FileGraph {
    path: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    edge_hash: u64,
}

fn symbol_id(path: &str, s: &parse::Symbol) -> String {
    match &s.parent {
        Some(parent) => format!("{path}::{parent}::{}", s.name),
        None => format!("{path}::{}", s.name),
    }
}

/// `src/graph/mod.rs` to `src/graph`: the first two directories; root files are `.`.
fn community(path: &str) -> String {
    let dirs: Vec<&str> = path.split('/').collect();
    let dirs = &dirs[..dirs.len() - 1];
    if dirs.is_empty() {
        return ".".into();
    }
    dirs[..dirs.len().min(2)].join("/")
}

/// Import resolution: the last path segment of a module's key to its (key, file) pairs.
/// A key is a file's path without extension (the directory for `mod`, `index`, `__init__` and Go files) or its full path.
fn module_index(paths: &[&str]) -> HashMap<String, Vec<(String, String)>> {
    let mut index: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut add = |key: String, path: &str| {
        if let Some(last) = key.rsplit('/').next().filter(|l| !l.is_empty()) {
            index.entry(last.to_string()).or_default().push((key.clone(), path.to_string()));
        }
    };
    for &path in paths {
        add(path.to_string(), path);
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let (stem, ext) = file.rsplit_once('.').unwrap_or((file, ""));
        if ext == "go" || ["mod", "index", "__init__"].contains(&stem) {
            if !dir.is_empty() {
                add(dir.to_string(), path);
            }
        } else {
            add(if dir.is_empty() { stem.to_string() } else { format!("{dir}/{stem}") }, path);
        }
    }
    index
}

/// The file an import's candidates point at: the first candidate that one module key ends with;
/// among several, the one next to the importer; else none.
fn resolve_import(index: &HashMap<String, Vec<(String, String)>>, importer: &str, candidates: &[String]) -> Option<String> {
    let importer_dir = importer.rsplit_once('/').map_or("", |(d, _)| d);
    for c in candidates {
        let Some(entries) = c.rsplit('/').next().and_then(|last| index.get(last)) else { continue };
        let suffix = format!("/{c}");
        let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
        for (key, path) in entries.iter().filter(|(k, _)| k == c || k.ends_with(&suffix)) {
            keys.entry(key).or_insert(path);
        }
        match keys.len() {
            0 => continue,
            1 => return keys.values().next().map(|p| p.to_string()),
            _ => {
                let near: Vec<&&str> = keys.iter().filter(|(k, _)| k.rsplit_once('/').map_or("", |(d, _)| d) == importer_dir).map(|(_, p)| p).collect();
                return (near.len() == 1).then(|| near[0].to_string());
            }
        }
    }
    None
}

/// Nodes and edges of every file. A call resolves to the one symbol of that
/// name in the repo, else to one in its own file, else it is dropped.
fn resolve(files: &[(String, Parsed)]) -> Vec<FileGraph> {
    let ids: Vec<Vec<String>> = files.iter().map(|(path, p)| p.symbols.iter().map(|s| symbol_id(path, s)).collect()).collect();
    let mut by_name: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (i, (path, p)) in files.iter().enumerate() {
        for (s, id) in p.symbols.iter().zip(&ids[i]) {
            let list = by_name.entry(s.name.as_str()).or_default();
            if !list.iter().any(|(known, _)| *known == id.as_str()) {
                list.push((id.as_str(), path.as_str()));
            }
        }
    }
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let modules = module_index(&paths);
    files
        .iter()
        .enumerate()
        .map(|(i, (path, p))| {
            let base = path.rsplit('/').next().unwrap_or(path);
            let mut nodes = vec![Node { id: path.clone(), kind: "file", name: base.to_string(), path: path.clone(), start: 1, end: p.lines.max(1) }];
            let mut seen = HashSet::new();
            for (s, id) in p.symbols.iter().zip(&ids[i]) {
                if seen.insert(id.as_str()) {
                    nodes.push(Node { id: id.clone(), kind: s.kind, name: s.name.clone(), path: path.clone(), start: s.start, end: s.end });
                }
            }
            let mut edges: BTreeSet<Edge> = BTreeSet::new();
            for call in &p.calls {
                let src = call.owner.map_or(path.as_str(), |o| ids[i][o].as_str());
                let Some(candidates) = by_name.get(call.name.as_str()) else { continue };
                let dst = match candidates.as_slice() {
                    [only] => Some(only.0),
                    many => many.iter().find(|(_, file)| *file == path.as_str()).map(|(id, _)| *id),
                };
                if let Some(dst) = dst.filter(|d| *d != src) {
                    edges.insert((src.to_string(), dst.to_string(), "calls"));
                }
            }
            for candidates in &p.imports {
                if let Some(dst) = resolve_import(&modules, path, candidates).filter(|d| d != path) {
                    edges.insert((path.clone(), dst, "imports"));
                }
            }
            let edges: Vec<Edge> = edges.into_iter().collect();
            let joined: String = edges.iter().map(|(s, d, k)| format!("{s}\t{d}\t{k}\n")).collect();
            FileGraph { path: path.clone(), nodes, edges, edge_hash: fingerprint(joined.as_bytes()) }
        })
        .collect()
}

// --- the files ---------------------------------------------------------------------------

/// A relative path under a hidden or vendored directory.
fn skipped(path: &str) -> bool {
    let mut parts = path.split('/').collect::<Vec<_>>();
    parts.pop();
    parts.iter().any(|d| d.starts_with('.') || SKIP_DIRS.contains(d))
}

async fn git_files(root: &Path) -> Option<Vec<String>> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).current_dir(root).env("GIT_TERMINAL_PROMPT", "0").stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    out.status.success().then(|| out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

fn walk(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    dirs.push(entry.path());
                }
            } else if kind.is_file() && crate::outline::lang(Path::new(&name)).is_some() {
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    found.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        if found.len() > MAX_FILES {
            break;
        }
    }
    found
}

/// Source files of the project, sorted, at most `MAX_FILES`; true when more existed.
async fn list_files(root: &Path) -> (Vec<String>, bool) {
    let listed = if root.join(".git").exists() { git_files(root).await } else { None };
    let mut paths = match listed {
        Some(paths) => paths,
        None => {
            let root = root.to_path_buf();
            tokio::task::spawn_blocking(move || walk(&root)).await.unwrap_or_default()
        }
    };
    paths.retain(|p| !skipped(p) && crate::outline::lang(Path::new(p)).is_some());
    paths.sort();
    paths.dedup();
    let truncated = paths.len() > MAX_FILES;
    if truncated {
        crate::warn!("code graph: {} has {} source files, indexing the first {MAX_FILES}", root.display(), paths.len());
        paths.truncate(MAX_FILES);
    }
    (paths, truncated)
}

struct Scanned {
    path: String,
    stamp: (u64, u64),
    content: String,
    parsed: Option<Parsed>,
}

/// Stat each file, hash the ones whose stat changed (all when `parse_all`) and
/// parse them when `parse_all`. Files that vanished or are too big are left out.
fn scan(root: &Path, files: Vec<(String, Option<Known>)>, parse_all: bool) -> Vec<Scanned> {
    files
        .into_iter()
        .filter_map(|(path, known)| {
            let full = root.join(&path);
            let meta = std::fs::metadata(&full).ok().filter(|m| m.is_file() && m.len() <= MAX_FILE_BYTES)?;
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
            let stamp = (meta.len(), mtime);
            if let Some(known) = known.filter(|k| !parse_all && k.stamp == stamp) {
                return Some(Scanned { path, stamp, content: known.hash[..16].to_string(), parsed: None });
            }
            let bytes = std::fs::read(&full).ok()?;
            let content = format!("{:016x}", fingerprint(&bytes));
            let parsed = if parse_all { parse::parse(Path::new(&path), &String::from_utf8_lossy(&bytes)) } else { None };
            Some(Scanned { path, stamp, content, parsed })
        })
        .collect()
}

async fn scan_all(root: &Path, st: &Project, paths: &[String], parse_all: bool) -> Result<Vec<Scanned>, String> {
    let mut all = Vec::with_capacity(paths.len());
    for chunk in paths.chunks(CHUNK_FILES) {
        let files: Vec<(String, Option<Known>)> = chunk.iter().map(|p| (p.clone(), if parse_all { None } else { st.files.get(p).cloned() })).collect();
        let root = root.to_path_buf();
        all.extend(tokio::task::spawn_blocking(move || scan(&root, files, parse_all)).await.map_err(|e| format!("scan failed: {e}"))?);
    }
    Ok(all)
}

// --- writing -----------------------------------------------------------------------------

fn delete_rows(key: &str, path: &str) -> Vec<Stmt> {
    let (low, high) = (format!("{path}::"), format!("{path}:;"));
    vec![
        Stmt { sql: DELETE_EDGES, args: vec![text(key), text(path), text(&low), text(&high)] },
        Stmt { sql: DELETE_NODES, args: vec![text(key), text(path)] },
    ]
}

/// Replace a file's rows; its `feiyi_graph_files` row goes last, so an interrupted write is redone next scan.
fn file_statements(key: &str, g: &FileGraph, hash: &str) -> Vec<Stmt> {
    let mut stmts = delete_rows(key, &g.path);
    for n in &g.nodes {
        stmts.push(Stmt { sql: INSERT_NODE, args: vec![text(key), text(&n.id), text(n.kind), text(&n.name), text(&n.path), int(n.start as i64), int(n.end as i64)] });
    }
    for (src, dst, kind) in &g.edges {
        stmts.push(Stmt { sql: INSERT_EDGE, args: vec![text(key), text(src), text(dst), text(kind)] });
    }
    stmts.push(Stmt { sql: INSERT_FILE, args: vec![text(key), text(&g.path), text(hash)] });
    stmts
}

fn gone_statements(key: &str, path: &str) -> Vec<Stmt> {
    let mut stmts = delete_rows(key, path);
    stmts.push(Stmt { sql: DELETE_FILE, args: vec![text(key), text(path)] });
    stmts
}

/// Hands statements back in pipelines of `BATCH`.
#[derive(Default)]
struct Batcher {
    pending: Vec<Stmt>,
}

impl Batcher {
    fn add(&mut self, stmts: Vec<Stmt>) -> Vec<Vec<Stmt>> {
        self.pending.extend(stmts);
        let mut full = Vec::new();
        while self.pending.len() >= BATCH {
            let rest = self.pending.split_off(BATCH);
            full.push(std::mem::replace(&mut self.pending, rest));
        }
        full
    }
}

// --- the build ---------------------------------------------------------------------------

async fn load_known(turso: &Turso, key: &str, st: &mut Project) -> Result<(), String> {
    let mut stmts: Vec<Stmt> = CREATE.iter().map(|sql| Stmt { sql, args: vec![] }).collect();
    stmts.push(Stmt { sql: SELECT_FILES, args: vec![text(key)] });
    let rows = turso.run(stmts).await?.pop().unwrap_or_default();
    st.files = rows
        .into_iter()
        .filter_map(|r| match <[String; 2]>::try_from(r) {
            Ok([path, hash]) if hash.len() == 32 => Some((path, Known { stamp: (0, 0), hash })),
            _ => None,
        })
        .collect();
    st.loaded = true;
    Ok(())
}

fn record(key: &str, info: Info) {
    if let Ok(mut infos) = INFOS.lock() {
        infos.insert(key.to_string(), info);
    }
}

async fn build(turso: &Turso, key: &str, st: &mut Project, force: bool) -> Result<(), String> {
    let started = Instant::now();
    let root = PathBuf::from(key);
    let (paths, truncated) = list_files(&root).await;
    if !st.loaded || force {
        load_known(turso, key, st).await?;
    }
    let seen = scan_all(&root, st, &paths, false).await?;
    let unchanged = !force && seen.len() == st.files.len() && seen.iter().all(|s| st.files.get(&s.path).is_some_and(|k| k.hash[..16] == s.content));
    if unchanged {
        for s in seen {
            if let Some(k) = st.files.get_mut(&s.path) {
                k.stamp = s.stamp;
            }
        }
        st.scanned = Some(Instant::now());
        let previous = INFOS.lock().ok().and_then(|i| i.get(key).cloned());
        let info = Info { at: SystemTime::now(), millis: started.elapsed().as_millis() as u64, files: st.files.len(), symbols: previous.as_ref().and_then(|i| i.symbols), edges: previous.and_then(|i| i.edges), truncated };
        record(key, info);
        return Ok(());
    }

    let mut stamps: BTreeMap<String, ((u64, u64), String)> = BTreeMap::new();
    let mut files = Vec::new();
    for s in scan_all(&root, st, &paths, true).await? {
        if let Some(parsed) = s.parsed {
            stamps.insert(s.path.clone(), (s.stamp, s.content));
            files.push((s.path, parsed));
        }
    }
    let graphs = tokio::task::spawn_blocking(move || resolve(&files)).await.map_err(|e| format!("resolve failed: {e}"))?;

    let mut next: BTreeMap<String, Known> = BTreeMap::new();
    let mut batcher = Batcher::default();
    let mut rewritten = 0;
    for g in &graphs {
        let (stamp, content) = &stamps[&g.path];
        let hash = format!("{content}{:016x}", g.edge_hash);
        if force || st.files.get(&g.path).map_or(true, |k| k.hash != hash) {
            rewritten += 1;
            for batch in batcher.add(file_statements(key, g, &hash)) {
                turso.run(batch).await?;
            }
        }
        next.insert(g.path.clone(), Known { stamp: *stamp, hash });
    }
    for path in st.files.keys().filter(|p| !next.contains_key(*p)) {
        for batch in batcher.add(gone_statements(key, path)) {
            turso.run(batch).await?;
        }
    }
    let rest = std::mem::take(&mut batcher.pending);
    if !rest.is_empty() {
        turso.run(rest).await?;
    }
    st.files = next;
    st.scanned = Some(Instant::now());

    let symbols: usize = graphs.iter().map(|g| g.nodes.len() - 1).sum();
    let edges: usize = graphs.iter().map(|g| g.edges.len()).sum();
    let millis = started.elapsed().as_millis() as u64;
    crate::log!("code graph: {} files, {symbols} symbols, {edges} edges, {rewritten} files rewritten, {millis} ms", graphs.len());
    record(key, Info { at: SystemTime::now(), millis, files: graphs.len(), symbols: Some(symbols), edges: Some(edges), truncated });
    Ok(())
}

/// The database and the project key, after a scan when the last one is older than `FRESH` (or `force`).
async fn ensure(dir: &str, force: bool) -> Result<(Turso, String), String> {
    let turso = DB.get().ok_or(NO_DB)?.clone();
    if !Path::new(dir).is_absolute() {
        return Err("pass the absolute path of the project directory as dir".into());
    }
    let root = std::fs::canonicalize(dir).map_err(|e| format!("{dir}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("{dir} is not a directory"));
    }
    let key = root.to_string_lossy().into_owned();
    let project = PROJECTS.lock().map_err(|_| "code graph state is poisoned")?.entry(key.clone()).or_default().clone();
    let mut st = project.lock().await;
    if force || st.scanned.map_or(true, |t| t.elapsed() > FRESH) {
        build(&turso, &key, &mut st, force).await?;
    }
    Ok((turso, key))
}

// --- queries -------------------------------------------------------------------------------

/// `lines` joined while they fit in `budget` characters, then a note of what was cut.
fn fit(lines: &[String], budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for (i, line) in lines.iter().enumerate() {
        let len = line.chars().count() + 1;
        if used + len > budget {
            out.push_str(&format!("… cut at {budget} characters, {} more lines\n", lines.len() - i));
            return out;
        }
        out.push_str(line);
        out.push('\n');
        used += len;
    }
    out
}

fn like_escape(q: &str) -> String {
    q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// `[kind, name, path, start, end]` as `kind name path:start-end`.
fn node_line(c: &[String]) -> String {
    format!("{} {} {}:{}-{}", c[0], c[1], c[2], c[3], c[4])
}

fn budget_of(budget: usize) -> usize {
    budget.clamp(500, 200_000)
}

/// Symbols whose name, or files whose path, contain `q`, with the callers, callees and imports within `depth` edges.
pub async fn query(dir: &str, q: &str, depth: usize, budget: usize) -> Result<String, String> {
    let q = q.trim();
    if q.is_empty() {
        return Err("q is empty".into());
    }
    let (turso, key) = ensure(dir, false).await?;
    let (pattern, depth) = (format!("%{}%", like_escape(q)), depth.clamp(1, MAX_DEPTH) as i64);
    let reach = || vec![text(&key), text(&pattern), text(&pattern), text(&key), int(depth), text(&key), int(depth), text(&key)];
    let mut rows = turso.run(vec![Stmt { sql: QUERY_NODES, args: reach() }, Stmt { sql: QUERY_EDGES, args: reach() }]).await?;
    let edges = rows.pop().unwrap_or_default();
    let nodes = rows.pop().unwrap_or_default();
    if nodes.is_empty() {
        return Ok(format!("nothing matches \"{q}\" in {key}\n"));
    }
    let shown: HashSet<&str> = nodes.iter().map(|n| n[0].as_str()).collect();
    let mut lines = vec![format!("{} nodes within {depth} edges of \"{q}\"", nodes.len())];
    lines.extend(nodes.iter().map(|n| node_line(&n[1..])));
    let edges: Vec<String> = edges.iter().filter(|e| shown.contains(e[0].as_str()) && shown.contains(e[1].as_str())).map(|e| format!("{} {} {}", e[0], e[2], e[1])).collect();
    lines.push(format!("{} edges (src kind dst, ids are path::name):", edges.len()));
    lines.extend(edges);
    Ok(fit(&lines, budget_of(budget)))
}

/// Everything that calls `symbol`, directly or not, grouped by file.
pub async fn impact(dir: &str, symbol: &str, budget: usize) -> Result<String, String> {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return Err("symbol is empty".into());
    }
    let (turso, key) = ensure(dir, false).await?;
    let args = vec![text(&key), text(symbol), text(symbol), text(&key), int(IMPACT_DEPTH as i64), text(&key)];
    let rows = turso.run(vec![Stmt { sql: IMPACT, args }]).await?.pop().unwrap_or_default();
    let (targets, callers): (Vec<_>, Vec<_>) = rows.iter().partition(|r| r[5] == "0");
    if targets.is_empty() {
        return Ok(format!("no symbol named {symbol} in {key}\n"));
    }
    let files: BTreeSet<&str> = callers.iter().map(|r| r[0].as_str()).collect();
    let mut lines = vec![format!("{} callers of {symbol} in {} files", callers.len(), files.len())];
    for t in &targets {
        lines.push(format!("target: {}", node_line(&[t[1].clone(), t[2].clone(), t[0].clone(), t[3].clone(), t[4].clone()])));
    }
    let mut current = "";
    for r in &callers {
        if r[0] != current {
            current = &r[0];
            lines.push(current.to_string());
        }
        lines.push(format!("  {} {} {}-{} (depth {})", r[1], r[2], r[3], r[4], r[5]));
    }
    Ok(fit(&lines, budget_of(budget)))
}

/// One breadth-first step over `rows` (`[src, dst, kind]`, edges touching `frontier`):
/// records how each new node was reached in `reached` and returns them.
fn advance(frontier: &[String], rows: &[Vec<String>], reached: &mut HashMap<String, (String, String)>) -> Vec<String> {
    let from: HashSet<&str> = frontier.iter().map(String::as_str).collect();
    let mut next = Vec::new();
    for r in rows {
        let (src, dst, kind) = (&r[0], &r[1], &r[2]);
        let (new, prev, arrow) = if from.contains(src.as_str()) && !reached.contains_key(dst) {
            (dst, src, format!("--{kind}-->"))
        } else if from.contains(dst.as_str()) && !reached.contains_key(src) {
            (src, dst, format!("<--{kind}--"))
        } else {
            continue;
        };
        reached.insert(new.clone(), (prev.clone(), arrow));
        next.push(new.clone());
    }
    next
}

/// `a --calls--> b <--imports-- c`: the way back from `end` to a start node.
fn trail(reached: &HashMap<String, (String, String)>, end: &str) -> String {
    let mut parts = vec![end.to_string()];
    let mut at = end;
    while let Some((prev, arrow)) = reached.get(at).filter(|(p, _)| !p.is_empty()) {
        parts.push(arrow.clone());
        parts.push(prev.clone());
        at = prev;
    }
    parts.reverse();
    parts.join(" ")
}

/// The shortest chain of calls and imports between two symbols or files, at most `PATH_DEPTH` edges long.
pub async fn path(dir: &str, from: &str, to: &str) -> Result<String, String> {
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() || to.is_empty() {
        return Err("from and to are required".into());
    }
    let (turso, key) = ensure(dir, false).await?;
    let find = |name: &str| Stmt { sql: FIND_NODE, args: vec![text(&key), text(name), text(name), text(name)] };
    let mut found = turso.run(vec![find(from), find(to)]).await?;
    let ends: HashSet<String> = found.pop().unwrap_or_default().into_iter().flatten().collect();
    let starts: Vec<String> = found.pop().unwrap_or_default().into_iter().flatten().collect();
    if starts.is_empty() || ends.is_empty() {
        return Ok(format!("no node named {} in {key}\n", if starts.is_empty() { from } else { to }));
    }
    let mut reached: HashMap<String, (String, String)> = starts.iter().map(|s| (s.clone(), (String::new(), String::new()))).collect();
    let mut frontier = starts;
    for step in 0..=PATH_DEPTH {
        if let Some(end) = frontier.iter().find(|n| ends.contains(*n)) {
            return Ok(format!("{}\n", trail(&reached, end)));
        }
        if step == PATH_DEPTH || frontier.is_empty() {
            break;
        }
        let ids = json!(frontier).to_string();
        let rows = turso.run(vec![Stmt { sql: NEIGHBOURS, args: vec![text(&key), text(&ids), text(&ids)] }]).await?.pop().unwrap_or_default();
        frontier = advance(&frontier, &rows, &mut reached);
    }
    Ok(format!("no path from {from} to {to} within {PATH_DEPTH} edges\n"))
}

/// Counts, communities, the most depended-on nodes and the last scan.
pub async fn report(dir: &str) -> Result<String, String> {
    let (turso, key) = ensure(dir, false).await?;
    let one = |sql| Stmt { sql, args: vec![text(&key)] };
    let mut rows = turso.run(vec![one(COUNT_FILES), one(COUNT_SYMBOLS), one(COUNT_EDGES), one("SELECT path FROM feiyi_graph_files WHERE project = ?"), Stmt { sql: GOD_NODES, args: vec![text(&key), text(&key)] }]).await?;
    let gods = rows.pop().unwrap_or_default();
    let paths = rows.pop().unwrap_or_default();
    let edge_kinds = rows.pop().unwrap_or_default();
    let symbols = rows.pop().unwrap_or_default();
    let files = rows.pop().and_then(|r| r.into_iter().next()).and_then(|c| c.into_iter().next()).unwrap_or_default();

    let mut communities: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for p in &paths {
        communities.entry(community(&p[0])).or_default().0 += 1;
    }
    for s in &symbols {
        communities.entry(community(&s[0])).or_default().1 += s[1].parse::<usize>().unwrap_or(0);
    }
    let mut communities: Vec<_> = communities.into_iter().collect();
    communities.sort_by(|a, b| (b.1 .0 + b.1 .1).cmp(&(a.1 .0 + a.1 .1)).then_with(|| a.0.cmp(&b.0)));

    let symbol_total: usize = symbols.iter().map(|s| s[1].parse::<usize>().unwrap_or(0)).sum();
    let edge_total: usize = edge_kinds.iter().map(|e| e[1].parse::<usize>().unwrap_or(0)).sum();
    let kinds: Vec<String> = edge_kinds.iter().map(|e| format!("{} {}", e[1], e[0])).collect();
    let mut out = format!("{key}\n{files} files, {symbol_total} symbols, {edge_total} edges ({})\n", kinds.join(", "));
    match INFOS.lock().ok().and_then(|i| i.get(&key).cloned()) {
        Some(i) => out.push_str(&format!("last scan {} ({} ms){}\n", humantime::format_rfc3339_seconds(i.at), i.millis, if i.truncated { format!(", truncated to the first {MAX_FILES} files") } else { String::new() })),
        None => out.push_str("no scan since the daemon started\n"),
    }
    out.push_str("communities (files, symbols):\n");
    for (name, (f, s)) in communities.iter().take(20) {
        out.push_str(&format!("  {name}: {f}, {s}\n"));
    }
    out.push_str("most depended-on (in-degree):\n");
    for g in &gods {
        out.push_str(&format!("  {} <- {}\n", node_line(&g[..5]), g[5]));
    }
    Ok(out)
}

/// Rescan and rewrite every file of the project, ignoring the stored hashes.
pub async fn rebuild(dir: &str) -> Result<String, String> {
    let (_, key) = ensure(dir, true).await?;
    let info = INFOS.lock().ok().and_then(|i| i.get(&key).cloned());
    Ok(match info {
        Some(i) => format!("rebuilt {key}: {} files, {} symbols, {} edges in {} ms{}\n", i.files, i.symbols.unwrap_or(0), i.edges.unwrap_or(0), i.millis, if i.truncated { format!(" (truncated to the first {MAX_FILES} files)") } else { String::new() }),
        None => format!("rebuilt {key}\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(file: &str, src: &str) -> (String, Parsed) {
        (file.to_string(), parse::parse(Path::new(file), src).unwrap())
    }

    fn edges_of(graphs: &[FileGraph], path: &str) -> Vec<String> {
        graphs.iter().find(|g| g.path == path).unwrap().edges.iter().map(|(s, d, k)| format!("{s} {k} {d}")).collect()
    }

    #[test]
    fn a_call_resolves_to_the_unique_symbol_else_one_in_its_own_file_else_nothing() {
        let files = vec![
            parsed("src/a.rs", "fn run() {\n    helper();\n    shared();\n    ghost();\n    run();\n}\n\nfn shared() {}\n"),
            parsed("src/b.rs", "fn helper() {}\n\nfn shared() {}\n"),
            parsed("src/c.rs", "fn other() {\n    shared();\n}\n"),
        ];
        let graphs = resolve(&files);
        assert_eq!(edges_of(&graphs, "src/a.rs"), ["src/a.rs::run calls src/a.rs::shared", "src/a.rs::run calls src/b.rs::helper"]);
        assert!(edges_of(&graphs, "src/c.rs").is_empty(), "ambiguous and not local: dropped");
        let nodes: Vec<&str> = graphs[0].nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(nodes, ["src/a.rs", "src/a.rs::run", "src/a.rs::shared"]);
    }

    #[test]
    fn imports_resolve_by_path_suffix() {
        let files = vec![
            parsed("src/main.rs", "mod graph;\nuse crate::backup::Turso;\nuse std::fs;\nfn main() {}\n"),
            parsed("src/backup.rs", "pub struct Turso {}\n"),
            parsed("src/graph/mod.rs", "fn x() {}\n"),
            parsed("web/app.ts", "import { a } from './lib/a';\nimport React from 'react';\nexport function go() {}\n"),
            parsed("web/lib/a.ts", "export function a() {}\n"),
            parsed("main.go", "package main\n\nimport \"github.com/o/r/pkg/util\"\n"),
            parsed("pkg/util/u.go", "package util\n"),
            parsed("x/util.rs", "fn u() {}\n"),
            parsed("y/util.rs", "fn v() {}\n"),
            parsed("x/main.rs", "mod util;\n"),
            parsed("z/main.rs", "mod util;\n"),
        ];
        let graphs = resolve(&files);
        assert_eq!(edges_of(&graphs, "src/main.rs"), ["src/main.rs imports src/backup.rs", "src/main.rs imports src/graph/mod.rs"]);
        assert_eq!(edges_of(&graphs, "web/app.ts"), ["web/app.ts imports web/lib/a.ts"]);
        assert_eq!(edges_of(&graphs, "main.go"), ["main.go imports pkg/util/u.go"]);
        assert_eq!(edges_of(&graphs, "x/main.rs"), ["x/main.rs imports x/util.rs"], "the neighbour wins");
        assert!(edges_of(&graphs, "z/main.rs").is_empty(), "two candidates, none next to it");
    }

    #[test]
    fn communities_are_the_first_two_directories() {
        assert_eq!(community("src/graph/mod.rs"), "src/graph");
        assert_eq!(community("src/main.rs"), "src");
        assert_eq!(community("a/b/c/d.rs"), "a/b");
        assert_eq!(community("build.sh"), ".");
    }

    #[test]
    fn output_stops_at_the_budget_and_says_how_much_was_cut() {
        let lines: Vec<String> = (0..5).map(|_| "aaaaaaaaa".to_string()).collect();
        assert_eq!(fit(&lines, 25), "aaaaaaaaa\naaaaaaaaa\n… cut at 25 characters, 3 more lines\n");
        assert_eq!(fit(&lines, 100).lines().count(), 5);
    }

    fn filler(n: usize) -> Vec<Stmt> {
        (0..n).map(|_| Stmt { sql: DELETE_FILE, args: vec![] }).collect()
    }

    #[test]
    fn statements_go_out_in_pipelines_of_the_batch_size() {
        let mut b = Batcher::default();
        let full = b.add(filler(450));
        assert_eq!(full.iter().map(Vec::len).collect::<Vec<_>>(), [BATCH, BATCH]);
        assert_eq!(b.pending.len(), 50);
        assert_eq!(b.add(filler(150)).len(), 1);
        assert!(b.pending.is_empty());
    }

    #[test]
    fn a_file_is_replaced_row_by_row_and_its_files_row_goes_last() {
        let node = |id: &str| Node { id: id.into(), kind: "fn", name: "f".into(), path: "src/a.rs".into(), start: 1, end: 2 };
        let g = FileGraph { path: "src/a.rs".into(), nodes: vec![node("src/a.rs"), node("src/a.rs::f")], edges: vec![("src/a.rs::f".into(), "src/b.rs::g".into(), "calls")], edge_hash: 0 };
        let stmts = file_statements("/p", &g, "h");
        let sqls: Vec<&str> = stmts.iter().map(|s| s.sql).collect();
        assert_eq!(sqls, [DELETE_EDGES, DELETE_NODES, INSERT_NODE, INSERT_NODE, INSERT_EDGE, INSERT_FILE]);
        assert_eq!(stmts[0].args[2]["value"], "src/a.rs::");
        assert_eq!(stmts[0].args[3]["value"], "src/a.rs:;");
        assert_eq!(stmts[2].args[5]["value"], "1");
        let gone: Vec<&str> = gone_statements("/p", "src/a.rs").iter().map(|s| s.sql).collect();
        assert_eq!(gone, [DELETE_EDGES, DELETE_NODES, DELETE_FILE]);
    }

    #[test]
    fn placeholders_match_the_arguments_each_query_is_sent_with() {
        let count = |sql: &str| sql.matches('?').count();
        assert_eq!((count(QUERY_NODES), count(QUERY_EDGES), count(IMPACT)), (8, 8, 6));
        assert_eq!((count(NEIGHBOURS), count(GOD_NODES), count(DELETE_EDGES), count(INSERT_NODE)), (3, 2, 4, 7));
        assert_eq!(like_escape("a_b%c\\"), "a\\_b\\%c\\\\");
    }

    #[test]
    fn the_shortest_path_is_walked_back_over_either_edge_direction() {
        let row = |s: &str, d: &str, k: &str| vec![s.to_string(), d.to_string(), k.to_string()];
        let mut reached = HashMap::from([("a".to_string(), (String::new(), String::new()))]);
        let step1 = advance(&["a".to_string()], &[row("a", "b", "calls"), row("x", "y", "calls")], &mut reached);
        assert_eq!(step1, ["b"]);
        let step2 = advance(&step1, &[row("a", "b", "calls"), row("c", "b", "imports")], &mut reached);
        assert_eq!(step2, ["c"]);
        assert_eq!(trail(&reached, "c"), "a --calls--> b <--imports-- c");
        assert_eq!(trail(&reached, "a"), "a");
    }

    #[test]
    fn source_files_under_hidden_or_vendored_directories_are_skipped() {
        assert!(skipped("node_modules/x/a.js") && skipped(".github/a.sh") && skipped("a/vendor/b.go"));
        assert!(!skipped("src/a.rs") && !skipped("build.sh"));
    }

    #[tokio::test]
    async fn without_a_database_every_tool_says_what_to_configure() {
        assert_eq!(query("/tmp", "x", 2, 100).await.unwrap_err(), NO_DB);
        assert_eq!(rebuild("/tmp").await.unwrap_err(), NO_DB);
        assert_eq!(status()["enabled"], false);
    }
}
