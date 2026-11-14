//! Directory sync: a change feed per kind of directory entry (agents,
//! workspaces, projects), so a reconnecting app fetches only what changed
//! since its cursor. Fetch requests carry `sync: { generation, afterSeq }`,
//! responses `sync: { mode, generation, headSeq, removals }`, and live updates
//! `generation` + `seq`. State is in memory: a new generation per daemon start
//! makes every app take one snapshot.

use crate::util::new_id;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

/// Removals kept per kind; an older cursor gets a snapshot instead.
const REMOVAL_LOG: usize = 1000;

#[derive(Clone, Copy)]
pub enum Kind {
    Agents,
    Workspaces,
    Projects,
}

#[derive(Default)]
struct Feed {
    head: u64,
    /// id -> (seq of its last change, hash of the entry then; 0 = unknown).
    items: HashMap<String, (u64, u64)>,
    removed: VecDeque<(u64, String)>,
    /// Highest seq dropped from `removed`.
    trimmed: u64,
}

impl Feed {
    fn bump(&mut self, id: &str, hash: u64) -> u64 {
        self.head += 1;
        self.removed.retain(|(_, gone)| gone != id);
        self.items.insert(id.to_string(), (self.head, hash));
        self.head
    }

    fn remove(&mut self, id: &str) -> u64 {
        self.head += 1;
        self.items.remove(id);
        self.removed.retain(|(_, gone)| gone != id);
        self.removed.push_back((self.head, id.to_string()));
        if self.removed.len() > REMOVAL_LOG {
            if let Some((seq, _)) = self.removed.pop_front() {
                self.trimmed = seq;
            }
        }
        self.head
    }
}

pub struct DirectorySync {
    generation: String,
    feeds: Mutex<[Feed; 3]>,
}

/// Answer to a fetch: `sync` goes into the response payload; `only` maps the
/// ids that changed after the cursor to their seq (None for a snapshot).
pub struct Changes {
    pub sync: Value,
    only: Option<HashMap<String, u64>>,
}

impl Changes {
    /// Keep only changed entries. Changed ids the caller's filter hid are
    /// reported as removals so the app drops its copy.
    pub fn narrow(&mut self, visible: &mut Vec<Value>, id_ptr: &str) {
        let Some(only) = &self.only else { return };
        let id_of = |e: &Value| e.pointer(id_ptr).and_then(|v| v.as_str()).map(String::from);
        let shown: HashSet<String> = visible.iter().filter_map(id_of).collect();
        visible.retain(|e| id_of(e).is_some_and(|id| only.contains_key(&id)));
        if let Some(removals) = self.sync["removals"].as_array_mut() {
            removals.extend(only.iter().filter(|(id, _)| !shown.contains(*id)).map(|(id, seq)| json!({ "id": id, "seq": seq })));
        }
    }
}

fn fingerprint(entry: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    entry.to_string().hash(&mut hasher);
    hasher.finish()
}

impl DirectorySync {
    pub fn new() -> Self {
        DirectorySync { generation: new_id(), feeds: Mutex::new(Default::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, [Feed; 3]> {
        self.feeds.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number a live update (`kind: "remove"` or an upsert of `id`) and tag it
    /// with the generation, so the app can advance its cursor.
    pub fn stamp(&self, kind: Kind, id: &str, payload: &mut Value) {
        let mut feeds = self.lock();
        let feed = &mut feeds[kind as usize];
        let seq = if payload["kind"] == "remove" {
            feed.remove(id)
        } else {
            let hash = feed.items.get(id).map_or(0, |item| item.1);
            feed.bump(id, hash)
        };
        payload["generation"] = json!(self.generation);
        payload["seq"] = json!(seq);
    }

    /// Answer a fetch carrying `request` (its `sync` object) over every current
    /// entry of the kind, unfiltered. Entries that changed or vanished without a
    /// live update are caught here by comparing against what was last seen.
    pub fn fetch(&self, kind: Kind, request: &Value, entries: &[Value], id_ptr: &str) -> Changes {
        let mut feeds = self.lock();
        let feed = &mut feeds[kind as usize];
        let mut current = HashSet::new();
        for entry in entries {
            let Some(id) = entry.pointer(id_ptr).and_then(|v| v.as_str()) else { continue };
            let hash = fingerprint(entry);
            if feed.items.get(id).map(|item| item.1) != Some(hash) {
                feed.bump(id, hash);
            }
            current.insert(id);
        }
        let gone: Vec<String> = feed.items.keys().filter(|id| !current.contains(id.as_str())).cloned().collect();
        for id in gone {
            feed.remove(&id);
        }
        let after = Some(request)
            .filter(|r| r["generation"] == json!(self.generation))
            .and_then(|r| r["afterSeq"].as_u64())
            .filter(|after| (feed.trimmed..=feed.head).contains(after));
        let removals: Vec<Value> = after.map_or_else(Vec::new, |after| feed.removed.iter().filter(|(seq, _)| *seq > after).map(|(seq, id)| json!({ "id": id, "seq": seq })).collect());
        let only = after.map(|after| feed.items.iter().filter(|(_, item)| item.0 > after).map(|(id, item)| (id.clone(), item.0)).collect());
        let mode = if after.is_some() { "changes" } else { "snapshot" };
        let sync = json!({ "mode": mode, "generation": self.generation, "headSeq": feed.head, "removals": removals });
        Changes { sync, only }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, rev: u32) -> Value {
        json!({ "id": id, "rev": rev })
    }

    fn cursor(ds: &DirectorySync, after: u64) -> Value {
        json!({ "generation": ds.generation, "afterSeq": after })
    }

    fn ids(only: &Option<HashMap<String, u64>>) -> Vec<String> {
        let mut ids: Vec<String> = only.as_ref().expect("changes mode").keys().cloned().collect();
        ids.sort();
        ids
    }

    #[test]
    fn first_fetch_is_a_snapshot() {
        let ds = DirectorySync::new();
        let changes = ds.fetch(Kind::Agents, &json!({}), &[entry("a", 1), entry("b", 1)], "/id");
        assert_eq!(changes.sync["mode"], "snapshot");
        assert_eq!(changes.sync["generation"], json!(ds.generation));
        assert_eq!(changes.sync["headSeq"], 2);
        assert!(changes.only.is_none());
    }

    #[test]
    fn fetch_after_a_change_returns_only_that_change() {
        let ds = DirectorySync::new();
        let head = ds.fetch(Kind::Agents, &json!({}), &[entry("a", 1), entry("b", 1)], "/id").sync["headSeq"].as_u64().unwrap();
        let mut changes = ds.fetch(Kind::Agents, &cursor(&ds, head), &[entry("a", 2), entry("b", 1)], "/id");
        assert_eq!(changes.sync["mode"], "changes");
        assert_eq!(ids(&changes.only), ["a"]);
        let mut visible = vec![entry("a", 2), entry("b", 1)];
        changes.narrow(&mut visible, "/id");
        assert_eq!(visible, [entry("a", 2)]);
        let quiet = ds.fetch(Kind::Agents, &cursor(&ds, changes.sync["headSeq"].as_u64().unwrap()), &[entry("a", 2), entry("b", 1)], "/id");
        assert!(ids(&quiet.only).is_empty());
    }

    #[test]
    fn removal_after_the_cursor_is_reported() {
        let ds = DirectorySync::new();
        ds.fetch(Kind::Workspaces, &json!({}), &[entry("a", 1), entry("b", 1)], "/id");
        let changes = ds.fetch(Kind::Workspaces, &cursor(&ds, 2), &[entry("a", 1)], "/id");
        assert_eq!(changes.sync["mode"], "changes");
        assert_eq!(changes.sync["removals"], json!([{ "id": "b", "seq": 3 }]));
        let older = ds.fetch(Kind::Workspaces, &cursor(&ds, 3), &[entry("a", 1)], "/id");
        assert_eq!(older.sync["removals"], json!([]));
    }

    #[test]
    fn hidden_changed_entry_becomes_a_removal() {
        let ds = DirectorySync::new();
        ds.fetch(Kind::Agents, &json!({}), &[entry("a", 1)], "/id");
        let mut changes = ds.fetch(Kind::Agents, &cursor(&ds, 1), &[entry("a", 2)], "/id");
        let mut visible = vec![];
        changes.narrow(&mut visible, "/id");
        assert_eq!(changes.sync["removals"], json!([{ "id": "a", "seq": 2 }]));
    }

    #[test]
    fn readded_entry_is_not_also_a_removal() {
        let ds = DirectorySync::new();
        let mut gone = json!({ "kind": "remove" });
        ds.stamp(Kind::Projects, "p", &mut gone);
        let mut back = json!({ "kind": "upsert" });
        ds.stamp(Kind::Projects, "p", &mut back);
        assert_eq!(back["seq"], 2);
        let changes = ds.fetch(Kind::Projects, &cursor(&ds, 0), &[json!({ "id": "p" })], "/id");
        assert_eq!(changes.sync["removals"], json!([]));
        assert_eq!(ids(&changes.only), ["p"]);
    }

    #[test]
    fn other_generation_gets_a_snapshot() {
        let ds = DirectorySync::new();
        ds.fetch(Kind::Agents, &json!({}), &[entry("a", 1)], "/id");
        let changes = ds.fetch(Kind::Agents, &json!({ "generation": "older-daemon", "afterSeq": 1 }), &[entry("a", 1)], "/id");
        assert_eq!(changes.sync["mode"], "snapshot");
        assert!(changes.only.is_none());
    }

    #[test]
    fn cursor_older_than_the_removal_log_gets_a_snapshot() {
        let ds = DirectorySync::new();
        for i in 0..=REMOVAL_LOG {
            ds.stamp(Kind::Agents, &format!("a{i}"), &mut json!({ "kind": "remove" }));
        }
        let stale = ds.fetch(Kind::Agents, &cursor(&ds, 0), &[], "/id");
        assert_eq!(stale.sync["mode"], "snapshot");
        let kept = ds.fetch(Kind::Agents, &cursor(&ds, 1), &[], "/id");
        assert_eq!(kept.sync["mode"], "changes");
        assert_eq!(kept.sync["removals"].as_array().unwrap().len(), REMOVAL_LOG);
    }

    #[test]
    fn stamp_numbers_live_updates() {
        let ds = DirectorySync::new();
        let mut payload = json!({ "kind": "upsert" });
        ds.stamp(Kind::Agents, "a", &mut payload);
        assert_eq!(payload["seq"], 1);
        assert_eq!(payload["generation"], json!(ds.generation));
        let mut other = json!({ "kind": "upsert" });
        ds.stamp(Kind::Workspaces, "w", &mut other);
        assert_eq!(other["seq"], 1);
    }
}
