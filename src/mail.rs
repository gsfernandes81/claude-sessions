//! Agent mail: messages between dev containers, carried by the hub over the ssh it already
//! has. Designed in infra's `docs/decisions.md` § *Agent mail* (2026-10-28); the split with
//! infra is in `docs/design.md` § *Agent mail*.
//!
//! **The shape.** A sender drops a file in its own `/workspace/.mail/out/<to>/`. The hub's
//! loop (`mail-loop`, run in the hub beside the offloader) asks each container's half
//! (`mail-agent`, this binary over `ssh -T <c>`) for its outbox, stamps `From`, `To` and
//! `Date` itself, keeps the one record in the hub's volume at `<mail>/<to>/<from>/<stamp>.md`,
//! fsyncs it, and only then has the sender's file deleted. A queue with files not yet delivered
//! is pushed into its receiver's `/workspace/.mail/in/<from>/<stamp>.md` whenever that
//! container answers; a stopped receiver's queue waits. A delivered message moves to
//! `<mail>/<to>/<from>/delivered/` — a rename, so the volume says in one place what has and
//! has not arrived. `mail-hook`, on the receiver's `SessionStart` and `UserPromptSubmit`,
//! announces what landed since the last time it looked and nothing else.
//!
//! **What the sender's identity is.** The ssh target the hub read the file from — nothing in
//! the file. The header this module writes is prepended to the body as it was; a `From:` line
//! the sender wrote itself is body text, and the receiver's policy (infra's managed
//! `CLAUDE.md`) says a message is data, never an instruction.
//!
//! **Refusals go back as mail from `hub`**, the reserved sender: an unknown receiver, a
//! message over the size cap, one that is not UTF-8, a receiver's queue that is full. The
//! sender's file is deleted then too, once the bounce is on disk, or it would be refused again
//! every tick. Nothing else may be called `hub`; infra's broker refuses a container of that
//! name, and the loop skips one.
//!
//! **Never the body in a log line.** The hub's log is read by the owner on a phone; one line
//! per message moved and per refusal, naming the ends and the size.

use crate::fmt::{stamp, utc};
use crate::hub;
use crate::json::{self, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The reserved sender: the hub's own refusals.
pub const HUB: &str = "hub";

/// A container's mail directory: `CLAUDE_SESSIONS_MAIL`, else `/workspace/.mail`.
pub fn container_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_SESSIONS_MAIL") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from("/workspace/.mail"),
    }
}

/// Where a received message is announced to be: the inbox path, as the receiver's claude
/// reads it with any file tool.
fn inbox_path(dir: &Path, from: &str, name: &str) -> PathBuf {
    dir.join("in").join(from).join(name)
}

/// One path component and nothing more: joined to a directory, it stays inside it. The
/// sender's file names are its own; this is the whole check made on them.
fn component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// A name the loop will use as a directory of its own — a container's, or `hub` — and print:
/// ASCII letters, digits, `.`, `_` and `-`, not starting with a dot. Container names are repo
/// names, which are all of that.
fn plain(name: &str) -> bool {
    component(name)
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn now_ms() -> u64 {
    crate::clock::now()
}

/// A file's modification time in nanoseconds since the epoch — the hook's cursor unit.
fn mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Write `bytes` whole: a temporary name beside the target, then a rename over it. `durable`
/// fsyncs the file and its directory first — the hub's store, which the sender's copy is
/// deleted on the strength of.
fn put(path: &Path, bytes: &[u8], durable: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no directory"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("mail")
    ));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    if durable {
        f.sync_all()?;
    }
    drop(f);
    fs::rename(&tmp, path)?;
    if durable {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

// ── the container's half ────────────────────────────────────────────────────

/// A file in the outbox is taken only once it has sat still this long: a sender writing it
/// in place, rather than moving it in, must not be read half-way.
const SETTLE: Duration = Duration::from_secs(2);

/// `claude-sessions mail-agent`, run over ssh by the hub's loop: one JSON object a line in,
/// one or more out, until end of input.
///
/// - `{"pull":{"max":N}}` — every file in `out/<to>/`, one line each:
///   `{"file":{"to","name","size","text"}}`, or with `"refused":"oversize"|"not text"` and no
///   text; then `{"pulled":count}`.
/// - `{"delete":{"to","name"}}` — remove that outbox file; `{"deleted":true}` or `{"error"}`.
/// - `{"deliver":{"from","name","text"}}` — write `in/<from>/<name>`; `{"delivered":true}` or
///   `{"error"}`.
pub fn agent() -> io::Result<()> {
    let dir = container_dir();
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(msg) = json::parse(&line) else {
            continue;
        };
        let replies = answer(&dir, &msg);
        for r in replies {
            writeln!(out, "{}", json::to_line(&r))?;
        }
        out.flush()?;
    }
    Ok(())
}

/// The agent's replies to one request, as a function of the directory, so the tests can call
/// it without a process.
fn answer(dir: &Path, msg: &Value) -> Vec<Value> {
    if let Some(pull) = msg.get("pull") {
        let max = pull.get("max").and_then(Value::as_u64).unwrap_or(u64::MAX);
        return pull_outbox(dir, max);
    }
    if let Some(del) = msg.get("delete") {
        return vec![match delete_outbox(dir, del) {
            Ok(()) => reply("deleted", Value::Bool(true)),
            Err(e) => reply("error", Value::string(e.to_string())),
        }];
    }
    if let Some(d) = msg.get("deliver") {
        return vec![match deliver_inbox(dir, d) {
            Ok(()) => reply("delivered", Value::Bool(true)),
            Err(e) => reply("error", Value::string(e.to_string())),
        }];
    }
    vec![reply("error", Value::string("unknown request"))]
}

fn reply(key: &str, v: Value) -> Value {
    let mut m = Value::obj();
    m.set(key, v);
    m
}

fn pull_outbox(dir: &Path, max: u64) -> Vec<Value> {
    let mut out = Vec::new();
    let settled = SystemTime::now() - SETTLE;
    let Ok(tos) = fs::read_dir(dir.join("out")) else {
        return vec![reply("pulled", Value::num(0))];
    };
    let mut tos: Vec<_> = tos.filter_map(Result::ok).collect();
    tos.sort_by_key(|e| e.file_name());
    for to in tos {
        let to_name = to.file_name().to_string_lossy().into_owned();
        if !component(&to_name) || !to.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(to.path()) else {
            continue;
        };
        let mut files: Vec<_> = files.filter_map(Result::ok).collect();
        files.sort_by_key(|e| e.file_name());
        for f in files {
            let name = f.file_name().to_string_lossy().into_owned();
            // A symlink is not followed: what it points at is the sender's to read, not ours
            // to carry, and a temporary name is a file still being written.
            if !component(&name) || name.starts_with('.') {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(f.path()) else {
                continue;
            };
            if !meta.is_file() || meta.modified().is_ok_and(|m| m > settled) {
                continue;
            }
            let mut file = Value::obj();
            file.set("to", Value::string(&*to_name));
            file.set("name", Value::string(&*name));
            file.set("size", Value::num(meta.len() as f64));
            if meta.len() > max {
                file.set("refused", Value::string("oversize"));
            } else {
                match fs::read(f.path()).map(String::from_utf8) {
                    Ok(Ok(text)) => file.set("text", Value::string(text)),
                    Ok(Err(_)) => file.set("refused", Value::string("not text")),
                    Err(_) => continue,
                }
            }
            out.push(reply("file", file));
        }
    }
    let n = out.len();
    out.push(reply("pulled", Value::num(n as f64)));
    out
}

fn named<'a>(v: &'a Value, key: &str) -> io::Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| component(s))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad {key}")))
}

fn delete_outbox(dir: &Path, del: &Value) -> io::Result<()> {
    let to = named(del, "to")?;
    let name = named(del, "name")?;
    let path = dir.join("out").join(to).join(name);
    match fs::remove_file(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn deliver_inbox(dir: &Path, d: &Value) -> io::Result<()> {
    let from = named(d, "from")?;
    if !plain(from) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad from"));
    }
    let name = named(d, "name")?;
    let text = d
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no text"))?;
    put(&inbox_path(dir, from, name), text.as_bytes(), false)
}

// ── the hook ────────────────────────────────────────────────────────────────

/// `claude-sessions mail-hook`, on `SessionStart` and `UserPromptSubmit`: one line per message
/// in `in/` newer than the cursor, oldest first —
///
/// `new message from <from>, <date>, at <path>`
///
/// — and nothing at all when there is none, since it runs on every prompt. The cursor is
/// `.mail/.cursor`, written by the receiver and never by the hub: the newest modification
/// time announced, in nanoseconds, and under it the messages at exactly that time, because a
/// file's time is the kernel's coarse clock and two deliveries a few milliseconds apart share
/// one — a message landing in the same tick as the newest already announced would otherwise
/// never be said. A message is announced once; the file stays where it is for the agent to
/// read and keep or remove as it sees fit. Errors are logged and swallowed, as the registry
/// hook's are: a hook that fails is shown to the person, and this one's job is to say one
/// line or none.
pub fn hook(say: &mut dyn Write, log: &dyn Fn(&str)) -> io::Result<()> {
    // Claude Code writes the event on stdin; it is not needed, but left unread it could fail
    // the writer with a closed pipe.
    let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
    let dir = container_dir();
    let cursor_path = dir.join(".cursor");
    let cursor = Cursor::parse(&fs::read_to_string(&cursor_path).unwrap_or_default());
    let fresh = inbox_since(&dir, &cursor);
    if fresh.is_empty() {
        return Ok(());
    }
    for m in &fresh {
        writeln!(
            say,
            "new message from {}, {}, at {}",
            m.from,
            m.date,
            m.path.display()
        )?;
    }
    let moved = cursor.after(&fresh);
    if let Err(e) = put(&cursor_path, moved.to_string().as_bytes(), false) {
        log(&format!("mail-hook: cursor not written: {e}"));
    }
    Ok(())
}

/// What the hook has announced: everything up to `at`, and at `at` itself the `seen`
/// messages (`<from>/<name>`). Missing or unreadable, it is the beginning: every message in
/// the inbox is new.
#[derive(Debug, PartialEq)]
struct Cursor {
    at: u128,
    seen: Vec<String>,
}

impl Cursor {
    fn parse(text: &str) -> Cursor {
        let mut lines = text.lines();
        let at = lines.next().and_then(|l| l.trim().parse().ok());
        match at {
            Some(at) => Cursor {
                at,
                seen: lines
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect(),
            },
            None => Cursor {
                at: 0,
                seen: Vec::new(),
            },
        }
    }

    fn covers(&self, m: &Arrived) -> bool {
        m.mtime < self.at || (m.mtime == self.at && self.seen.contains(&m.key))
    }

    /// The cursor once `fresh` (every message this one did not cover) is announced.
    fn after(&self, fresh: &[Arrived]) -> Cursor {
        let at = fresh.iter().map(|m| m.mtime).max().unwrap_or(self.at);
        let mut seen: Vec<String> = if at == self.at {
            self.seen.clone()
        } else {
            Vec::new()
        };
        seen.extend(
            fresh
                .iter()
                .filter(|m| m.mtime == at)
                .map(|m| m.key.clone()),
        );
        Cursor { at, seen }
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.at)?;
        for k in &self.seen {
            writeln!(f, "{k}")?;
        }
        Ok(())
    }
}

struct Arrived {
    /// `<from>/<name>`, what the cursor records.
    key: String,
    mtime: u128,
    from: String,
    date: String,
    path: PathBuf,
}

/// Every message in `in/` the cursor does not cover, oldest first — by time, and by the
/// hub's stamp in the name where the times tie.
fn inbox_since(dir: &Path, cursor: &Cursor) -> Vec<Arrived> {
    let mut found = Vec::new();
    let Ok(froms) = fs::read_dir(dir.join("in")) else {
        return found;
    };
    for from in froms.filter_map(Result::ok) {
        let from_name = from.file_name().to_string_lossy().into_owned();
        if from_name.starts_with('.') || !from.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(from.path()) else {
            continue;
        };
        for f in files.filter_map(Result::ok) {
            let name = f.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let mtime = mtime_ns(&meta);
            let date = header_date(&f.path()).unwrap_or_else(|| utc((mtime / 1_000_000) as u64));
            let m = Arrived {
                key: format!("{from_name}/{name}"),
                mtime,
                from: from_name.clone(),
                date,
                path: f.path(),
            };
            if !cursor.covers(&m) {
                found.push(m);
            }
        }
    }
    found.sort_by(|a, b| {
        a.mtime
            .cmp(&b.mtime)
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });
    found
}

/// The `Date:` the hub stamped, from the header — the lines before the first blank one.
fn header_date(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .take_while(|l| !l.trim().is_empty())
        .find_map(|l| l.strip_prefix("Date: "))
        .map(|d| d.trim().to_string())
}

// ── the hub's loop ──────────────────────────────────────────────────────────

/// The loop's settings, from the hub's environment (infra's compose sets each, one line):
///
/// | variable | default | |
/// |---|---|---|
/// | `HUB_MAIL` | `$HOME/mail` | the volume: `<to>/<from>/<stamp>.md`, `delivered/` beside |
/// | `HUB_MAIL_MAX_BYTES` | `16384` | a larger message is bounced |
/// | `HUB_MAIL_MAX_QUEUE` | `200` | undelivered messages a receiver may have; more are bounced |
/// | `HUB_MAIL_RETAIN_DAYS` | `90` | the volume is pruned of older messages, delivered or not |
/// | `HUB_MAIL_TICK` | `15` | seconds between passes |
pub struct Limits {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pub max_queue: usize,
    pub retain: Duration,
    pub tick: Duration,
}

impl Limits {
    pub fn from_env() -> Limits {
        let num = |key: &str, default: u64| -> u64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        let dir = match std::env::var_os("HUB_MAIL") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/home/dev"))
                .join("mail"),
        };
        Limits {
            dir,
            max_bytes: num("HUB_MAIL_MAX_BYTES", 16_384),
            max_queue: num("HUB_MAIL_MAX_QUEUE", 200) as usize,
            retain: Duration::from_secs(num("HUB_MAIL_RETAIN_DAYS", 90) * 86_400),
            tick: Duration::from_secs(num("HUB_MAIL_TICK", 15).max(1)),
        }
    }
}

/// How long one container's turn may take: a reply that is slower than this means the
/// container, or ssh to it, has stopped, and the next tick asks again.
const REPLY_WAIT: Duration = Duration::from_secs(30);
/// How often the volume is pruned by age.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// `claude-sessions mail-loop [--once]`: a pass over every container, then sleep, for ever —
/// or once, for a test and for infra's CI round trip. Each line it prints is one event,
/// stamped; infra's entrypoint keeps them in a log.
pub fn run(once: bool) -> io::Result<()> {
    let limits = Limits::from_env();
    let mut hosts = hub::hosts();
    if let Some(i) = hosts.iter().position(|h| h == HUB) {
        say(&format!(
            "a container called {HUB:?} is skipped: that is the reserved sender"
        ));
        hosts.remove(i);
    }
    fs::create_dir_all(&limits.dir)?;
    say(&format!(
        "carrying mail between {} in {}, every {}s; limit {} bytes, {} a queue, kept {} days",
        hosts.join(" "),
        limits.dir.display(),
        limits.tick.as_secs(),
        limits.max_bytes,
        limits.max_queue,
        limits.retain.as_secs() / 86_400,
    ));
    let mut down: BTreeSet<String> = BTreeSet::new();
    let mut pruned_at: Option<Instant> = None;
    loop {
        if pruned_at.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            match prune(&limits) {
                Ok(0) => {}
                Ok(n) => say(&format!(
                    "pruned {n} message(s) older than {} days",
                    limits.retain.as_secs() / 86_400
                )),
                Err(e) => say(&format!("prune failed: {e}")),
            }
            pruned_at = Some(Instant::now());
        }
        // Every outbox first, then every queue, so a message lands in the tick it was sent
        // whichever of the two containers comes first in the list.
        let mut links = Vec::new();
        for host in &hosts {
            match pull_host(host, &hosts, &limits) {
                Ok(link) => {
                    if down.remove(host) {
                        say(&format!("{host} is answering again"));
                    }
                    links.push((host, link));
                }
                Err(why) => {
                    if down.insert(host.clone()) {
                        say(&format!("{host} is not answering: {why}; its mail waits"));
                    }
                }
            }
        }
        for (host, mut link) in links {
            deliver(&mut link, host, &limits);
        }
        // A heartbeat for the hub's health check, if infra reads it.
        let _ = fs::write(limits.dir.join(".beat"), b"");
        if once {
            return Ok(());
        }
        std::thread::sleep(limits.tick);
    }
}

fn say(line: &str) {
    say!("{} {line}", utc(now_ms()));
}

/// One container's agent for the length of a turn: requests down its stdin, replies read on a
/// thread so a container that stops mid-answer costs a wait, not the loop.
struct Link {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    replies: mpsc::Receiver<Value>,
}

impl Link {
    fn open(host: &str) -> Result<Link, String> {
        let mut cmd = hub::command(host, &["mail-agent"], false);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("could not run ssh: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            let Some(out) = stdout else { return };
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = json::parse(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Link {
            child,
            stdin,
            replies,
        })
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        let Some(pipe) = self.stdin.as_mut() else {
            return Err("not connected".into());
        };
        let wrote = writeln!(pipe, "{}", json::to_line(v)).and_then(|_| pipe.flush());
        // A pipe that broke under the first request is ssh gone before it was read — a
        // refused connection, usually — and ssh's own last line says why better than EPIPE.
        wrote.map_err(|_| self.why_gone())
    }

    fn reply(&mut self) -> Result<Value, String> {
        match self.replies.recv_timeout(REPLY_WAIT) {
            Ok(v) => Ok(v),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("no answer in {}s", REPLY_WAIT.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.why_gone()),
        }
    }

    /// ssh's own last word, once the channel has closed without an answer.
    fn why_gone(&mut self) -> String {
        let _ = self.stdin.take();
        let mut err = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = io::Read::read_to_string(&mut e, &mut err);
        }
        let status = self.child.wait().ok();
        match err.lines().rfind(|l| !l.trim().is_empty()) {
            Some(l) => l.trim().to_string(),
            None => match status.and_then(|s| s.code()) {
                Some(c) => format!("its agent exited {c}"),
                None => "its agent was stopped".to_string(),
            },
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.stdin.take();
        // End of input ends the agent; give it a moment, then make sure.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A container's outbox pulled into the volume, over a link left open for its delivery.
fn pull_host(host: &str, hosts: &[String], limits: &Limits) -> Result<Link, String> {
    let mut link = Link::open(host)?;
    let mut pull = Value::obj();
    pull.set("max", Value::num(limits.max_bytes as f64));
    link.send(&reply("pull", pull))?;
    let mut files = Vec::new();
    loop {
        let v = link.reply()?;
        if let Some(f) = v.get("file") {
            files.push(f.clone());
        } else if v.get("pulled").is_some() {
            break;
        }
    }
    for f in files {
        let Some(to) = f.get("to").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = f.get("name").and_then(Value::as_str) else {
            continue;
        };
        let size = f.get("size").and_then(Value::as_u64).unwrap_or(0);
        let refusal = refusal_of(&f, to, hosts, limits);
        let kept = match &refusal {
            None => {
                let text = f.get("text").and_then(Value::as_str).unwrap_or("");
                store(&limits.dir, to, host, text).map(|stored| {
                    say(&format!("{host} -> {to}: {stored} ({size} bytes)"));
                })
            }
            Some(why) => {
                say(&format!(
                    "{host} -> {}: refused, {why}; bounced",
                    to.escape_debug()
                ));
                bounce(limits, host, to, name, size, why, hosts)
            }
        };
        match kept {
            Ok(()) => {
                let mut del = Value::obj();
                del.set("to", Value::string(to));
                del.set("name", Value::string(name));
                link.send(&reply("delete", del))?;
                let v = link.reply()?;
                if let Some(e) = v.get("error").and_then(Value::as_str) {
                    say(&format!("{host}: outbox file not deleted: {e}"));
                }
            }
            Err(e) => say(&format!("{host} -> {}: not stored: {e}", to.escape_debug())),
        }
    }
    Ok(link)
}

/// Why a pulled file is bounced rather than stored, if it is.
fn refusal_of(f: &Value, to: &str, hosts: &[String], limits: &Limits) -> Option<String> {
    if to == HUB {
        return Some("the hub takes no mail".to_string());
    }
    if !plain(to) || !hosts.iter().any(|h| h == to) {
        return Some(format!("unknown receiver {}", to.escape_debug()));
    }
    match f.get("refused").and_then(Value::as_str) {
        Some("oversize") => {
            return Some(format!("over the size limit of {} bytes", limits.max_bytes));
        }
        Some(_) => return Some("not UTF-8 text".to_string()),
        None => {}
    }
    if f.get("text").and_then(Value::as_str).is_none() {
        return Some("not UTF-8 text".to_string());
    }
    if pending(&limits.dir, to) >= limits.max_queue {
        return Some(format!(
            "{to}'s queue is full ({} undelivered)",
            limits.max_queue
        ));
    }
    None
}

/// Store a message in the volume with the hub's header, durably; the stored name.
fn store(dir: &Path, to: &str, from: &str, body: &str) -> io::Result<String> {
    let qdir = dir.join(to).join(from);
    fs::create_dir_all(&qdir)?;
    let now = now_ms();
    let base = stamp(now);
    let mut name = format!("{base}.md");
    let mut n = 1;
    while qdir.join(&name).exists() || qdir.join("delivered").join(&name).exists() {
        n += 1;
        name = format!("{base}-{n}.md");
    }
    let text = format!("From: {from}\nTo: {to}\nDate: {}\n\n{body}", utc(now));
    put(&qdir.join(&name), text.as_bytes(), true)?;
    Ok(name)
}

/// A refusal, as mail from `hub` to the sender. Its queue full too is the one case nothing
/// can be said: logged, and the sender's file still goes.
fn bounce(
    limits: &Limits,
    sender: &str,
    to: &str,
    name: &str,
    size: u64,
    why: &str,
    hosts: &[String],
) -> io::Result<()> {
    if pending(&limits.dir, sender) >= limits.max_queue {
        say(&format!(
            "{sender}: its own queue is full, so the bounce was dropped"
        ));
        return Ok(());
    }
    let body = format!(
        "Your message to {} — {} in your outbox, {size} bytes — was not delivered: {why}.\n\
         It has been removed from your outbox and nothing was kept; fix it and send it again.\n\
         The containers that take mail are: {}.\n",
        to.escape_debug(),
        name.escape_debug(),
        hosts.join(", ")
    );
    store(&limits.dir, sender, HUB, &body).map(|_| ())
}

/// A queue's undelivered messages: every `<from>/<stamp>.md` under `<dir>/<to>/`, oldest
/// first by name, which is by time.
fn queue(dir: &Path, to: &str) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(froms) = fs::read_dir(dir.join(to)) else {
        return out;
    };
    for from in froms.filter_map(Result::ok) {
        let from_name = from.file_name().to_string_lossy().into_owned();
        if !plain(&from_name) || !from.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(from.path()) else {
            continue;
        };
        for f in files.filter_map(Result::ok) {
            let name = f.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") && f.file_type().is_ok_and(|t| t.is_file()) {
                out.push((from_name.clone(), f.path()));
            }
        }
    }
    out.sort_by(|a, b| a.1.file_name().cmp(&b.1.file_name()));
    out
}

fn pending(dir: &Path, to: &str) -> usize {
    queue(dir, to).len()
}

/// Push `host`'s queue into its inbox, marking each delivered as it is acknowledged.
fn deliver(link: &mut Link, host: &str, limits: &Limits) {
    for (from, path) in queue(&limits.dir, host) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(text) = fs::read_to_string(&path) else {
            say(&format!("{host}: {from}/{name} could not be read; left"));
            continue;
        };
        let mut d = Value::obj();
        d.set("from", Value::string(&*from));
        d.set("name", Value::string(&*name));
        d.set("text", Value::string(text));
        let delivered = link
            .send(&reply("deliver", d))
            .and_then(|_| link.reply())
            .map(|v| v.get("delivered").and_then(Value::as_bool) == Some(true));
        match delivered {
            Ok(true) => {
                let done = path.parent().map(|p| p.join("delivered"));
                let moved = done
                    .as_ref()
                    .map_or(Err(io::Error::other("no directory")), |d| {
                        fs::create_dir_all(d).and_then(|_| fs::rename(&path, d.join(&name)))
                    });
                match moved {
                    Ok(()) => say(&format!("delivered to {host}: {from}/{name}")),
                    Err(e) => say(&format!(
                        "delivered to {host}: {from}/{name}, but not marked so: {e}"
                    )),
                }
            }
            Ok(false) => say(&format!(
                "{host}: {from}/{name} not written to its inbox; will try again"
            )),
            Err(e) => {
                say(&format!("{host}: delivery stopped: {e}; will try again"));
                return;
            }
        }
    }
}

/// Remove every message older than the retention, delivered or not; how many went.
fn prune(limits: &Limits) -> io::Result<usize> {
    let cutoff = SystemTime::now() - limits.retain;
    let mut gone = 0;
    for to in fs::read_dir(&limits.dir)?.filter_map(Result::ok) {
        if !to.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for from in fs::read_dir(to.path())?.filter_map(Result::ok) {
            if !from.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for d in [from.path(), from.path().join("delivered")] {
                let Ok(files) = fs::read_dir(&d) else {
                    continue;
                };
                for f in files.filter_map(Result::ok) {
                    let Ok(meta) = f.metadata() else { continue };
                    let old = meta.is_file() && meta.modified().is_ok_and(|m| m < cutoff);
                    if old && fs::remove_file(f.path()).is_ok() {
                        gone += 1;
                    }
                }
            }
        }
    }
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs-mail-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn settled(path: &Path) {
        let t = SystemTime::now() - Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn names_are_one_component_and_plain_ones_are_ascii() {
        for ok in ["infra", "a.b-c_d", "hello world.md"] {
            assert!(component(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\0"] {
            assert!(!component(bad), "{bad:?}");
        }
        assert!(plain("destiny-director"));
        for bad in [".hidden", "hello world", "é", "a/b"] {
            assert!(!plain(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_agent_pulls_settled_files_refuses_as_asked_deletes_and_delivers() {
        let dir = tmp("agent");
        let out = dir.join("out/infra");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("hello.md"), "hi\n").unwrap();
        settled(&out.join("hello.md"));
        fs::write(out.join("big.md"), "x".repeat(50)).unwrap();
        settled(&out.join("big.md"));
        fs::write(out.join("bin.md"), [0xff, 0xfe]).unwrap();
        settled(&out.join("bin.md"));
        fs::write(out.join("fresh.md"), "still being written\n").unwrap();
        fs::write(out.join(".tmp.md"), "temporary\n").unwrap();
        settled(&out.join(".tmp.md"));
        std::os::unix::fs::symlink("/etc/hostname", out.join("link.md")).unwrap();
        let req = json::parse(r#"{"pull":{"max":40}}"#).unwrap();
        let got = answer(&dir, &req);
        let lines: Vec<String> = got.iter().map(json::to_line).collect();
        assert_eq!(
            lines,
            [
                r#"{"file":{"name":"big.md","refused":"oversize","size":50,"to":"infra"}}"#,
                r#"{"file":{"name":"bin.md","refused":"not text","size":2,"to":"infra"}}"#,
                r#"{"file":{"name":"hello.md","size":3,"text":"hi\n","to":"infra"}}"#,
                r#"{"pulled":3}"#,
            ],
            "the fresh file, the temporary one and the symlink are not offered"
        );
        let del = json::parse(r#"{"delete":{"to":"infra","name":"hello.md"}}"#).unwrap();
        assert_eq!(json::to_line(&answer(&dir, &del)[0]), r#"{"deleted":true}"#);
        assert!(!out.join("hello.md").exists());
        let bad = json::parse(r#"{"delete":{"to":"..","name":"hello.md"}}"#).unwrap();
        assert!(answer(&dir, &bad)[0].get("error").is_some());
        let d =
            json::parse(r#"{"deliver":{"from":"or3","name":"s.md","text":"From: or3\n\nyo\n"}}"#)
                .unwrap();
        assert_eq!(json::to_line(&answer(&dir, &d)[0]), r#"{"delivered":true}"#);
        assert_eq!(
            fs::read_to_string(dir.join("in/or3/s.md")).unwrap(),
            "From: or3\n\nyo\n"
        );
        let bad = json::parse(r#"{"deliver":{"from":"../x","name":"s.md","text":""}}"#).unwrap();
        assert!(answer(&dir, &bad)[0].get("error").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inbox_is_announced_once_oldest_first_with_the_stamped_date() {
        let dir = tmp("inbox");
        let a = inbox_path(&dir, "infra", "1.md");
        let b = inbox_path(&dir, "or3", "2.md");
        put(
            &a,
            b"From: infra\nTo: me\nDate: 2026-10-28T03:15:00Z\n\nhi\n",
            false,
        )
        .unwrap();
        put(&b, b"no header\n", false).unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_157_300);
        let set = |p: &Path, t| {
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap()
        };
        set(&a, t + Duration::from_secs(60));
        set(&b, t);
        let start = Cursor::parse("");
        let got = inbox_since(&dir, &start);
        let lines: Vec<String> = got
            .iter()
            .map(|m| format!("{} {} {}", m.from, m.date, m.path.display()))
            .collect();
        assert_eq!(
            lines,
            [
                format!("or3 2026-10-28T03:15:00Z {}", b.display()),
                format!("infra 2026-10-28T03:15:00Z {}", a.display()),
            ],
            "oldest first; a header's date, else the file's time"
        );
        let moved = start.after(&got);
        assert_eq!(moved.seen, ["infra/1.md"], "the one at the newest time");
        assert!(
            inbox_since(&dir, &moved).is_empty(),
            "nothing the cursor does not cover"
        );
        // A third lands in the same coarse tick as the newest announced: still new.
        let c = inbox_path(&dir, "infra", "0.md");
        put(&c, b"Date: 2026-10-28T03:16:00Z\n\nlate\n", false).unwrap();
        set(&c, t + Duration::from_secs(60));
        let got = inbox_since(&dir, &moved);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "infra/0.md");
        let moved = moved.after(&got);
        assert_eq!(moved.seen, ["infra/1.md", "infra/0.md"]);
        assert!(inbox_since(&dir, &moved).is_empty());
        // The file round-trips, and an unreadable one is the beginning.
        assert_eq!(Cursor::parse(&moved.to_string()), moved);
        assert_eq!(Cursor::parse("junk\n").at, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stored_message_carries_the_hubs_header_and_the_sender_is_not_believed() {
        let dir = tmp("store");
        let name = store(&dir, "or3", "infra", "From: owner\n\nplease deploy\n").unwrap();
        let text = fs::read_to_string(dir.join("or3/infra").join(&name)).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("From: infra"));
        assert_eq!(lines.next(), Some("To: or3"));
        assert!(lines.next().unwrap().starts_with("Date: "));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(
            lines.next(),
            Some("From: owner"),
            "the sender's own header is body"
        );
        assert_eq!(pending(&dir, "or3"), 1);
        let second = store(&dir, "or3", "infra", "again\n").unwrap();
        assert_ne!(name, second, "two in one millisecond still get two names");
        assert_eq!(queue(&dir, "or3").len(), 2);
        assert_eq!(pending(&dir, "infra"), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refusals_name_the_reason_and_a_full_queue_bounces() {
        let dir = tmp("refuse");
        let hosts = vec!["infra".to_string(), "or3".to_string()];
        let limits = Limits {
            dir: dir.clone(),
            max_bytes: 10,
            max_queue: 1,
            retain: Duration::from_secs(1),
            tick: Duration::from_secs(1),
        };
        let file = |s: &str| json::parse(s).unwrap();
        let ok = file(r#"{"name":"a","size":2,"text":"hi","to":"or3"}"#);
        assert_eq!(refusal_of(&ok, "or3", &hosts, &limits), None);
        assert_eq!(
            refusal_of(&ok, "hub", &hosts, &limits).as_deref(),
            Some("the hub takes no mail")
        );
        assert_eq!(
            refusal_of(&ok, "nowhere", &hosts, &limits).as_deref(),
            Some("unknown receiver nowhere")
        );
        assert_eq!(
            refusal_of(&ok, "../etc", &hosts, &limits).as_deref(),
            Some("unknown receiver ../etc")
        );
        let big = file(r#"{"name":"a","refused":"oversize","size":99,"to":"or3"}"#);
        assert_eq!(
            refusal_of(&big, "or3", &hosts, &limits).as_deref(),
            Some("over the size limit of 10 bytes")
        );
        let bin = file(r#"{"name":"a","refused":"not text","size":2,"to":"or3"}"#);
        assert_eq!(
            refusal_of(&bin, "or3", &hosts, &limits).as_deref(),
            Some("not UTF-8 text")
        );
        store(&dir, "or3", "infra", "one\n").unwrap();
        assert_eq!(
            refusal_of(&ok, "or3", &hosts, &limits).as_deref(),
            Some("or3's queue is full (1 undelivered)")
        );
        bounce(&limits, "infra", "or3", "a", 2, "queue full", &hosts).unwrap();
        let q = queue(&dir, "infra");
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, HUB);
        let text = fs::read_to_string(&q[0].1).unwrap();
        assert!(text.starts_with("From: hub\nTo: infra\n"), "{text}");
        assert!(text.contains("was not delivered: queue full"), "{text}");
        // Everything is old enough to prune now.
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(prune(&limits).unwrap(), 2);
        assert_eq!(queue(&dir, "or3").len() + queue(&dir, "infra").len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
