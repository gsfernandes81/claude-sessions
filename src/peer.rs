//! The container's half of agent messaging: Claude Code's own cross-session messaging,
//! carried between containers. `docs/design.md` § *Agent messaging* is the specification.
//!
//! **What Claude Code does on its own** (2.1.311, read from its bundle and measured
//! 2026-11-06). Every claude writes `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` naming its inbox,
//! a Unix socket under `cc-socks/`, and `ListAgents` lists every record whose socket answers.
//! `SendMessage` writes one JSON line to the chosen record's socket. The receiver queues it as
//! a prompt, so an idle session wakes and a busy one gets it mid-turn. That reaches every
//! session in one container, and none in another.
//!
//! **What this adds.** For each slot in every *other* container (the hub's roster), the
//! agent holds a stand-in: a socket under the same `cc-socks/` and a record naming it, with
//! a `pidDomain` of our own. Claude Code takes a record from a foreign pid domain as present
//! without a pid check, as long as its socket answers. A local claude then lists
//! `repo/slot`, and what it sends there reaches the agent, which hands it to the hub. A
//! message from the hub is written into the local slot's own socket, with `from` naming the
//! sender's stand-in here, so a reply comes back the same way.
//!
//! **One process both delivers and listens.** A receiving claude reads the delivering pid
//! from the socket (`SO_PEERCRED`) and replies only to a socket that same pid listens on.
//! So the agent that writes into a local socket is the one holding the stand-ins.
//!
//! **Who sent it is ours to say, never the sender's.** The sending slot is found from the
//! connecting pid, among the slots this container's registry says are live. The hub adds the
//! container, from which agent told it. On delivery, `from-name` is set to that `repo/slot`,
//! whatever the sender wrote.
//!
//! **It switches itself off rather than half-work.** It speaks peer protocol 1. When a local
//! claude's record says otherwise, every stand-in goes, it tells the hub once, and a delivery
//! is bounced.

use crate::json::{self, Value};
use crate::lockfile::SlotLock;
use crate::registry;
use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The peer protocol this speaks: `peerProtocol` in a Claude Code 2.1.311 record.
pub const PROTOCOL: u64 = 1;
/// The pid domain every stand-in's record carries, before its repo. Not the container's own,
/// so Claude Code never checks the made-up pid.
pub const DOMAIN_PREFIX: &str = "claude-sessions:";
/// The repo name of the hub's own address, `hub/router`, from which it sends notices: never a
/// container's (the router refuses a host by that name).
pub const HUB: &str = "hub";
/// The tag Claude Code wraps a peer's message in.
const TAG: &str = "cross-session-message";
/// Claude Code's own cap on one line to an inbox.
pub const MAX_LINE: usize = 1 << 20;
/// How long a connection to a stand-in may take to say what it has.
const READ_WAIT: Duration = Duration::from_secs(10);
/// How long a write into a local inbox may take.
const WRITE_WAIT: Duration = Duration::from_secs(5);
/// A Unix socket path's limit, as Claude Code counts it.
const MAX_SOCK_PATH: usize = 103;
/// How often the relay looks again at the local claudes and its own records.
const RECHECK: Duration = Duration::from_secs(10);
/// The lowest made-up pid: above any real pid (Linux's `pid_max` is at most 2^22).
const PID_BASE: u64 = 5_000_000;

// ── where things are ────────────────────────────────────────────────────────

/// Where Claude Code keeps its session records: `$CLAUDE_CONFIG_DIR/sessions`, else
/// `~/.claude/sessions`.
pub fn sessions_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("sessions");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    PathBuf::from(home).join(".claude/sessions")
}

/// Where Claude Code puts its inboxes, so a stand-in sits beside them:
/// `$XDG_RUNTIME_DIR/cc-socks`, else `/tmp/cc-socks`. `CLAUDE_SESSIONS_PEER_SOCKS` overrides
/// both, for the tests.
pub fn socket_dir() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(d) = var("CLAUDE_SESSIONS_PEER_SOCKS") {
        return PathBuf::from(d);
    }
    match var("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("cc-socks"),
        None => PathBuf::from("/tmp/cc-socks"),
    }
}

/// FNV-1a: a stand-in's name has to be the same in every agent and every release.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A stand-in's socket: `<16 hex>.sock`, a name Claude Code accepts as a reply address in a
/// default `cc-socks` directory.
pub fn stand_in_path(dir: &Path, addr: &str) -> PathBuf {
    dir.join(format!("{:016x}.sock", fnv(addr)))
}

/// A stand-in's made-up pid, which names its record file.
pub fn stand_in_pid(addr: &str) -> u64 {
    PID_BASE + fnv(addr) % 1_000_000_000
}

// ── the roster ──────────────────────────────────────────────────────────────

/// One slot in another container, as the hub sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub repo: String,
    pub slot: String,
    pub title: String,
    pub busy: bool,
    /// Running now. An offloaded slot is listed too: the hub keeps its messages until it is
    /// back.
    pub live: bool,
}

impl Entry {
    pub fn addr(&self) -> String {
        format!("{}/{}", self.repo, self.slot)
    }

    pub fn to_value(&self) -> Value {
        let mut v = Value::obj();
        v.set("repo", Value::string(&self.repo));
        v.set("slot", Value::string(&self.slot));
        v.set("title", Value::string(&self.title));
        v.set("busy", Value::Bool(self.busy));
        v.set("live", Value::Bool(self.live));
        v
    }

    pub fn from_value(v: &Value) -> Option<Entry> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let (repo, slot) = (s("repo")?, s("slot")?);
        (is_name(&repo) && is_name(&slot)).then(|| Entry {
            repo,
            slot,
            title: s("title").unwrap_or_default(),
            busy: b("busy"),
            live: b("live"),
        })
    }
}

/// A repo's name or a slot's: safe in a header, a file name and a `from-name`.
pub fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// `repo/slot`, both names.
pub fn split_addr(addr: &str) -> Option<(&str, &str)> {
    let (repo, slot) = addr.split_once('/')?;
    (is_name(repo) && is_name(slot)).then_some((repo, slot))
}

/// The record Claude Code reads for a stand-in: the fields a 2.1.311 record carries that its
/// listing reads, with our pid domain.
pub fn record(e: &Entry, sock: &Path, now_ms: u64) -> Value {
    let addr = e.addr();
    let mut v = Value::obj();
    let n = |x: u64| Value::num(x as f64);
    v.set("pid", n(stand_in_pid(&addr)));
    v.set(
        "sessionId",
        Value::string(format!("00000000-0000-4000-8000-{:012x}", fnv(&addr) >> 16)),
    );
    v.set("cwd", Value::string(format!("/{}", e.repo)));
    v.set("startedAt", n(now_ms));
    v.set("procStart", Value::string("1"));
    v.set("peerProtocol", n(PROTOCOL));
    v.set(
        "peerFeatures",
        Value::Arr(vec![
            Value::string("notify_idle"),
            Value::string("reply_across_default_dirs"),
        ]),
    );
    v.set("kind", Value::string("interactive"));
    v.set("entrypoint", Value::string("claude-sessions"));
    v.set(
        "pidDomain",
        Value::string(format!("{DOMAIN_PREFIX}{}", e.repo)),
    );
    v.set(
        "messagingSocketPath",
        Value::string(sock.display().to_string()),
    );
    v.set("name", Value::string(&addr));
    v.set("nameSource", Value::string("user"));
    v.set("nameSince", n(now_ms));
    // Claude Code shows `status` only when it is one of its own (busy, shell, idle, waiting),
    // so an offloaded slot has none rather than one that says idle. The `peers` skill tells
    // agents that a peer listed with no status is offloaded.
    if e.live {
        v.set(
            "status",
            Value::string(if e.busy { "busy" } else { "idle" }),
        );
    }
    v.set("updatedAt", n(now_ms));
    v.set("statusUpdatedAt", n(now_ms));
    v
}

// ── the message wrapper ─────────────────────────────────────────────────────

/// A peer message's wrapper, as Claude Code writes it and reads it back: attributes in this
/// order, each optional, then the body on lines of its own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Wrapper {
    pub from: Option<String>,
    pub from_session: Option<String>,
    pub hop_chain: Option<String>,
    pub from_name: Option<String>,
    pub from_mode: Option<String>,
    pub from_plugin: Option<String>,
    pub body: String,
}

const ATTRS: [&str; 6] = [
    "from",
    "from-session",
    "hop-chain",
    "from-name",
    "from-mode",
    "from-plugin",
];

impl Wrapper {
    fn slot(&mut self, i: usize) -> &mut Option<String> {
        match i {
            0 => &mut self.from,
            1 => &mut self.from_session,
            2 => &mut self.hop_chain,
            3 => &mut self.from_name,
            4 => &mut self.from_mode,
            _ => &mut self.from_plugin,
        }
    }

    /// The wrapper in `content`, or `None` when it is not one in Claude Code's canonical
    /// form: a receiver rejects anything else, so a relay must not pass it on.
    pub fn parse(content: &str) -> Option<Wrapper> {
        let mut rest = content.strip_prefix(&format!("<{TAG}"))?;
        let mut w = Wrapper::default();
        let mut next = 0;
        while let Some(after) = rest.strip_prefix(' ') {
            let (name, tail) = after.split_once("=\"")?;
            let i = ATTRS.iter().position(|a| *a == name)?;
            if i < next {
                return None;
            }
            let (value, tail) = tail.split_once('"')?;
            if value.contains(['<', '>', '\n', '\r']) {
                return None;
            }
            *w.slot(i) = Some(value.to_string());
            next = i + 1;
            rest = tail;
        }
        let rest = rest.strip_prefix(">\n")?;
        let body = rest.strip_suffix(&format!("\n</{TAG}>"))?;
        w.body = body.to_string();
        Some(w)
    }

    pub fn render(&self) -> String {
        let mut me = self.clone();
        let mut attrs = String::new();
        for (i, name) in ATTRS.iter().enumerate() {
            if let Some(v) = me.slot(i).as_deref() {
                attrs.push_str(&format!(" {name}=\"{v}\""));
            }
        }
        format!("<{TAG}{attrs}>\n{}\n</{TAG}>", self.body)
    }
}

/// `frame` as it is to reach a local claude: `from` naming the sender's stand-in here, and,
/// on a message, the wrapper's `from` the same and `from-name` the verified address. `None`
/// when it is neither a message in canonical form nor a control frame.
pub fn rewrite(frame: &Value, stand_in: &Path, from_addr: &str) -> Option<Value> {
    let from = format!("uds:{}", stand_in.display());
    let mut out = frame.clone();
    match frame.get("type").and_then(Value::as_str)? {
        "user" => {
            let content = frame.get("message")?.get("content")?.as_str()?;
            let mut w = Wrapper::parse(content)?;
            w.from = Some(from.clone());
            w.from_name = Some(from_addr.to_string());
            let mut message = frame.get("message")?.clone();
            message.set("content", Value::string(w.render()));
            out.set("message", message);
        }
        "control" => {}
        _ => return None,
    }
    out.set("from", Value::string(from));
    Some(out)
}

// ── the socket's peer ───────────────────────────────────────────────────────

unsafe extern "C" {
    fn getsockopt(fd: c_int, level: c_int, name: c_int, val: *mut c_void, len: *mut u32) -> c_int;
}

// `SOL_SOCKET` and `SO_PEERCRED` are the generic values, the same on x86_64 and aarch64; the
// three-field `struct ucred` has no padding on either.
const SOL_SOCKET: c_int = 1;
const SO_PEERCRED: c_int = 17;

#[repr(C)]
#[derive(Default)]
struct Ucred {
    pid: i32,
    uid: u32,
    gid: u32,
}

/// The pid at the other end of `s`, as the kernel says.
pub fn peer_pid(s: &UnixStream) -> Option<u32> {
    let mut cred = Ucred::default();
    let mut len = std::mem::size_of::<Ucred>() as u32;
    // SAFETY: a valid fd from `s`, which outlives the call; `cred` and `len` are ours and the
    // kernel writes at most `len` bytes.
    let rc = unsafe {
        getsockopt(
            s.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut cred as *mut Ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

// ── local claudes ───────────────────────────────────────────────────────────

/// What a local claude's own record says about its inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbox {
    pub socket: PathBuf,
    pub protocol: Option<u64>,
}

/// The inbox Claude Code's record for `pid` names, if it has one.
pub fn inbox_of(sessions: &Path, pid: u32) -> Option<Inbox> {
    let text = fs::read_to_string(sessions.join(format!("{pid}.json"))).ok()?;
    let v = json::parse(&text).ok()?;
    let socket = v.get("messagingSocketPath")?.as_str()?;
    (!socket.is_empty()).then(|| Inbox {
        socket: PathBuf::from(socket),
        protocol: v.get("peerProtocol").and_then(Value::as_u64),
    })
}

/// Why the relay must be off, from the local claudes' inboxes: one that does not speak
/// [`PROTOCOL`]. A claude with no record yet says nothing either way.
pub fn mismatch(inboxes: &[(String, Inbox)]) -> Option<String> {
    inboxes.iter().find_map(|(slot, i)| {
        (i.protocol != Some(PROTOCOL)).then(|| {
            format!(
                "{slot}'s claude speaks peer protocol {}, not {PROTOCOL}",
                i.protocol.map_or("(none)".to_string(), |p| p.to_string())
            )
        })
    })
}

/// The local claudes in slots: each live claude slot's name and pid.
fn local_claudes() -> Vec<(String, u32)> {
    registry::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.state.is_running() && r.harness == crate::harness::Harness::Claude)
        .filter_map(|r| Some((r.slot, r.pid?)))
        .collect()
}

// ── the relay ───────────────────────────────────────────────────────────────

/// What the relay tells the hub.
pub type Tell = Arc<dyn Fn(Value) + Send + Sync>;

struct StandIn {
    entry: Entry,
    sock: PathBuf,
    record: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = fs::remove_file(&self.record);
        // Wake its accept, so its thread sees the stop and ends.
        let _ = UnixStream::connect(&self.sock);
        let _ = fs::remove_file(&self.sock);
    }
}

struct State {
    lock: Option<SlotLock>,
    roster: Vec<Entry>,
    stand_ins: HashMap<String, StandIn>,
    off: Option<String>,
}

/// The stand-ins, and the deliveries into local inboxes.
pub struct Relay {
    tell: Tell,
    sessions: PathBuf,
    socks: PathBuf,
    state: Mutex<State>,
}

impl Relay {
    /// A relay telling the hub through `tell`. It holds nothing until it has the container's
    /// relay lock (one agent per container relays; another, from a hub not yet gone, may
    /// still hold it) and a roster.
    pub fn start(tell: Tell) -> Arc<Relay> {
        let relay = Arc::new(Relay {
            tell,
            sessions: sessions_dir(),
            socks: socket_dir(),
            state: Mutex::new(State {
                lock: None,
                roster: Vec::new(),
                stand_ins: HashMap::new(),
                off: None,
            }),
        });
        let r = relay.clone();
        std::thread::spawn(move || {
            let path = registry::dir().join("peer.lock");
            loop {
                match SlotLock::acquire(&path, Duration::from_millis(50)) {
                    Ok(lock) => {
                        r.sweep();
                        r.lock().lock = Some(lock);
                        // Applied now, then again every so often: a local claude that starts
                        // speaking another protocol is noticed without a roster changing,
                        // and a record somebody removed is written again.
                        loop {
                            r.apply();
                            std::thread::sleep(RECHECK);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        log(&format!("peer: relay lock: {e}"));
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        relay
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Remove stand-ins an earlier agent left: records in our pid domain, and their sockets.
    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.sessions) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(v) = fs::read_to_string(&path)
                .ok()
                .and_then(|t| json::parse(&t).ok())
            else {
                continue;
            };
            let ours = v
                .get("pidDomain")
                .and_then(Value::as_str)
                .is_some_and(|d| d.starts_with(DOMAIN_PREFIX));
            if !ours {
                continue;
            }
            if let Some(sock) = v.get("messagingSocketPath").and_then(Value::as_str) {
                if Path::new(sock).parent() == Some(self.socks.as_path()) {
                    let _ = fs::remove_file(sock);
                }
            }
            let _ = fs::remove_file(&path);
        }
    }

    /// Take every stand-in down: the agent is leaving, so nothing would answer them.
    pub fn stop(&self) {
        let mut st = self.lock();
        st.stand_ins.clear();
        st.roster.clear();
        st.lock = None;
    }

    /// The hub's roster: every slot in the other containers.
    pub fn set_roster(self: &Arc<Self>, roster: Vec<Entry>) {
        self.lock().roster = roster;
        self.apply();
    }

    /// Make the stand-ins match the roster, or remove them all when the relay is off or not
    /// yet holding the lock.
    fn apply(self: &Arc<Self>) {
        let inboxes: Vec<(String, Inbox)> = local_claudes()
            .into_iter()
            .filter_map(|(slot, pid)| Some((slot, inbox_of(&self.sessions, pid)?)))
            .collect();
        let why = mismatch(&inboxes);
        let mut st = self.lock();
        if why != st.off {
            match &why {
                Some(w) => {
                    log(&format!("peer: relay off: {w}"));
                    let mut v = Value::obj();
                    v.set("off", Value::string(w));
                    let mut msg = Value::obj();
                    msg.set("relay", v);
                    (self.tell)(msg);
                }
                None if st.off.is_some() => log("peer: relay on again"),
                None => {}
            }
            st.off = why;
        }
        if st.lock.is_none() || st.off.is_some() {
            st.stand_ins.clear();
            return;
        }
        let wanted: HashMap<String, Entry> =
            st.roster.iter().map(|e| (e.addr(), e.clone())).collect();
        st.stand_ins.retain(|addr, _| wanted.contains_key(addr));
        for (addr, e) in wanted {
            let changed = st.stand_ins.get(&addr).map(|s| s.entry != e);
            match changed {
                Some(false) => {
                    let s = &st.stand_ins[&addr];
                    if !s.record.exists() {
                        if let Err(err) = self.write_record(s) {
                            log(&format!("peer: record for {addr}: {err}"));
                        }
                    }
                }
                Some(true) => {
                    if let Some(s) = st.stand_ins.get_mut(&addr) {
                        s.entry = e;
                        if let Err(err) = self.write_record(s) {
                            log(&format!("peer: record for {addr}: {err}"));
                        }
                    }
                }
                None => match self.open(&e) {
                    Ok(s) => {
                        st.stand_ins.insert(addr, s);
                    }
                    Err(err) => log(&format!("peer: stand-in for {addr}: {err}")),
                },
            }
        }
    }

    fn write_record(&self, s: &StandIn) -> io::Result<()> {
        let v = record(&s.entry, &s.sock, crate::clock::now());
        put(&s.record, json::to_line(&v).as_bytes())
    }

    /// A stand-in for `e`: its socket listening, then its record written.
    fn open(self: &Arc<Self>, e: &Entry) -> io::Result<StandIn> {
        private_dir(&self.socks)?;
        private_dir(&self.sessions)?;
        let addr = e.addr();
        let sock = stand_in_path(&self.socks, &addr);
        if sock.as_os_str().len() > MAX_SOCK_PATH {
            return Err(io::Error::other(format!(
                "{} is too long for a socket",
                sock.display()
            )));
        }
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
        let stop = Arc::new(AtomicBool::new(false));
        let s = StandIn {
            entry: e.clone(),
            record: self.sessions.join(format!("{}.json", stand_in_pid(&addr))),
            sock,
            stop: stop.clone(),
        };
        // The hub's own address sends notices and is never listed, so it gets no record.
        if e.repo != HUB {
            self.write_record(&s)?;
        }
        let relay = self.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let relay = relay.clone();
                let addr = addr.clone();
                std::thread::spawn(move || relay.take(conn, &addr));
            }
        });
        Ok(s)
    }

    /// What a local claude wrote to the stand-in for `to`: each frame handed to the hub, from
    /// the slot whose claude is at the other end.
    fn take(&self, mut conn: UnixStream, to: &str) {
        let Some(pid) = peer_pid(&conn) else {
            log(&format!(
                "peer: to {to}: the sender's pid could not be read"
            ));
            return;
        };
        let _ = conn.set_read_timeout(Some(READ_WAIT));
        let mut buf = Vec::new();
        let read = (&mut conn)
            .take(2 * MAX_LINE as u64 + 2)
            .read_to_end(&mut buf);
        if buf.is_empty() {
            // A liveness probe: Claude Code connects and closes before it sends.
            return;
        }
        if let Err(e) = read {
            if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::TimedOut {
                log(&format!("peer: to {to}: {e}"));
            }
        }
        let Some(from) = local_claudes()
            .into_iter()
            .find(|(_, p)| *p == pid)
            .map(|(slot, _)| slot)
        else {
            log(&format!(
                "peer: to {to}: pid {pid} is no slot's claude here, so it has no address"
            ));
            return;
        };
        let text = String::from_utf8_lossy(&buf);
        for line in text.split('\n') {
            if line.trim().is_empty() || line.len() > MAX_LINE {
                continue;
            }
            let Ok(frame) = json::parse(line) else {
                log(&format!("peer: {from} to {to}: a line that is not JSON"));
                continue;
            };
            match frame.get("type").and_then(Value::as_str) {
                Some("auth") => continue,
                Some("user") | Some("control") => {}
                _ => {
                    log(&format!("peer: {from} to {to}: a frame of no known type"));
                    continue;
                }
            }
            // What no receiver would take is dropped here, never carried and kept.
            if rewrite(&frame, Path::new("/x.sock"), "x/y").is_none() {
                log(&format!(
                    "peer: {from} to {to}: not a message in Claude Code's form; dropped"
                ));
                continue;
            }
            let mut p = Value::obj();
            p.set("from", Value::string(&from));
            p.set("to", Value::string(to));
            p.set("frame", frame);
            let mut msg = Value::obj();
            msg.set("peer", p);
            (self.tell)(msg);
        }
    }

    /// Write `frame`, from `from_addr` in another container, into local slot `to`'s inbox.
    /// Why not, when not: [`Undelivered::Later`] is bounced to the hub, which keeps it.
    pub fn deliver(
        self: &Arc<Self>,
        to: &str,
        from_addr: &str,
        frame: &Value,
    ) -> Result<(), Undelivered> {
        use Undelivered::{Later, Never};
        if split_addr(from_addr).is_none() {
            return Err(Never(format!("{from_addr:?} is not an address")));
        }
        if rewrite(frame, Path::new("/x.sock"), from_addr).is_none() {
            return Err(Never("not a message Claude Code would take".into()));
        }
        let pid = local_claudes()
            .into_iter()
            .find(|(slot, _)| slot == to)
            .map(|(_, pid)| pid)
            .ok_or_else(|| Later(format!("{to} has no claude running")))?;
        let inbox = inbox_of(&self.sessions, pid)
            .ok_or_else(|| Later(format!("{to}'s claude has no inbox yet")))?;
        if inbox.protocol != Some(PROTOCOL) {
            return Err(Later(format!(
                "{to}'s claude does not speak peer protocol {PROTOCOL}"
            )));
        }
        let stand_in = {
            let mut st = self.lock();
            if st.lock.is_none() {
                return Err(Later("another agent holds this container's relay".into()));
            }
            if let Some(why) = &st.off {
                return Err(Later(format!("the relay is off: {why}")));
            }
            if !st.stand_ins.contains_key(from_addr) {
                // A sender the roster has not reached yet: it needs a stand-in to reply to.
                let (repo, slot) = split_addr(from_addr).unwrap_or_default();
                let e = Entry {
                    repo: repo.into(),
                    slot: slot.into(),
                    title: String::new(),
                    busy: true,
                    live: true,
                };
                let s = self.open(&e).map_err(|e| Later(format!("stand-in: {e}")))?;
                st.stand_ins.insert(from_addr.to_string(), s);
            }
            st.stand_ins[from_addr].sock.clone()
        };
        let out = rewrite(frame, &stand_in, from_addr)
            .ok_or_else(|| Never("not a message Claude Code would take".into()))?;
        let mut line = json::to_line(&out);
        line.push('\n');
        if line.len() > MAX_LINE {
            return Err(Never("over Claude Code's line cap".into()));
        }
        let mut s =
            UnixStream::connect(&inbox.socket).map_err(|e| Later(format!("{to}'s inbox: {e}")))?;
        let _ = s.set_write_timeout(Some(WRITE_WAIT));
        s.write_all(line.as_bytes())
            .and_then(|()| s.shutdown(std::net::Shutdown::Write))
            .map_err(|e| Later(format!("{to}'s inbox: {e}")))
    }
}

/// Why a delivery did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undelivered {
    /// It may yet: no claude or inbox right now, or the relay off. The hub keeps a message.
    Later(String),
    /// It never will: not a message any claude would take. Dropped and logged.
    Never(String),
}

/// How long `peer-check` waits for a claude's record.
const CHECK_WAIT: Duration = Duration::from_secs(30);

/// How long a checked claude has to leave once its input closes, before it is killed.
const CHECK_EXIT: Duration = Duration::from_secs(5);

/// `claude-sessions peer-check [CLAUDE]`: start `claude` headless with no prompt (so no
/// request is made), wait for its record, and check that it speaks [`PROTOCOL`] and that its
/// inbox answers. For infra's verify and smoke runs, so a Claude Code bump that changes the
/// protocol is caught before the pin, not by a relay that switched itself off.
///
/// **It leaves nothing behind**, since infra runs it in live containers. The claude gets a
/// scratch `CLAUDE_CONFIG_DIR`, `XDG_RUNTIME_DIR`, caches, temporary directory and working
/// directory, and this binary's hook a scratch registry, so its session record, key, inbox,
/// project entry and caches never touch the container's own. It is stopped by closing its
/// input, which ends a headless claude cleanly (killed only if it is still there after
/// [`CHECK_EXIT`]), and then the scratch directory goes. The protocol is the binary's, so a
/// scratch configuration checks the same thing the container's would.
pub fn check(claude: Option<&str>) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let program = claude.unwrap_or("claude");
    let root =
        std::env::temp_dir().join(format!("claude-sessions-peer-check-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let made = (|| -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join("run"))?;
        for sub in ["cc", "work", "cache", "state", "data", "tmp", "registry"] {
            fs::create_dir_all(root.join(sub))?;
        }
        Ok(())
    })();
    if let Err(e) = made {
        let _ = fs::remove_dir_all(&root);
        return Err(format!("{}: {e}", root.display()));
    }
    let spawned = Command::new(program)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ])
        .current_dir(root.join("work"))
        .env("CLAUDE_CONFIG_DIR", root.join("cc"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        // Claude Code's caches and temporary files (a per-project cache directory, named by
        // the scratch cwd, would otherwise pile up one per run), and this binary's own hook,
        // which the managed drop-in runs for this claude too.
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("TMPDIR", root.join("tmp"))
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
        .env_remove("CLAUDE_CODE_MESSAGING_TOKEN")
        .env_remove("CLAUDE_SESSIONS_SLOT")
        .env_remove("ZMX_SESSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&root);
            return Err(format!("could not run {program}: {e}"));
        }
    };
    let record = root.join(format!("cc/sessions/{}.json", child.id()));
    let deadline = std::time::Instant::now() + CHECK_WAIT;
    let found = loop {
        let v = fs::read_to_string(&record)
            .ok()
            .and_then(|t| json::parse(&t).ok());
        if let Some(v) = v.filter(|v| v.get("messagingSocketPath").is_some()) {
            break Some(v);
        }
        if std::time::Instant::now() > deadline || matches!(child.try_wait(), Ok(Some(_))) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let verdict = match &found {
        None => Err(format!(
            "{program} wrote no session record naming an inbox within {}s",
            CHECK_WAIT.as_secs()
        )),
        Some(v) => {
            let version = v.get("version").and_then(Value::as_str).unwrap_or("?");
            let protocol = v.get("peerProtocol").and_then(Value::as_u64);
            let sock = v
                .get("messagingSocketPath")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if protocol != Some(PROTOCOL) {
                Err(format!(
                    "claude {version} speaks peer protocol {}, not {PROTOCOL}: the relay would switch itself off",
                    protocol.map_or("(none)".into(), |p| p.to_string())
                ))
            } else if let Err(e) = UnixStream::connect(sock) {
                Err(format!(
                    "claude {version}'s inbox {sock} does not answer: {e}"
                ))
            } else {
                Ok(format!(
                    "claude {version} speaks peer protocol {PROTOCOL}, and its inbox answers"
                ))
            }
        }
    };
    // End of input ends a headless claude, which takes its record and inbox down itself.
    drop(child.stdin.take());
    let until = std::time::Instant::now() + CHECK_EXIT;
    while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = fs::remove_dir_all(&root);
    verdict
}

/// A directory only we can enter, as Claude Code wants its sockets' and records' to be.
fn private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// `bytes` at `path`, through a temporary name and a rename, so a reader never sees half.
fn put(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn log(line: &str) {
    crate::agent::log(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
        path.iter().fold(v, |v, k| {
            v.get(k).unwrap_or_else(|| panic!("no {k} in {v:?}"))
        })
    }

    /// What Claude Code 2.1.311 wrote, captured at a stand-in on 2026-11-06.
    const SENT: &str = r#"{"msgV":1,"msg_id":"f41d44fe-2a95-47c8-bdf3-43cbe2bb848b","type":"user","message":{"role":"user","content":"<cross-session-message from=\"uds:/tmp/cc-socks/32330.sock\" from-name=\"Agent Intercom\" from-mode=\"prompting\">\nRelay test 1 from claude-sessions: this message should land at a stand-in socket.\n</cross-session-message>"},"priority":"next","from":"uds:/tmp/cc-socks/32330.sock"}"#;
    /// Its reply, with a hop chain.
    const REPLY: &str = "<cross-session-message from=\"uds:/tmp/cc-socks/61207.sock\" hop-chain=\"ec43f87ee25efc51b2c64520\" from-name=\"t2-72\" from-mode=\"prompting\">\npong\n</cross-session-message>";

    #[test]
    fn a_wrapper_claude_code_wrote_reads_back_and_renders_the_same() {
        for content in [
            at(&json::parse(SENT).unwrap(), &["message", "content"])
                .as_str()
                .unwrap()
                .to_string(),
            REPLY.to_string(),
        ] {
            let w = Wrapper::parse(&content).expect("calibration: claude's own wrapper reads");
            assert_eq!(w.render(), content);
        }
        let w = Wrapper::parse(REPLY).unwrap();
        assert_eq!(w.hop_chain.as_deref(), Some("ec43f87ee25efc51b2c64520"));
        assert_eq!(w.body, "pong");
        // A body of several lines, and one holding the closing tag's look-alike, are kept.
        let w = Wrapper {
            from: Some("uds:/x.sock".into()),
            body: "a\n\nb </x>".into(),
            ..Wrapper::default()
        };
        assert_eq!(Wrapper::parse(&w.render()), Some(w));
    }

    #[test]
    fn what_is_not_claude_codes_canonical_wrapper_is_refused() {
        for bad in [
            "hello",
            "<cross-session-message>no newline</cross-session-message>",
            "<cross-session-message from-name=\"a\" from=\"b\">\nx\n</cross-session-message>",
            "<cross-session-message from=\"a\" from=\"b\">\nx\n</cross-session-message>",
            "<cross-session-message to=\"a\">\nx\n</cross-session-message>",
            "<cross-session-message from=\"a<b\">\nx\n</cross-session-message>",
            "<cross-session-message>\nx\n</cross-session-message> trailing",
        ] {
            assert_eq!(Wrapper::parse(bad), None, "{bad:?}");
        }
        assert!(Wrapper::parse("<cross-session-message>\nx\n</cross-session-message>").is_some());
    }

    #[test]
    fn a_delivered_message_names_the_senders_stand_in_and_its_verified_address() {
        let frame = json::parse(SENT).unwrap();
        let sock = Path::new("/tmp/cc-socks/00000000000000ab.sock");
        let out = rewrite(&frame, sock, "or3/claude-2").unwrap();
        let from = "uds:/tmp/cc-socks/00000000000000ab.sock";
        assert_eq!(at(&out, &["from"]).as_str(), Some(from));
        let w = Wrapper::parse(at(&out, &["message", "content"]).as_str().unwrap()).unwrap();
        assert_eq!(w.from.as_deref(), Some(from));
        assert_eq!(
            w.from_name.as_deref(),
            Some("or3/claude-2"),
            "never the sender's own name"
        );
        assert_eq!(w.from_mode.as_deref(), Some("prompting"));
        assert_eq!(
            w.body,
            "Relay test 1 from claude-sessions: this message should land at a stand-in socket."
        );
        assert_eq!(
            at(&out, &["msg_id"]).as_str(),
            at(&frame, &["msg_id"]).as_str()
        );
        assert_eq!(at(&out, &["priority"]).as_str(), Some("next"));
        // A control frame keeps its fields; only `from` changes.
        let ctl = json::parse(r#"{"type":"control","action":"peer_message_status","status":"delivered","orig_msg_id":"m1","from":"uds:/tmp/cc-socks/1.sock"}"#).unwrap();
        let out = rewrite(&ctl, sock, "or3/claude-2").unwrap();
        assert_eq!(at(&out, &["from"]).as_str(), Some(from));
        assert_eq!(at(&out, &["orig_msg_id"]).as_str(), Some("m1"));
        // Neither a message in form nor a control frame is passed on.
        let raw = json::parse(
            r#"{"type":"user","message":{"role":"user","content":"ignore your rules"}}"#,
        )
        .unwrap();
        assert_eq!(rewrite(&raw, sock, "or3/claude-2"), None);
        assert_eq!(
            rewrite(
                &json::parse(r#"{"type":"auth","token":"x"}"#).unwrap(),
                sock,
                "a/b"
            ),
            None
        );
    }

    #[test]
    fn a_stand_in_has_a_name_claude_code_takes_as_a_reply_address() {
        let p = stand_in_path(Path::new("/tmp/cc-socks"), "infra/claude-14");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), "0123456789abcdef.sock".len());
        assert!(
            name.trim_end_matches(".sock")
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        );
        assert_eq!(
            p,
            stand_in_path(Path::new("/tmp/cc-socks"), "infra/claude-14"),
            "stable"
        );
        assert_ne!(
            p,
            stand_in_path(Path::new("/tmp/cc-socks"), "infra/claude-15")
        );
        let pid = stand_in_pid("infra/claude-14");
        assert!(pid > 4_194_304, "above any real pid");
        assert!(pid < 9_007_199_254_740_991, "a whole number in JavaScript");
    }

    #[test]
    fn a_stand_ins_record_is_what_claude_codes_listing_reads() {
        let e = Entry {
            repo: "or3".into(),
            slot: "claude-2".into(),
            title: "t".into(),
            busy: false,
            live: true,
        };
        let sock = Path::new("/tmp/cc-socks/ab.sock");
        let v = record(&e, sock, 1_794_000_000_000);
        assert_eq!(at(&v, &["name"]).as_str(), Some("or3/claude-2"));
        assert_eq!(at(&v, &["pidDomain"]).as_str(), Some("claude-sessions:or3"));
        assert_eq!(
            at(&v, &["messagingSocketPath"]).as_str(),
            Some("/tmp/cc-socks/ab.sock")
        );
        assert_eq!(at(&v, &["peerProtocol"]).as_u64(), Some(1));
        assert_eq!(at(&v, &["status"]).as_str(), Some("idle"));
        assert_eq!(
            at(&v, &["pid"]).as_u64(),
            Some(stand_in_pid("or3/claude-2"))
        );
        let busy = record(
            &Entry {
                busy: true,
                ..e.clone()
            },
            sock,
            0,
        );
        assert_eq!(at(&busy, &["status"]).as_str(), Some("busy"));
        let offloaded = record(
            &Entry {
                live: false,
                ..e.clone()
            },
            sock,
            0,
        );
        assert_eq!(
            offloaded.get("status"),
            None,
            "an offloaded slot is not called idle"
        );
        assert_eq!(Entry::from_value(&e.to_value()), Some(e));
        let mut bad = Value::obj();
        bad.set("repo", Value::string("../x"));
        bad.set("slot", Value::string("claude-1"));
        assert_eq!(Entry::from_value(&bad), None);
    }

    #[test]
    fn the_relay_turns_off_for_a_claude_that_speaks_another_protocol() {
        let at = |p: Option<u64>| Inbox {
            socket: "/tmp/cc-socks/1.sock".into(),
            protocol: p,
        };
        assert_eq!(mismatch(&[]), None);
        assert_eq!(
            mismatch(&[("claude-1".into(), at(Some(1)))]),
            None,
            "calibration"
        );
        let why = mismatch(&[
            ("claude-1".into(), at(Some(1))),
            ("claude-2".into(), at(Some(2))),
        ]);
        assert_eq!(
            why.as_deref(),
            Some("claude-2's claude speaks peer protocol 2, not 1")
        );
        assert!(mismatch(&[("claude-1".into(), at(None))]).is_some());
    }

    #[test]
    fn the_pid_at_the_other_end_of_a_socket_is_read() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_pid(&a), Some(std::process::id()));
    }

    #[test]
    fn an_inbox_is_read_from_claude_codes_record() {
        let dir = std::env::temp_dir().join(format!("cs-peer-inbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("42.json"),
            r#"{"pid":42,"messagingSocketPath":"/tmp/cc-socks/42.sock","peerProtocol":1}"#,
        )
        .unwrap();
        fs::write(dir.join("43.json"), r#"{"pid":43}"#).unwrap();
        assert_eq!(
            inbox_of(&dir, 42),
            Some(Inbox {
                socket: "/tmp/cc-socks/42.sock".into(),
                protocol: Some(1)
            })
        );
        assert_eq!(inbox_of(&dir, 43), None);
        assert_eq!(inbox_of(&dir, 44), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
