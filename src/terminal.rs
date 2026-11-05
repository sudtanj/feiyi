//! Terminals for the app's terminal panes: shells on a PTY that outlive the
//! client connection (a reattaching client gets the last output back as a
//! `restore` frame). Terminal data travels in binary frames like upstream's
//! `@getpaseo/protocol/binary-frames/terminal`: `opcode, slot, payload`.
//! There is no screen emulator: the scrollback is a raw byte ring.

use crate::manager::{wrap, Cmd, Manager};
use crate::rpc::{normalize_dir, s};
use crate::session::Outbound;
use crate::util::{basename, new_id};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, watch};

const MAX_TERMINALS: usize = 4;
/// A terminal nobody is subscribed to is ended after this long.
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
const RING_BYTES: usize = 256 * 1024;
const FRAME_BYTES: usize = 64 * 1024;
const OP_OUTPUT: u8 = 0x01;
const OP_INPUT: u8 = 0x02;
const OP_RESIZE: u8 = 0x03;
const OP_RESTORE: u8 = 0x05;

/// A terminal-stream frame (opcodes 0x01..=0x05), as opposed to JSON or a file-transfer frame.
pub fn is_frame(bytes: &[u8]) -> bool {
    matches!(bytes.first(), Some(0x01..=0x05))
}

fn encode_frame(opcode: u8, slot: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + payload.len());
    bytes.extend([opcode, slot]);
    bytes.extend_from_slice(payload);
    bytes
}

fn decode_frame(bytes: &[u8]) -> Option<(u8, u8, &[u8])> {
    match bytes {
        [opcode, slot, payload @ ..] => Some((*opcode, *slot, payload)),
        _ => None,
    }
}

fn text(msg: Value) -> Outbound {
    Outbound::Text(wrap(msg))
}

/// The last `cap` bytes a terminal printed, and how many it printed in all.
struct Ring {
    buf: VecDeque<u8>,
    total: u64,
    cap: usize,
}

impl Ring {
    fn new(cap: usize) -> Ring {
        Ring { buf: VecDeque::new(), total: 0, cap }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend(data);
        let over = self.buf.len().saturating_sub(self.cap);
        self.buf.drain(..over);
        self.total += data.len() as u64;
    }

    /// Bytes printed after `cursor` (a `total` seen earlier); None when the ring already dropped some of them.
    fn since(&self, cursor: u64) -> Option<Vec<u8>> {
        let start = self.total - self.buf.len() as u64;
        (cursor >= start).then(|| self.buf.range((cursor - start) as usize..).copied().collect())
    }

    /// Everything kept, starting on a UTF-8 character boundary.
    fn all(&self) -> Vec<u8> {
        let cut = self.total > self.buf.len() as u64;
        self.buf.iter().copied().skip_while(|b| cut && b & 0xC0 == 0x80).collect()
    }
}

struct State {
    ring: Ring,
    exited: bool,
    title: Option<String>,
}

struct Term {
    id: String,
    name: String,
    cwd: String,
    workspace_id: Option<String>,
    pid: i32,
    io: AsyncFd<OwnedFd>,
    child: Mutex<Option<Child>>,
    state: Mutex<State>,
}

struct Registry {
    list: Mutex<Vec<Arc<Term>>>,
    /// Bumped whenever any terminal prints or ends; sessions wake on it.
    output: watch::Sender<u64>,
    /// Bumped when a terminal is added, removed or renamed.
    version: AtomicU64,
}

static TERMINALS: LazyLock<Registry> = LazyLock::new(|| Registry { list: Mutex::new(Vec::new()), output: watch::channel(0).0, version: AtomicU64::new(0) });

impl Registry {
    fn wake(&self) {
        self.output.send_modify(|n| *n += 1);
    }

    fn changed(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.wake();
    }

    fn get(&self, id: &str) -> Option<Arc<Term>> {
        self.list.lock().unwrap().iter().find(|t| t.id == id).cloned()
    }

    fn remove(&self, id: &str) -> Option<Arc<Term>> {
        let mut list = self.list.lock().unwrap();
        let at = list.iter().position(|t| t.id == id)?;
        let term = list.remove(at);
        drop(list);
        self.changed();
        Some(term)
    }
}

/// Wakes when any terminal has news.
pub fn watcher() -> watch::Receiver<u64> {
    TERMINALS.output.subscribe()
}

/// Kill every terminal's process group and reap the shells (daemon shutdown).
pub fn kill_all() {
    let all: Vec<Arc<Term>> = TERMINALS.list.lock().unwrap().drain(..).collect();
    for term in all {
        term.end();
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn set_flags(fd: RawFd, nonblocking: bool) -> std::io::Result<()> {
    unsafe {
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if nonblocking {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// A shell (or `command`) as session leader on a fresh PTY of `rows` x `cols`.
fn spawn(cwd: &str, command: Option<(&str, &[String])>, rows: u16, cols: u16) -> std::io::Result<(Child, AsyncFd<OwnedFd>)> {
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_flags(master.as_raw_fd(), true)?;
    set_flags(slave.as_raw_fd(), false)?;
    let master = AsyncFd::new(master)?;
    let mut cmd = match command {
        Some((program, args)) => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
        None => Command::new(std::env::var("SHELL").ok().filter(|sh| !sh.is_empty()).unwrap_or_else(|| "/bin/sh".into())),
    };
    cmd.current_dir(cwd).env("TERM", "xterm-256color").env("COLORTERM", "truecolor");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PASEO_") {
            cmd.env_remove(key);
        }
    }
    cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok((child, master))
}

impl Term {
    fn info(&self, with_cwd: bool) -> Value {
        let mut info = json!({ "id": self.id, "name": self.name, "activity": null });
        if with_cwd {
            info["cwd"] = json!(self.cwd);
        }
        if let Some(ws) = &self.workspace_id {
            info["workspaceId"] = json!(ws);
        }
        if let Some(title) = &self.state.lock().unwrap().title {
            info["title"] = json!(title);
        }
        info
    }

    fn push(&self, data: &[u8]) {
        self.state.lock().unwrap().ring.push(data);
        TERMINALS.wake();
    }

    fn resize(&self, size: &Value) {
        let dim = |key: &str| size.get(key).and_then(|v| v.as_f64()).filter(|n| *n >= 1.0).map(|n| n.min(1000.0) as u16);
        let (Some(rows), Some(cols)) = (dim("rows"), dim("cols")) else { return };
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.io.get_ref().as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
    }

    /// Typed input; dropped when the shell stops reading for a while.
    async fn write(&self, mut data: &[u8]) {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while !data.is_empty() {
                let Ok(mut guard) = self.io.writable().await else { return };
                let wrote = guard.try_io(|fd| {
                    let n = unsafe { libc::write(fd.get_ref().as_raw_fd(), data.as_ptr().cast(), data.len()) };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                });
                match wrote {
                    Ok(Ok(n)) => data = &data[n..],
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Ok(Err(_)) => return,
                    Err(_) => {}
                }
            }
        })
        .await;
    }

    /// Once the shell exits, its leftover group is killed and the child dropped,
    /// so `end` never signals a recycled pid.
    fn gone(&self) -> bool {
        let mut child = self.child.lock().unwrap();
        let alive = child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if !alive && child.take().is_some() {
            unsafe { libc::killpg(self.pid, libc::SIGKILL) };
        }
        !alive
    }

    /// Kill the whole process group, reap the shell, mark the terminal exited.
    fn end(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            unsafe { libc::killpg(self.pid, libc::SIGKILL) };
            let _ = child.wait();
        }
        self.state.lock().unwrap().exited = true;
        TERMINALS.wake();
    }
}

/// Reads the shell's output into the ring until it exits, then cleans up.
async fn pump(term: Arc<Term>) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut idle_since = Instant::now();
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ready = term.io.readable() => {
                let Ok(mut guard) = ready else { break };
                match guard.try_io(|fd| read_fd(fd.get_ref().as_raw_fd(), &mut buf)) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => term.push(&buf[..n]),
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
            _ = tick.tick() => {
                if term.gone() {
                    break;
                }
                // The registry and this task hold two references; any more are subscribers.
                if Arc::strong_count(&term) > 2 {
                    idle_since = Instant::now();
                } else if idle_since.elapsed() >= IDLE_LIMIT {
                    break;
                }
            }
        }
    }
    while let Ok(n @ 1..) = read_fd(term.io.get_ref().as_raw_fd(), &mut buf) {
        term.push(&buf[..n]);
    }
    term.end();
    TERMINALS.remove(&term.id);
}

fn create_term(msg: &Value, cwd: String, workspace_id: Option<String>) -> Result<Arc<Term>, String> {
    let size = msg.get("size").cloned().unwrap_or(Value::Null);
    let dim = |key: &str, default: u16| size.get(key).and_then(|v| v.as_u64()).filter(|n| *n >= 1).map(|n| n.min(1000) as u16).unwrap_or(default);
    let command = s(msg, "command").filter(|c| !c.trim().is_empty());
    let args: Vec<String> = msg.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let mut list = TERMINALS.list.lock().unwrap();
    if list.len() >= MAX_TERMINALS {
        return Err(format!("Terminal limit reached ({MAX_TERMINALS})"));
    }
    let (child, io) = spawn(&cwd, command.as_deref().map(|c| (c, args.as_slice())), dim("rows", 24), dim("cols", 80)).map_err(|e| format!("Failed to start terminal: {e}"))?;
    let name = s(msg, "name").filter(|n| !n.trim().is_empty()).or(command).map(|n| basename(&n)).unwrap_or_else(|| basename(&std::env::var("SHELL").unwrap_or_else(|_| "sh".into())));
    let term = Arc::new(Term {
        id: new_id(),
        name,
        cwd,
        workspace_id,
        pid: child.id() as i32,
        io,
        child: Mutex::new(Some(child)),
        state: Mutex::new(State { ring: Ring::new(RING_BYTES), exited: false, title: None }),
    });
    list.push(term.clone());
    drop(list);
    TERMINALS.changed();
    tokio::spawn(pump(term.clone()));
    Ok(term)
}

impl Manager {
    /// Canonical directory and workspace for a new terminal: the directory must be
    /// inside a project or an active workspace.
    pub fn terminal_target(&self, cwd: &str, workspace_id: Option<&str>) -> Result<(String, Option<String>), String> {
        let dir = normalize_dir(cwd)?;
        let live = || self.workspaces.iter().filter(|w| w.archived_at.is_none());
        if let Some(id) = workspace_id {
            if !live().any(|w| w.id == id) {
                return Err(format!("Workspace {id} is not active or does not exist"));
            }
        }
        let known = live().any(|w| Path::new(&dir).starts_with(&w.directory)) || self.projects.iter().any(|p| Path::new(&dir).starts_with(&p.root_path));
        if !known {
            return Err(format!("{dir} is not in a known project or workspace"));
        }
        let workspace = workspace_id.map(String::from).or_else(|| live().find(|w| w.directory == dir).map(|w| w.id.clone()));
        Ok((dir, workspace))
    }
}

/// End every terminal of a workspace that is being archived.
pub fn end_workspace(workspace_id: &str) {
    for term in matching(None, Some(workspace_id)) {
        term.end();
    }
}

/// Terminals in `cwd` (or its subdirectories) or in `workspace_id`; all when neither is given.
fn matching(cwd: Option<&str>, workspace_id: Option<&str>) -> Vec<Arc<Term>> {
    let root = cwd.map(|c| std::fs::canonicalize(c).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| c.to_string()));
    let list = TERMINALS.list.lock().unwrap();
    list.iter()
        .filter(|t| match (workspace_id, &root) {
            (Some(id), _) => t.workspace_id.as_deref() == Some(id),
            (None, Some(root)) => Path::new(&t.cwd).starts_with(root),
            (None, None) => true,
        })
        .cloned()
        .collect()
}

struct Sub {
    id: String,
    slot: u8,
    term: Arc<Term>,
    /// `ring.total` already sent to the client.
    cursor: u64,
}

struct DirSub {
    id: String,
    cwd: String,
    workspace_id: Option<String>,
    last: String,
}

/// One connection's terminal subscriptions.
pub struct Attached {
    subs: Vec<Sub>,
    dirs: Vec<DirSub>,
    seen_version: u64,
}

fn changed_message(id: &str, cwd: &str, workspace_id: Option<&str>, request_id: Option<&str>) -> Value {
    let terminals: Vec<Value> = matching(Some(cwd), workspace_id).iter().map(|t| t.info(false)).collect();
    let mut payload = json!({ "subscriptionId": id, "cwd": cwd, "terminals": terminals });
    if let Some(ws) = workspace_id {
        payload["workspaceId"] = json!(ws);
    }
    if let Some(rid) = request_id {
        payload["requestId"] = json!(rid);
    }
    json!({ "type": "terminals_changed", "payload": payload })
}

impl Attached {
    pub fn new() -> Attached {
        Attached { subs: Vec::new(), dirs: Vec::new(), seen_version: TERMINALS.version.load(Ordering::Relaxed) }
    }

    /// New output as frames, exits of terminals this connection watches, and changed terminal lists.
    pub fn pump(&mut self) -> Vec<Outbound> {
        let mut out = Vec::new();
        let mut finished = Vec::new();
        for (at, sub) in self.subs.iter_mut().enumerate() {
            let state = sub.term.state.lock().unwrap();
            match state.ring.since(sub.cursor) {
                Some(data) => out.extend(data.chunks(FRAME_BYTES).map(|chunk| Outbound::Binary(encode_frame(OP_OUTPUT, sub.slot, chunk)))),
                None => out.push(Outbound::Binary(encode_frame(OP_RESTORE, sub.slot, &state.ring.all()))),
            }
            sub.cursor = state.ring.total;
            if state.exited {
                finished.push(at);
            }
        }
        for at in finished.into_iter().rev() {
            let sub = self.subs.remove(at);
            out.push(text(json!({ "type": "terminal_stream_exit", "payload": { "subscriptionId": sub.id, "terminalId": sub.term.id } })));
        }
        let version = TERMINALS.version.load(Ordering::Relaxed);
        if version != self.seen_version {
            self.seen_version = version;
            for dir in &mut self.dirs {
                let msg = changed_message(&dir.id, &dir.cwd, dir.workspace_id.as_deref(), None);
                let json = msg.to_string();
                if json != dir.last {
                    dir.last = json;
                    out.push(text(msg));
                }
            }
        }
        out
    }

    /// Binary input and resize frames from the client, addressed by slot.
    pub async fn binary(&self, bytes: &[u8]) {
        let Some((opcode, slot, payload)) = decode_frame(bytes) else { return };
        let Some(sub) = self.subs.iter().find(|sub| sub.slot == slot) else { return };
        match opcode {
            OP_INPUT => sub.term.write(payload).await,
            OP_RESIZE => {
                if let Ok(size) = serde_json::from_slice::<Value>(payload) {
                    sub.term.resize(&size);
                }
            }
            _ => {}
        }
    }

    /// Replies for a terminal request; None when `msg` is not one.
    pub async fn handle(&mut self, msg: &Value, cmds: &mpsc::UnboundedSender<Cmd>) -> Option<Vec<Outbound>> {
        let kind = s(msg, "type")?;
        let rid = s(msg, "requestId").unwrap_or_default();
        let id = s(msg, "terminalId").unwrap_or_default();
        let reply = match kind.as_str() {
            "create_terminal_request" => create(msg, &rid, cmds).await,
            "list_terminals_request" => {
                let cwd = s(msg, "cwd");
                let terminals: Vec<Value> = matching(cwd.as_deref(), s(msg, "workspaceId").as_deref()).iter().map(|t| t.info(true)).collect();
                let mut payload = json!({ "terminals": terminals, "requestId": rid });
                if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
                    payload["cwd"] = json!(cwd);
                }
                json!({ "type": "list_terminals_response", "payload": payload })
            }
            "subscribe_terminals_request" => {
                let (sub_id, cwd, workspace_id) = (new_id(), s(msg, "cwd").unwrap_or_default(), s(msg, "workspaceId"));
                let reply = changed_message(&sub_id, &cwd, workspace_id.as_deref(), s(msg, "requestId").as_deref());
                let mut last = reply.clone();
                last["payload"].as_object_mut()?.remove("requestId");
                self.dirs.push(DirSub { id: sub_id, cwd, workspace_id, last: last.to_string() });
                reply
            }
            "unsubscribe_terminals_request" => {
                let cwd = s(msg, "cwd").unwrap_or_default();
                self.dirs.retain(|d| d.cwd != cwd);
                return Some(Vec::new());
            }
            "subscribe_terminal_request" => return Some(self.subscribe(msg, &rid, &id)),
            "unsubscribe_terminal_request" => {
                self.subs.retain(|sub| sub.term.id != id);
                return Some(Vec::new());
            }
            "subscription.release.request" => {
                let sub_id = s(msg, "subscriptionId").unwrap_or_default();
                self.subs.retain(|sub| sub.id != sub_id);
                self.dirs.retain(|d| d.id != sub_id);
                json!({ "type": "subscription.release.response", "payload": { "requestId": rid, "subscriptionId": sub_id } })
            }
            "terminal_input" => {
                if let (Some(term), Some(input)) = (TERMINALS.get(&id), msg.get("message")) {
                    match input.get("type").and_then(|t| t.as_str()) {
                        Some("input") => term.write(s(input, "data").unwrap_or_default().as_bytes()).await,
                        Some("resize") => term.resize(input),
                        _ => {}
                    }
                }
                return Some(Vec::new());
            }
            "kill_terminal_request" => {
                let term = TERMINALS.remove(&id);
                let success = term.is_some();
                if let Some(term) = term {
                    term.end();
                }
                json!({ "type": "kill_terminal_response", "payload": { "terminalId": id, "success": success, "requestId": rid } })
            }
            "terminal.rename.request" => {
                let title = s(msg, "title").unwrap_or_default().trim().to_string();
                let term = TERMINALS.get(&id);
                let error = match &term {
                    _ if title.is_empty() => Some("Title is required"),
                    _ if title.chars().count() > 200 => Some("Title is too long"),
                    None => Some("Terminal not found"),
                    Some(_) => None,
                };
                if let (None, Some(term)) = (error, term) {
                    term.state.lock().unwrap().title = Some(title);
                    TERMINALS.changed();
                }
                json!({ "type": "terminal.rename.response", "payload": { "requestId": rid, "success": error.is_none(), "error": error } })
            }
            _ => return None,
        };
        Some(vec![text(reply)])
    }

    fn subscribe(&mut self, msg: &Value, rid: &str, id: &str) -> Vec<Outbound> {
        let fail = |error: &str| vec![text(json!({ "type": "subscribe_terminal_response", "payload": { "terminalId": id, "error": error, "requestId": rid } }))];
        let Some(term) = TERMINALS.get(id) else { return fail("Terminal not found") };
        self.subs.retain(|sub| sub.term.id != id);
        let Some(slot) = (0..=255u8).find(|slot| !self.subs.iter().any(|sub| sub.slot == *slot)) else { return fail("No terminal stream slots available") };
        let restore = msg.get("restore");
        if let Some(size) = restore.and_then(|r| r.get("size")) {
            term.resize(size);
        }
        let sub_id = new_id();
        let mut out = vec![text(json!({ "type": "subscribe_terminal_response", "payload": { "terminalId": id, "slot": slot, "subscriptionId": sub_id, "error": null, "requestId": rid } }))];
        let state = term.state.lock().unwrap();
        if restore.and_then(|r| r.get("mode")).and_then(|m| m.as_str()) != Some("live") {
            out.push(Outbound::Binary(encode_frame(OP_RESTORE, slot, &state.ring.all())));
        }
        let cursor = state.ring.total;
        drop(state);
        self.subs.push(Sub { id: sub_id, slot, term, cursor });
        out
    }
}

async fn create(msg: &Value, rid: &str, cmds: &mpsc::UnboundedSender<Cmd>) -> Value {
    let fail = |error: String| json!({ "type": "create_terminal_response", "payload": { "terminal": null, "error": error, "requestId": rid } });
    if let Some(agent) = s(msg, "agentId") {
        return fail(format!("Agent-backed terminals are not supported (agent {agent})"));
    }
    let (reply, answer) = oneshot::channel();
    if cmds.send(Cmd::TerminalTarget { cwd: s(msg, "cwd").unwrap_or_default(), workspace_id: s(msg, "workspaceId"), reply }).is_err() {
        return fail("daemon is shutting down".into());
    }
    let (cwd, workspace_id) = match tokio::time::timeout(Duration::from_secs(5), answer).await {
        Ok(Ok(Ok(target))) => target,
        Ok(Ok(Err(e))) => return fail(e),
        _ => return fail("daemon did not answer".into()),
    };
    match create_term(msg, cwd, workspace_id) {
        Ok(term) => json!({ "type": "create_terminal_response", "payload": { "terminal": term.info(true), "error": null, "requestId": rid } }),
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_bytes_and_counts_all() {
        let mut ring = Ring::new(8);
        ring.push(b"abcdef");
        assert_eq!(ring.since(0).unwrap(), b"abcdef");
        ring.push(b"ghij");
        assert_eq!(ring.total, 10);
        assert_eq!(ring.all(), b"cdefghij");
        assert_eq!(ring.since(6).unwrap(), b"ghij");
        assert_eq!(ring.since(10).unwrap(), b"");
    }

    #[test]
    fn ring_reports_a_reader_that_fell_behind() {
        let mut ring = Ring::new(4);
        ring.push(b"abcdefgh");
        assert!(ring.since(3).is_none());
        assert_eq!(ring.since(4).unwrap(), b"efgh");
    }

    #[test]
    fn ring_snapshot_starts_on_a_character_boundary() {
        let mut ring = Ring::new(3);
        ring.push("a\u{e9}\u{e9}".as_bytes());
        assert_eq!(ring.all(), "\u{e9}".as_bytes());
        let mut whole = Ring::new(8);
        whole.push("\u{e9}".as_bytes());
        assert_eq!(whole.all(), "\u{e9}".as_bytes());
    }

    #[test]
    fn frames_round_trip_and_do_not_clash_with_file_frames() {
        let bytes = encode_frame(OP_OUTPUT, 7, b"hi");
        assert_eq!(bytes, [0x01, 7, b'h', b'i']);
        assert_eq!(decode_frame(&bytes), Some((OP_OUTPUT, 7, &b"hi"[..])));
        assert!(decode_frame(&[0x01]).is_none());
        assert!(is_frame(&bytes) && is_frame(&[OP_RESTORE]));
        assert!(!is_frame(b"{\"type\":\"x\"}") && !is_frame(&[0x10, 0]) && !is_frame(&[]));
    }
}
