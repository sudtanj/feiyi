//! Schedules (`schedule/*` requests): prompts that run on a cadence, with
//! upstream's message shapes. State lives in `schedules.json`; the manager
//! sleeps until the next due time (`Manager::schedule_deadline`) and fires
//! through the normal agent paths. Runs missed while the daemon was down are
//! not replayed: the next run is the first slot after startup.

use crate::cron::Cadence;
use crate::manager::{reply, Manager, Out};
use crate::providers::is_supported;
use crate::rpc::{normalize_dir, s};
use crate::util::{new_id, now_iso, truncate, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Finished runs kept per schedule (more when `maxRuns` is higher).
const RUN_HISTORY: usize = 100;
const OUTPUT_MAX: usize = 16 * 1024;
const MAX_SLEEP_MS: i64 = 24 * 3600 * 1000;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn iso(ms: i64) -> String {
    humantime::format_rfc3339_millis(UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)).to_string()
}

fn parse_iso(text: &str) -> Option<i64> {
    humantime::parse_rfc3339(text).ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub scheduled_for: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// `running`, `succeeded` or `failed`.
    pub status: String,
    pub agent_id: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub name: Option<String>,
    pub prompt: String,
    pub cadence: Cadence,
    /// `{type: "agent", agentId}` or `{type: "new-agent", config}`, as upstream.
    pub target: Value,
    /// `active`, `paused` or `completed`.
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub paused_at: Option<String>,
    pub expires_at: Option<String>,
    pub max_runs: Option<u64>,
    pub runs: Vec<Run>,
}

impl Schedule {
    fn next_run_ms(&self) -> Option<i64> {
        self.next_run_at.as_deref().and_then(parse_iso)
    }

    fn expires_ms(&self) -> Option<i64> {
        self.expires_at.as_deref().and_then(parse_iso)
    }

    fn completed_runs(&self) -> u64 {
        self.runs.iter().filter(|r| r.status != "running").count() as u64
    }

    fn finished(&self, now: i64) -> bool {
        self.expires_ms().is_some_and(|e| e <= now) || self.max_runs.is_some_and(|m| self.completed_runs() >= m)
    }

    fn complete(&mut self, now: i64) {
        self.status = "completed".into();
        self.next_run_at = None;
        self.paused_at = None;
        self.updated_at = iso(now);
    }

    fn push_run(&mut self, run: Run) {
        self.runs.push(run);
        let keep = RUN_HISTORY.max(self.max_runs.unwrap_or(0) as usize);
        let mut excess = self.runs.len().saturating_sub(keep);
        self.runs.retain(|r| {
            let remove = excess > 0 && r.status != "running";
            excess -= usize::from(remove);
            !remove
        });
    }

    /// The `ScheduleSummary` shape: everything but the runs.
    pub fn summary(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("runs");
        }
        v
    }
}

fn start_run(scheduled_for: i64, now: i64) -> Run {
    Run { id: new_id(), scheduled_for: iso(scheduled_for), started_at: iso(now), ended_at: None, status: "running".into(), agent_id: None, output: None, error: None }
}

/// A run that started an agent and waits for its turn to end.
#[derive(Clone)]
pub struct InFlight {
    pub schedule_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub manual: bool,
}

pub struct Outcome {
    pub agent_id: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    /// The agent or folder the schedule targets is gone for good: complete the schedule.
    pub target_gone: bool,
}

pub struct Schedules {
    path: PathBuf,
    pub list: Vec<Schedule>,
    pub in_flight: Vec<InFlight>,
}

fn parse_max_runs(v: Option<&Value>) -> Result<Option<u64>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| *n > 0).map(Some).ok_or_else(|| "maxRuns must be a positive integer".to_string()),
    }
}

fn parse_expires(v: Option<&Value>) -> Result<Option<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_str().filter(|t| parse_iso(t).is_some()).map(|t| Some(t.to_string())).ok_or_else(|| "expiresAt must be an ISO 8601 timestamp".to_string()),
    }
}

fn trimmed(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|t| !t.is_empty()).map(String::from)
}

/// Normalizes a request target (`self` is an agent target) and checks a new-agent config.
fn parse_target(target: Option<&Value>) -> Result<Value, String> {
    let target = target.ok_or("Schedule target is required")?;
    match s(target, "type").as_deref() {
        Some("self" | "agent") => {
            let agent_id = s(target, "agentId").filter(|id| !id.is_empty()).ok_or("agentId is required")?;
            Ok(json!({ "type": "agent", "agentId": agent_id }))
        }
        Some("new-agent") => {
            let config = target.get("config").filter(|c| c.is_object()).ok_or("new-agent targets need a config")?;
            let provider = s(config, "provider").unwrap_or_default();
            if !is_supported(&provider) {
                return Err(format!("provider '{provider}' is not supported (claude, codex)"));
            }
            if config.get("cwd").and_then(trimmed).is_none() {
                return Err("cwd is required".into());
            }
            if s(config, "isolation").as_deref() == Some("worktree") {
                return Err("feiyi does not create git worktrees yet; choose local isolation".into());
            }
            Ok(json!({ "type": "new-agent", "config": config }))
        }
        _ => Err("Unknown schedule target type".into()),
    }
}

fn new_schedule(msg: &Value, now: i64) -> Result<Schedule, String> {
    let prompt = msg.get("prompt").and_then(trimmed).ok_or("Schedule prompt is required")?;
    let cadence: Cadence = serde_json::from_value(msg.get("cadence").cloned().unwrap_or_default()).map_err(|e| format!("Invalid cadence: {e}"))?;
    cadence.validate()?;
    let target = parse_target(msg.get("target"))?;
    let run_now = msg.get("runOnCreate").and_then(Value::as_bool).unwrap_or(matches!(cadence, Cadence::Every { .. }));
    let next = if run_now { now } else { cadence.next_run(now, now)? };
    Ok(Schedule {
        id: new_id(),
        name: msg.get("name").and_then(trimmed),
        prompt,
        cadence,
        target,
        status: "active".into(),
        created_at: iso(now),
        updated_at: iso(now),
        next_run_at: Some(iso(next)),
        last_run_at: None,
        paused_at: None,
        expires_at: parse_expires(msg.get("expiresAt"))?,
        max_runs: parse_max_runs(msg.get("maxRuns"))?,
        runs: Vec::new(),
    })
}

/// Applies `newAgentConfig`: empty or null clears model, mode and thinking.
fn patch_config(config: &mut serde_json::Map<String, Value>, patch: &Value) -> Result<(), String> {
    for key in ["provider", "cwd"] {
        if let Some(v) = patch.get(key) {
            config.insert(key.into(), json!(trimmed(v).ok_or_else(|| format!("{key} cannot be empty"))?));
        }
    }
    for key in ["model", "modeId", "thinkingOptionId"] {
        if let Some(v) = patch.get(key) {
            match trimmed(v) {
                Some(t) => config.insert(key.into(), json!(t)),
                None => config.remove(key),
            };
        }
    }
    for key in ["archiveOnFinish", "isolation"] {
        if let Some(v) = patch.get(key).filter(|v| !v.is_null()) {
            config.insert(key.into(), v.clone());
        }
    }
    Ok(())
}

impl Schedules {
    pub fn load(home: &Path, now: i64) -> Schedules {
        let path = home.join("schedules.json");
        let list = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                crate::warn!("schedules.json unreadable ({e}); kept as schedules.json.bad, starting empty");
                let _ = std::fs::rename(&path, home.join("schedules.json.bad"));
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let mut schedules = Schedules { path, list, in_flight: Vec::new() };
        if schedules.recover(now) {
            schedules.save();
        }
        schedules
    }

    /// Fails the runs a restart interrupted and moves overdue schedules past `now`.
    fn recover(&mut self, now: i64) -> bool {
        let mut dirty = false;
        for s in &mut self.list {
            let mut touched = false;
            for run in s.runs.iter_mut().filter(|r| r.status == "running") {
                run.status = "failed".into();
                run.ended_at = Some(iso(now));
                run.error = Some("Daemon restarted before the scheduled run completed".into());
                touched = true;
            }
            if s.status == "active" {
                if s.finished(now) {
                    s.complete(now);
                    touched = true;
                } else if let Some(next) = s.next_run_ms().filter(|n| *n <= now) {
                    match s.cadence.next_run(next, now) {
                        Ok(n) => s.next_run_at = Some(iso(n)),
                        Err(_) => s.complete(now),
                    }
                    touched = true;
                }
            }
            if touched {
                s.updated_at = iso(now);
                dirty = true;
            }
        }
        dirty
    }

    fn save(&self) {
        match serde_json::to_vec(&self.list) {
            Ok(bytes) => {
                if let Err(e) = write_atomic(&self.path, &bytes, true) {
                    crate::warn!("failed to save schedules: {e}");
                }
            }
            Err(e) => crate::warn!("failed to encode schedules: {e}"),
        }
    }

    pub fn get(&self, id: &str) -> Result<&Schedule, String> {
        self.list.iter().find(|s| s.id == id).ok_or_else(|| format!("Schedule not found: {id}"))
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Schedule, String> {
        self.list.iter_mut().find(|s| s.id == id).ok_or_else(|| format!("Schedule not found: {id}"))
    }

    fn busy(&self, id: &str) -> bool {
        self.in_flight.iter().any(|f| f.schedule_id == id)
    }

    pub fn add(&mut self, schedule: Schedule) -> Schedule {
        self.list.push(schedule.clone());
        self.save();
        schedule
    }

    pub fn delete(&mut self, id: &str) {
        self.list.retain(|s| s.id != id);
        self.in_flight.retain(|f| f.schedule_id != id);
        self.save();
    }

    pub fn pause(&mut self, id: &str, now: i64) -> Result<Schedule, String> {
        let s = self.get_mut(id)?;
        match s.status.as_str() {
            "completed" => return Err(format!("Schedule {id} is already completed")),
            "paused" => return Ok(s.clone()),
            _ => {}
        }
        s.status = "paused".into();
        s.next_run_at = None;
        s.paused_at = Some(iso(now));
        s.updated_at = iso(now);
        let out = s.clone();
        self.save();
        Ok(out)
    }

    pub fn resume(&mut self, id: &str, now: i64) -> Result<Schedule, String> {
        let s = self.get_mut(id)?;
        match s.status.as_str() {
            "completed" => return Err(format!("Schedule {id} is already completed")),
            "active" => return Ok(s.clone()),
            _ => {}
        }
        let next = s.cadence.next_run(now, now)?;
        s.status = "active".into();
        s.paused_at = None;
        s.next_run_at = Some(iso(next));
        s.updated_at = iso(now);
        let out = s.clone();
        self.save();
        Ok(out)
    }

    /// Applies a `schedule/update` request; nothing changes when any field is invalid.
    pub fn update(&mut self, id: &str, msg: &Value, now: i64) -> Result<Schedule, String> {
        let mut s = self.get(id)?.clone();
        if let Some(prompt) = msg.get("prompt") {
            s.prompt = trimmed(prompt).ok_or("Schedule prompt is required")?;
        }
        if let Some(name) = msg.get("name") {
            s.name = trimmed(name);
        }
        if let Some(c) = msg.get("cadence") {
            let mut cadence: Cadence = serde_json::from_value(c.clone()).map_err(|e| format!("Invalid cadence: {e}"))?;
            if let (Cadence::Cron { timezone, .. }, Cadence::Cron { timezone: Some(old), .. }) = (&mut cadence, &s.cadence) {
                if timezone.is_none() {
                    *timezone = Some(old.clone());
                }
            }
            cadence.validate()?;
            s.next_run_at = (s.status == "active").then(|| cadence.next_run(now, now)).transpose()?.map(iso);
            s.cadence = cadence;
        }
        if let Some(patch) = msg.get("newAgentConfig") {
            if s.target["type"] != "new-agent" {
                return Err("new-agent config updates are only valid for new-agent target schedules".into());
            }
            if let Some(config) = s.target["config"].as_object_mut() {
                patch_config(config, patch)?;
            }
            s.target = parse_target(Some(&s.target))?;
        }
        if let Some(v) = msg.get("maxRuns") {
            s.max_runs = parse_max_runs(Some(v))?;
        }
        if let Some(v) = msg.get("expiresAt") {
            s.expires_at = parse_expires(Some(v))?;
        }
        s.updated_at = iso(now);
        *self.get_mut(id)? = s.clone();
        self.save();
        Ok(s)
    }

    /// The next moment `due` has something to do (a run, or an expiry).
    pub fn next_due(&self) -> Option<i64> {
        self.list
            .iter()
            .filter(|s| s.status == "active" && !self.busy(&s.id))
            .filter_map(|s| {
                let next = s.next_run_ms()?;
                Some(s.expires_ms().map_or(next, |e| e.min(next)))
            })
            .min()
    }

    /// Completes schedules that ran out, and lists those whose run is due.
    /// A schedule whose previous run is still going waits for it.
    pub fn due(&mut self, now: i64) -> Vec<String> {
        let mut due = Vec::new();
        let mut changed = false;
        for s in &mut self.list {
            let Some(next) = s.next_run_ms().filter(|_| s.status == "active") else { continue };
            if self.in_flight.iter().any(|f| f.schedule_id == s.id) {
                continue;
            }
            if s.finished(now) {
                s.complete(now);
                changed = true;
            } else if next <= now {
                due.push(s.id.clone());
            }
        }
        if changed {
            self.save();
        }
        due
    }

    /// Records a due run and moves the schedule to its next slot.
    pub fn claim(&mut self, id: &str, now: i64) -> Option<(Schedule, String)> {
        let s = self.list.iter_mut().find(|s| s.id == id)?;
        let slot = s.next_run_ms().filter(|slot| *slot <= now && s.status == "active" && !s.finished(now))?;
        let last = s.max_runs.is_some_and(|m| s.completed_runs() + 1 >= m);
        s.next_run_at = if last { None } else { s.cadence.next_run(slot, now).ok().map(iso) };
        let run = start_run(slot, now);
        let run_id = run.id.clone();
        s.push_run(run);
        s.updated_at = iso(now);
        let out = s.clone();
        self.save();
        Some((out, run_id))
    }

    /// Records a run-now run; the cadence is untouched.
    pub fn claim_manual(&mut self, id: &str, now: i64) -> Result<(Schedule, String), String> {
        if self.busy(id) {
            return Err(format!("Schedule {id} is already running"));
        }
        let s = self.get_mut(id)?;
        if s.status == "completed" {
            return Err(format!("Schedule {id} is already completed"));
        }
        let run = start_run(now, now);
        let run_id = run.id.clone();
        s.push_run(run);
        s.updated_at = iso(now);
        let out = s.clone();
        self.save();
        Ok((out, run_id))
    }

    /// The run started `agent_id`; it ends when that agent's turn does.
    pub fn track(&mut self, schedule_id: &str, run_id: &str, agent_id: String, manual: bool) {
        if let Some(run) = self.list.iter_mut().find(|s| s.id == schedule_id).and_then(|s| s.runs.iter_mut().find(|r| r.id == run_id)) {
            run.agent_id = Some(agent_id.clone());
        }
        self.in_flight.push(InFlight { schedule_id: schedule_id.into(), run_id: run_id.into(), agent_id, manual });
        self.save();
    }

    pub fn finish(&mut self, id: &str, run_id: &str, outcome: Outcome, manual: bool, now: i64) {
        self.in_flight.retain(|f| f.run_id != run_id);
        let Some(s) = self.list.iter_mut().find(|s| s.id == id) else { return };
        let stamp = iso(now);
        let mut scheduled_for = now;
        if let Some(run) = s.runs.iter_mut().find(|r| r.id == run_id) {
            scheduled_for = parse_iso(&run.scheduled_for).unwrap_or(now);
            run.status = if outcome.error.is_none() { "succeeded" } else { "failed" }.into();
            run.ended_at = Some(stamp.clone());
            if outcome.agent_id.is_some() {
                run.agent_id = outcome.agent_id;
            }
            run.output = outcome.output;
            run.error = outcome.error;
        }
        s.last_run_at = Some(stamp.clone());
        s.updated_at = stamp;
        if outcome.target_gone {
            s.complete(now);
        } else if s.status == "completed" || manual {
            // Manual runs leave the cadence alone, and a completed schedule stays completed.
        } else if s.finished(now) {
            s.complete(now);
        } else if s.status == "paused" {
            s.next_run_at = None;
        } else {
            let next = match s.next_run_ms() {
                Some(cursor) if cursor > now => Ok(cursor),
                Some(cursor) => s.cadence.next_run(cursor, now),
                None => s.cadence.next_run(scheduled_for, now),
            };
            match next {
                Ok(n) => s.next_run_at = Some(iso(n)),
                Err(_) => s.complete(now),
            }
        }
        self.save();
    }
}

fn fire_body(schedule: &Schedule, run_id: &str) -> String {
    let heading = match &schedule.name {
        Some(name) => format!("Schedule \"{name}\" fired (id={}, run={run_id}).", schedule.id),
        None => format!("Schedule fired (id={}, run={run_id}).", schedule.id),
    };
    format!("{heading}\n{}", schedule.prompt)
}

impl Manager {
    /// When the manager should next wake to run a schedule.
    pub(crate) fn schedule_deadline(&self) -> Option<Instant> {
        let due = self.schedules.next_due()?;
        Some(Instant::now() + Duration::from_millis((due - now_ms()).clamp(0, MAX_SLEEP_MS) as u64))
    }

    pub(crate) fn run_due_schedules(&mut self) {
        let now = now_ms();
        for id in self.schedules.due(now) {
            if let Some((schedule, run_id)) = self.schedules.claim(&id, now) {
                self.start_scheduled_run(&schedule, run_id, false);
            }
        }
    }

    fn start_scheduled_run(&mut self, schedule: &Schedule, run_id: String, manual: bool) {
        match self.launch_scheduled_run(schedule, &run_id) {
            Ok(agent_id) => self.schedules.track(&schedule.id, &run_id, agent_id, manual),
            Err((error, target_gone)) => {
                crate::warn!("schedule {} run failed: {error}", schedule.id);
                self.schedules.finish(&schedule.id, &run_id, Outcome { agent_id: None, output: None, error: Some(error), target_gone }, manual, now_ms());
            }
        }
    }

    /// Sends the prompt to the target agent, or creates the agent for it.
    /// Errors carry whether the target is gone for good.
    fn launch_scheduled_run(&mut self, schedule: &Schedule, run_id: &str) -> Result<String, (String, bool)> {
        let target = &schedule.target;
        if target["type"] == "agent" {
            let id = s(target, "agentId").unwrap_or_default();
            let agent = self.agents.get(&id).ok_or_else(|| (format!("Agent {id} no longer exists"), true))?;
            if agent.rec.archived_at.is_some() {
                return Err((format!("Agent {id} is archived"), true));
            }
            if agent.status == "running" {
                return Err((format!("Agent {id} already has an active run"), false));
            }
            self.require_room(&agent.rec.provider)?;
            return self.submit_prompt(&id, fire_body(schedule, run_id), None).map(|_| id).map_err(|e| (e, false));
        }
        let config = &target["config"];
        normalize_dir(&s(config, "cwd").unwrap_or_default()).map_err(|e| (format!("Working directory {e}"), true))?;
        self.require_room(&s(config, "provider").unwrap_or_default())?;
        let request = json!({ "config": config, "initialPrompt": schedule.prompt, "labels": { "paseo.schedule-id": schedule.id, "paseo.schedule-run": run_id } });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.create_agent(&request, "", &tx);
        let status: Value = rx.try_recv().ok().and_then(|m| serde_json::from_str(&m).ok()).unwrap_or_default();
        let payload = &status["message"]["payload"];
        match s(payload, "agentId") {
            Some(id) if payload["status"] == "agent_created" => Ok(id),
            _ => Err((s(payload, "error").unwrap_or_else(|| "agent was not created".into()), false)),
        }
    }

    /// A scheduled turn must be able to start now: it is skipped, not queued, when the CLI is full.
    fn require_room(&self, provider: &str) -> Result<(), (String, bool)> {
        let cli = crate::manager::cli_of(provider);
        let running = self.running.len();
        let queued = self.cli_queue.len() + self.claude.waiting_count();
        let other_cli_busy = running > 0 && self.active_cli.is_some_and(|active| active != cli);
        if other_cli_busy || queued > 0 || running >= self.slots(cli) {
            return Err((format!("Skipped: the daemon is at capacity ({running} running, {queued} waiting)"), false));
        }
        Ok(())
    }

    /// Ends the runs whose agent stopped; a finished run's agent is archived
    /// unless the schedule keeps it (`archiveOnFinish: false`).
    pub(crate) fn settle_schedule_runs(&mut self) {
        for flight in self.schedules.in_flight.clone() {
            let id = &flight.agent_id;
            let (error, done) = match self.agents.get(id) {
                None => (Some("Agent no longer exists".to_string()), true),
                Some(a) if !a.permissions.is_empty() => (Some(format!("Scheduled agent {id} is waiting for permission")), false),
                Some(a) if a.status == "running" => continue,
                Some(a) if a.status == "error" => (Some(a.rec.last_error.clone().unwrap_or_else(|| format!("Scheduled agent {id} failed"))), true),
                Some(_) => (None, true),
            };
            let output = if error.is_none() { self.last_assistant_message(id).map(|t| truncate(&t, OUTPUT_MAX)) } else { None };
            let archive = done && self.schedules.get(&flight.schedule_id).is_ok_and(|s| s.target["type"] == "new-agent" && s.target["config"]["archiveOnFinish"] != false);
            self.schedules.finish(&flight.schedule_id, &flight.run_id, Outcome { agent_id: Some(id.clone()), output, error, target_gone: false }, flight.manual, now_ms());
            if archive {
                self.release_runtime(id);
                if let Some(a) = self.agents.get_mut(id) {
                    a.rec.archived_at = Some(now_iso());
                    a.rec.attention_reason = None;
                }
                self.touch(id);
                self.publish_agent(id);
            }
        }
    }

    pub(crate) fn schedule_rpc(&mut self, kind: &str, msg: &Value, rid: &str, out: &Out) {
        let now = now_ms();
        let id = s(msg, "scheduleId").unwrap_or_default();
        let result = match kind {
            "schedule/create" => new_schedule(msg, now).map(|schedule| json!({ "schedule": self.schedules.add(schedule).summary() })),
            "schedule/list" => Ok(json!({ "schedules": self.schedules.list.iter().map(Schedule::summary).collect::<Vec<_>>() })),
            "schedule/inspect" => self.schedules.get(&id).map(|schedule| json!({ "schedule": schedule })),
            "schedule/logs" => self.schedules.get(&id).map(|schedule| json!({ "runs": schedule.runs })),
            "schedule/pause" => self.schedules.pause(&id, now).map(|schedule| json!({ "schedule": schedule.summary() })),
            "schedule/resume" => self.schedules.resume(&id, now).map(|schedule| json!({ "schedule": schedule.summary() })),
            "schedule/delete" => {
                self.schedules.delete(&id);
                Ok(json!({ "scheduleId": id }))
            }
            "schedule/update" => self.schedules.update(&id, msg, now).map(|schedule| json!({ "schedule": schedule })),
            // Answers once the run has started; the run's end shows up in the schedule's runs.
            "schedule/run-once" => self.schedules.claim_manual(&id, now).and_then(|(schedule, run_id)| {
                self.start_scheduled_run(&schedule, run_id, true);
                self.schedules.get(&id).map(|schedule| json!({ "schedule": schedule }))
            }),
            _ => Err("not supported by feiyi".to_string()),
        };
        match result {
            Ok(mut payload) => {
                payload["requestId"] = json!(rid);
                payload["error"] = Value::Null;
                reply(out, json!({ "type": format!("{kind}/response"), "payload": payload }));
            }
            Err(error) => reply(out, json!({ "type": "rpc_error", "payload": { "requestId": rid, "requestType": kind, "error": error, "code": "schedule_request_failed" } })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    const T0: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z

    fn request(extra: Value) -> Value {
        let mut msg = json!({ "type": "schedule/create", "requestId": "r", "prompt": "check the build", "cadence": { "type": "cron", "expression": "*/5 * * * *" }, "target": { "type": "new-agent", "config": { "provider": "claude", "cwd": "/tmp" } } });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            msg[k] = v;
        }
        msg
    }

    fn store(tag: &str) -> Schedules {
        let dir = std::env::temp_dir().join(format!("feiyi-schedules-{tag}-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Schedules::load(&dir, T0)
    }

    fn outcome(error: Option<&str>) -> Outcome {
        Outcome { agent_id: Some("agent-1".into()), output: Some("done".into()), error: error.map(String::from), target_gone: false }
    }

    #[test]
    fn create_defaults_follow_the_cadence() {
        let cron = new_schedule(&request(json!({})), T0 + 2 * MIN).unwrap();
        assert_eq!(cron.next_run_at.as_deref(), Some(iso(T0 + 5 * MIN).as_str()));
        assert_eq!((cron.status.as_str(), cron.name, cron.max_runs), ("active", None, None));
        let every = new_schedule(&request(json!({ "cadence": { "type": "every", "everyMs": 1000 } })), T0).unwrap();
        assert_eq!(every.next_run_at.as_deref(), Some(iso(T0).as_str()));
        let now = new_schedule(&request(json!({ "runOnCreate": true })), T0 + 2 * MIN).unwrap();
        assert_eq!(now.next_run_at.as_deref(), Some(iso(T0 + 2 * MIN).as_str()));
    }

    #[test]
    fn create_rejects_bad_input() {
        let bad = |extra: Value| new_schedule(&request(extra), T0).unwrap_err();
        assert_eq!(bad(json!({ "prompt": "  " })), "Schedule prompt is required");
        assert!(bad(json!({ "cadence": { "type": "cron", "expression": "* * *" } })).contains("5 fields"));
        assert!(bad(json!({ "cadence": { "type": "weekly" } })).starts_with("Invalid cadence"));
        assert_eq!(bad(json!({ "maxRuns": 0 })), "maxRuns must be a positive integer");
        assert!(bad(json!({ "expiresAt": "tomorrow" })).starts_with("expiresAt"));
        assert!(bad(json!({ "target": { "type": "new-agent", "config": { "provider": "gemini", "cwd": "/tmp" } } })).contains("not supported"));
        assert_eq!(bad(json!({ "target": { "type": "new-agent", "config": { "provider": "claude", "cwd": " " } } })), "cwd is required");
        assert!(bad(json!({ "target": { "type": "new-agent", "config": { "provider": "claude", "cwd": "/tmp", "isolation": "worktree" } } })).contains("worktrees"));
        assert_eq!(bad(json!({ "target": { "type": "agent" } })), "agentId is required");
    }

    #[test]
    fn self_target_becomes_an_agent_target() {
        let s = new_schedule(&request(json!({ "name": " nightly ", "target": { "type": "self", "agentId": "a1" } })), T0).unwrap();
        assert_eq!(s.target, json!({ "type": "agent", "agentId": "a1" }));
        assert_eq!(s.name.as_deref(), Some("nightly"));
    }

    #[test]
    fn summary_has_upstream_keys_and_no_runs() {
        let v = new_schedule(&request(json!({})), T0).unwrap().summary();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["cadence", "createdAt", "expiresAt", "id", "lastRunAt", "maxRuns", "name", "nextRunAt", "pausedAt", "prompt", "status", "target", "updatedAt"]);
        assert!(v["name"].is_null() && v["expiresAt"].is_null());
    }

    #[test]
    fn due_claim_and_finish_advance_the_schedule() {
        let mut st = store("claim");
        let id = st.add(new_schedule(&request(json!({})), T0).unwrap()).id;
        assert_eq!(st.next_due(), Some(T0 + 5 * MIN));
        assert!(st.due(T0 + 4 * MIN).is_empty());
        assert_eq!(st.due(T0 + 5 * MIN), [id.clone()]);
        let (s, run_id) = st.claim(&id, T0 + 5 * MIN + 1000).unwrap();
        assert_eq!(s.next_run_at.as_deref(), Some(iso(T0 + 10 * MIN).as_str()));
        assert_eq!((s.runs.len(), s.runs[0].status.as_str()), (1, "running"));
        assert_eq!(s.runs[0].scheduled_for, iso(T0 + 5 * MIN));
        assert!(st.claim(&id, T0 + 5 * MIN + 2000).is_none());
        st.finish(&id, &run_id, outcome(None), false, T0 + 5 * MIN + 3000);
        let s = st.get(&id).unwrap();
        assert_eq!((s.runs[0].status.as_str(), s.runs[0].output.as_deref(), s.runs[0].agent_id.as_deref()), ("succeeded", Some("done"), Some("agent-1")));
        assert_eq!(s.last_run_at.as_deref(), Some(iso(T0 + 5 * MIN + 3000).as_str()));
        assert_eq!(s.next_run_at.as_deref(), Some(iso(T0 + 10 * MIN).as_str()));
    }

    #[test]
    fn missed_slots_are_skipped_not_replayed() {
        let mut st = store("missed");
        let id = st.add(new_schedule(&request(json!({})), T0).unwrap()).id;
        let late = T0 + 62 * MIN;
        assert_eq!(st.due(late), [id.clone()]);
        let (s, _) = st.claim(&id, late).unwrap();
        assert_eq!(s.next_run_at.as_deref(), Some(iso(T0 + 65 * MIN).as_str()));
        assert_eq!(st.due(late), Vec::<String>::new());
    }

    #[test]
    fn a_running_schedule_waits_for_its_run() {
        let mut st = store("busy");
        let id = st.add(new_schedule(&request(json!({ "cadence": { "type": "every", "everyMs": 60000 } })), T0).unwrap()).id;
        let (_, run_id) = st.claim(&id, T0).unwrap();
        st.track(&id, &run_id, "agent-1".into(), false);
        assert_eq!(st.next_due(), None);
        assert!(st.due(T0 + 10 * MIN).is_empty());
        assert!(st.claim_manual(&id, T0 + MIN).unwrap_err().contains("already running"));
        st.finish(&id, &run_id, outcome(None), false, T0 + 10 * MIN);
        assert!(st.in_flight.is_empty());
        assert_eq!(st.get(&id).unwrap().next_run_at.as_deref(), Some(iso(T0 + 11 * MIN).as_str()));
    }

    #[test]
    fn max_runs_completes_the_schedule_after_the_last_run() {
        let mut st = store("max");
        let id = st.add(new_schedule(&request(json!({ "maxRuns": 2 })), T0).unwrap()).id;
        let (s, first) = st.claim(&id, T0 + 5 * MIN).unwrap();
        assert!(s.next_run_at.is_some());
        st.finish(&id, &first, outcome(Some("boom")), false, T0 + 5 * MIN);
        let (s, second) = st.claim(&id, T0 + 10 * MIN).unwrap();
        assert_eq!(s.next_run_at, None);
        assert_eq!(st.get(&id).unwrap().status, "active");
        st.finish(&id, &second, outcome(None), false, T0 + 10 * MIN);
        let s = st.get(&id).unwrap();
        assert_eq!((s.status.as_str(), s.next_run_at.clone()), ("completed", None));
        assert_eq!(s.runs[0].error.as_deref(), Some("boom"));
        assert_eq!(s.runs[0].status, "failed");
    }

    #[test]
    fn expiry_completes_without_running() {
        let mut st = store("expiry");
        let expires = iso(T0 + 7 * MIN);
        let id = st.add(new_schedule(&request(json!({ "expiresAt": expires })), T0).unwrap()).id;
        assert_eq!(st.next_due(), Some(T0 + 5 * MIN));
        assert_eq!(st.due(T0 + 5 * MIN), [id.clone()]);
        let (_, run_id) = st.claim(&id, T0 + 5 * MIN).unwrap();
        st.finish(&id, &run_id, outcome(None), false, T0 + 5 * MIN);
        assert_eq!(st.next_due(), Some(T0 + 7 * MIN));
        assert!(st.due(T0 + 7 * MIN).is_empty());
        assert_eq!(st.get(&id).unwrap().status, "completed");
        assert_eq!(st.next_due(), None);
    }

    #[test]
    fn a_gone_target_completes_the_schedule_and_manual_runs_keep_the_cadence() {
        let mut st = store("gone");
        let id = st.add(new_schedule(&request(json!({})), T0).unwrap()).id;
        let before = st.get(&id).unwrap().next_run_at.clone();
        let (_, run_id) = st.claim_manual(&id, T0 + MIN).unwrap();
        assert_eq!(st.get(&id).unwrap().next_run_at, before);
        st.finish(&id, &run_id, outcome(None), true, T0 + MIN);
        assert_eq!(st.get(&id).unwrap().next_run_at, before);
        let (_, run_id) = st.claim_manual(&id, T0 + 2 * MIN).unwrap();
        st.finish(&id, &run_id, Outcome { agent_id: None, output: None, error: Some("Agent a no longer exists".into()), target_gone: true }, true, T0 + 2 * MIN);
        assert_eq!(st.get(&id).unwrap().status, "completed");
        assert!(st.claim_manual(&id, T0 + 3 * MIN).unwrap_err().contains("already completed"));
    }

    #[test]
    fn pause_and_resume() {
        let mut st = store("pause");
        let id = st.add(new_schedule(&request(json!({})), T0).unwrap()).id;
        let paused = st.pause(&id, T0 + MIN).unwrap();
        assert_eq!((paused.status.as_str(), paused.next_run_at.clone()), ("paused", None));
        assert_eq!(paused.paused_at.as_deref(), Some(iso(T0 + MIN).as_str()));
        assert_eq!(st.next_due(), None);
        assert_eq!(st.pause(&id, T0 + 2 * MIN).unwrap().paused_at, paused.paused_at);
        let resumed = st.resume(&id, T0 + 6 * MIN).unwrap();
        assert_eq!((resumed.status.as_str(), resumed.paused_at.clone()), ("active", None));
        assert_eq!(resumed.next_run_at.as_deref(), Some(iso(T0 + 10 * MIN).as_str()));
        assert!(st.pause("nope", T0).unwrap_err().contains("Schedule not found"));
    }

    #[test]
    fn update_merges_the_timezone_patches_the_config_and_is_all_or_nothing() {
        let mut st = store("update");
        let zoned = request(json!({ "cadence": { "type": "cron", "expression": "0 9 * * *", "timezone": "UTC" } }));
        let id = st.add(new_schedule(&zoned, T0).unwrap()).id;
        let s = st.update(&id, &json!({ "cadence": { "type": "cron", "expression": "30 10 * * *" }, "name": "  ", "prompt": " new prompt ", "maxRuns": 3 }), T0).unwrap();
        assert_eq!(s.cadence, Cadence::Cron { expression: "30 10 * * *".into(), timezone: Some("UTC".into()) });
        assert_eq!(s.next_run_at.as_deref(), Some(iso(T0 + 10 * 60 * MIN + 30 * MIN).as_str()));
        assert_eq!((s.name, s.prompt.as_str(), s.max_runs), (None, "new prompt", Some(3)));

        let s = st.update(&id, &json!({ "newAgentConfig": { "model": "opus", "modeId": "plan", "archiveOnFinish": false }, "maxRuns": null }), T0).unwrap();
        assert_eq!(s.target["config"]["model"], "opus");
        assert_eq!((s.target["config"]["archiveOnFinish"] == false, s.max_runs), (true, None));
        let s = st.update(&id, &json!({ "newAgentConfig": { "model": null } }), T0).unwrap();
        assert!(s.target["config"].get("model").is_none());
        assert_eq!(s.target["config"]["modeId"], "plan");

        let before = st.get(&id).unwrap().clone();
        assert!(st.update(&id, &json!({ "prompt": "changed", "cadence": { "type": "cron", "expression": "bad" } }), T0).is_err());
        assert!(st.update(&id, &json!({ "newAgentConfig": { "isolation": "worktree" } }), T0).is_err());
        assert!(st.update(&id, &json!({ "newAgentConfig": { "cwd": "" } }), T0).is_err());
        let after = st.get(&id).unwrap();
        assert_eq!((after.prompt.as_str(), after.updated_at.as_str()), (before.prompt.as_str(), before.updated_at.as_str()));
        assert_eq!(after.target, before.target);

        let agent = st.add(new_schedule(&request(json!({ "target": { "type": "agent", "agentId": "a1" } })), T0).unwrap()).id;
        assert!(st.update(&agent, &json!({ "newAgentConfig": { "model": "x" } }), T0).unwrap_err().contains("new-agent"));
    }

    #[test]
    fn run_history_is_capped_but_never_drops_a_running_run() {
        let mut st = store("history");
        let id = st.add(new_schedule(&request(json!({ "cadence": { "type": "every", "everyMs": 1000 } })), T0).unwrap()).id;
        for i in 0..(RUN_HISTORY as i64 + 20) {
            let (_, run_id) = st.claim_manual(&id, T0 + i).unwrap();
            st.finish(&id, &run_id, outcome(None), true, T0 + i);
        }
        let (s, _) = st.claim_manual(&id, T0 + 1000).unwrap();
        assert_eq!(s.runs.len(), RUN_HISTORY);
        assert_eq!(s.runs.last().unwrap().status, "running");
    }

    #[test]
    fn schedules_survive_a_restart_and_interrupted_runs_fail() {
        let dir = std::env::temp_dir().join(format!("feiyi-schedules-restart-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = Schedules::load(&dir, T0);
        let id = st.add(new_schedule(&request(json!({ "name": "build" })), T0).unwrap()).id;
        let (_, run_id) = st.claim(&id, T0 + 5 * MIN).unwrap();
        st.track(&id, &run_id, "agent-1".into(), false);
        let paused = st.add(new_schedule(&request(json!({})), T0).unwrap()).id;
        st.pause(&paused, T0).unwrap();

        let restarted = Schedules::load(&dir, T0 + 60 * MIN);
        assert!(restarted.in_flight.is_empty());
        assert_eq!(restarted.list.len(), 2);
        let s = restarted.get(&id).unwrap();
        assert_eq!(s.name.as_deref(), Some("build"));
        assert_eq!((s.runs[0].status.as_str(), s.runs[0].error.as_deref()), ("failed", Some("Daemon restarted before the scheduled run completed")));
        assert_eq!(s.runs[0].agent_id.as_deref(), Some("agent-1"));
        assert_eq!(s.next_run_at.as_deref(), Some(iso(T0 + 65 * MIN).as_str()));
        assert_eq!(restarted.get(&paused).unwrap().status, "paused");

        let reread = Schedules::load(&dir, T0 + 61 * MIN);
        assert_eq!(reread.get(&id).unwrap().runs.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("feiyi-schedules-corrupt-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("schedules.json"), "{ not json").unwrap();
        assert!(Schedules::load(&dir, T0).list.is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("schedules.json.bad")).unwrap(), "{ not json");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deleting_forgets_the_schedule_and_its_in_flight_run() {
        let mut st = store("delete");
        let id = st.add(new_schedule(&request(json!({})), T0).unwrap()).id;
        let (_, run_id) = st.claim(&id, T0 + 5 * MIN).unwrap();
        st.track(&id, &run_id, "agent-1".into(), false);
        st.delete(&id);
        assert!(st.list.is_empty() && st.in_flight.is_empty());
        st.finish(&id, &run_id, outcome(None), false, T0 + 6 * MIN);
        assert!(st.get(&id).is_err());
    }

    #[test]
    fn fire_body_names_the_schedule() {
        let mut s = new_schedule(&request(json!({ "name": "nightly" })), T0).unwrap();
        assert_eq!(fire_body(&s, "r1"), format!("Schedule \"nightly\" fired (id={}, run=r1).\ncheck the build", s.id));
        s.name = None;
        assert!(fire_body(&s, "r1").starts_with("Schedule fired (id="));
    }
}
