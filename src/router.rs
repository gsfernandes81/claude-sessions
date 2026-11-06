//! The hub's half of agent messaging: `claude-sessions router`. It carries Claude Code's own
//! cross-session messages between containers. `docs/design.md` § *Agent messaging* is the
//! specification, and `peer.rs` is the container's half.
//!
//! **What it holds.** One `claude-sessions agent` per container over the hub's ssh, as the menu
//! does. Each agent's rows say which slots are live. Each agent is sent the **roster**: every
//! slot in the *other* containers, for which it keeps a stand-in its local claudes can list
//! and message. A frame a local claude wrote to a stand-in comes here as `peer`, and goes out
//! to the target's container as `deliver`.
//!
//! **Who sent it is the hub's to say.** The ssh connection a frame arrives on names the
//! container, and the container's agent names the slot, from the pid at the socket. The hub
//! accepts that slot only while the container's own rows list it live. One repo's slots are
//! one trust domain, as they were for mail.
//!
//! **Kept until it can be given.** A message to a slot that is offloaded, or whose container
//! is not answering, is written to disk (`HUB_QUEUE`, fsynced with its directory) and
//! delivered when the slot is next live, as is one the target's agent bounced. Control frames
//! (receipts, idle notices) are about now and are dropped instead.
//!
//! **Never a body in the log.** One line per frame: who to whom, and what became of it.

use crate::clock;
use crate::fmt::utc;
use crate::hub::{self, Conn, Event};
use crate::json::{self, Value};
use crate::peer::{self, Entry};
use crate::proto::Snapshot;
use crate::ui::RowKey;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Messages one slot may have waiting.
pub const MAX_QUEUE: usize = 200;
/// How long a waiting message is kept: about what Claude Code keeps an unread one.
pub const RETAIN_MS: u64 = 7 * 86_400_000;
/// How often the router says it is alive, in `.beat` beside the queue.
pub const BEAT: Duration = Duration::from_secs(60);

fn say(line: &str) {
    let _ = writeln!(io::stdout(), "{} {line}", utc(clock::now()));
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ── what the containers say ─────────────────────────────────────────────────

/// One slot, as its container's rows show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub slot: String,
    pub title: String,
    pub busy: bool,
    pub live: bool,
}

/// A container's claude slots that can take a message now or later: running, or offloaded.
/// A closed or archived slot, a conversation row and a pi slot are not addresses.
pub fn slots_of(snap: &Snapshot) -> Vec<Slot> {
    snap.rows
        .iter()
        .filter_map(|r| {
            let RowKey::Slot(name) = &r.key else {
                return None;
            };
            let claude = r.harness() == crate::harness::Harness::Claude;
            let wanted = claude && !r.archived && (!r.closed || r.offloaded);
            (wanted && peer::is_name(name)).then(|| Slot {
                slot: name.clone(),
                title: r.title.clone(),
                busy: r.busy,
                live: !(r.offloaded || r.closed),
            })
        })
        .collect()
}

/// What the router knows of every container.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    /// Each container's slots as last heard, kept while it is not answering so its messages
    /// are kept rather than refused.
    pub slots: BTreeMap<String, Vec<Slot>>,
    /// Whether each container's agent is answering now.
    pub up: BTreeMap<String, bool>,
}

impl Fleet {
    fn slot(&self, repo: &str, slot: &str) -> Option<&Slot> {
        self.slots.get(repo)?.iter().find(|s| s.slot == slot)
    }

    /// Whether `repo/slot` can be given a message this moment.
    pub fn reachable(&self, repo: &str, slot: &str) -> bool {
        self.up.get(repo).copied().unwrap_or(false) && self.slot(repo, slot).is_some_and(|s| s.live)
    }

    /// What `host`'s agent is to hold stand-ins for: every slot in every other container.
    pub fn roster_for(&self, host: &str) -> Vec<Entry> {
        self.slots
            .iter()
            .filter(|(repo, _)| repo.as_str() != host)
            .flat_map(|(repo, slots)| {
                let up = self.up.get(repo).copied().unwrap_or(false);
                slots.iter().map(move |s| Entry {
                    repo: repo.clone(),
                    slot: s.slot.clone(),
                    title: s.title.clone(),
                    busy: up && s.live && s.busy,
                    live: up && s.live,
                })
            })
            .collect()
    }
}

// ── routing ─────────────────────────────────────────────────────────────────

/// A frame on its way to one slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// `repo/slot`.
    pub to: String,
    /// `repo/slot`, as the hub verified it.
    pub from: String,
    pub frame: Value,
    /// When the hub first had it.
    pub at: u64,
}

impl Item {
    fn is_message(&self) -> bool {
        self.frame.get("type").and_then(Value::as_str) == Some("user")
    }

    fn kind(&self) -> &str {
        match self.frame.get("type").and_then(Value::as_str) {
            Some("user") => "message",
            _ => self
                .frame
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("control"),
        }
    }

    fn to_value(&self) -> Value {
        let mut v = Value::obj();
        v.set("to", Value::string(&self.to));
        v.set("from", Value::string(&self.from));
        v.set("frame", self.frame.clone());
        v.set("at", Value::num(self.at as f64));
        v
    }

    fn from_value(v: &Value) -> Option<Item> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Item {
            to: s("to")?,
            from: s("from")?,
            frame: v.get("frame")?.clone(),
            at: v.get("at").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    /// What the target's agent is sent.
    fn deliver(&self) -> Value {
        let (_, slot) = peer::split_addr(&self.to).unwrap_or_default();
        let mut d = Value::obj();
        d.set("to", Value::string(slot));
        d.set("from", Value::string(&self.from));
        d.set("frame", self.frame.clone());
        let mut msg = Value::obj();
        msg.set("deliver", d);
        msg
    }
}

/// What becomes of a frame a container's agent handed up.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// To the target's container now.
    Deliver(String, Item),
    /// Kept until the target is live.
    Keep(Item),
    /// Not passed on, and why.
    Drop(String),
}

/// Route a `peer` message from `host`'s agent: `{"from":slot,"to":"repo/slot","frame":…}`.
pub fn route(fleet: &Fleet, host: &str, peer: &Value, now: u64) -> Route {
    let s = |k: &str| peer.get(k).and_then(Value::as_str).unwrap_or_default();
    let (from_slot, to) = (s("from"), s("to"));
    let Some(frame) = peer.get("frame") else {
        return Route::Drop("no frame".into());
    };
    if !matches!(
        frame.get("type").and_then(Value::as_str),
        Some("user") | Some("control")
    ) {
        return Route::Drop("not a message or a control frame".into());
    }
    if json::to_line(frame).len() > peer::MAX_LINE {
        return Route::Drop("over Claude Code's line cap".into());
    }
    if !fleet.slot(host, from_slot).is_some_and(|s| s.live) {
        return Route::Drop(format!("{host}/{from_slot} is not a live slot"));
    }
    let Some((repo, slot)) = peer::split_addr(to) else {
        return Route::Drop(format!("{to:?} is not an address"));
    };
    if repo == host {
        return Route::Drop("its own container's sessions are reached without the hub".into());
    }
    if fleet.slot(repo, slot).is_none() {
        return Route::Drop(format!("no slot {to}"));
    }
    let item = Item {
        to: to.to_string(),
        from: format!("{host}/{from_slot}"),
        frame: frame.clone(),
        at: now,
    };
    if fleet.reachable(repo, slot) {
        Route::Deliver(repo.to_string(), item)
    } else if item.is_message() {
        Route::Keep(item)
    } else {
        Route::Drop(format!(
            "{to} is not live, and a {} is about now",
            item.kind()
        ))
    }
}

// ── the queue ───────────────────────────────────────────────────────────────

/// Messages waiting for their slot, on disk under `dir` (one file each, `<seq>.json`) or only
/// in memory for the tests.
pub struct Queue {
    dir: Option<PathBuf>,
    items: BTreeMap<u64, Item>,
    next: u64,
}

impl Queue {
    #[cfg(test)]
    pub fn memory() -> Queue {
        Queue {
            dir: None,
            items: BTreeMap::new(),
            next: 1,
        }
    }

    /// The queue in `dir`, with the names of files that could not be read.
    pub fn open(dir: &Path) -> io::Result<(Queue, Vec<String>)> {
        fs::create_dir_all(dir)?;
        let mut q = Queue {
            dir: Some(dir.to_path_buf()),
            items: BTreeMap::new(),
            next: 1,
        };
        let mut skipped = Vec::new();
        for e in fs::read_dir(dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(seq) = name
                .strip_suffix(".json")
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            let item = fs::read_to_string(e.path())
                .ok()
                .and_then(|t| json::parse(&t).ok())
                .and_then(|v| Item::from_value(&v));
            match item {
                Some(i) => {
                    q.items.insert(seq, i);
                    q.next = q.next.max(seq + 1);
                }
                None => skipped.push(name),
            }
        }
        Ok((q, skipped))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Keep `item`, unless its slot already has [`MAX_QUEUE`] waiting.
    pub fn push(&mut self, item: Item) -> Result<(), String> {
        let waiting = self.items.values().filter(|i| i.to == item.to).count();
        if waiting >= MAX_QUEUE {
            return Err(format!("{} already has {MAX_QUEUE} waiting", item.to));
        }
        let seq = self.next;
        if let Some(dir) = &self.dir {
            let line = json::to_line(&item.to_value());
            put(&dir.join(format!("{seq:020}.json")), line.as_bytes())
                .map_err(|e| format!("could not be kept: {e}"))?;
        }
        self.next += 1;
        self.items.insert(seq, item);
        Ok(())
    }

    /// Everything waiting for a slot `ready` says can take it now, oldest first, taken out.
    pub fn take(&mut self, ready: impl Fn(&str) -> bool) -> Vec<Item> {
        let seqs: Vec<u64> = self
            .items
            .iter()
            .filter(|(_, i)| ready(&i.to))
            .map(|(s, _)| *s)
            .collect();
        seqs.into_iter()
            .filter_map(|s| {
                self.forget(s);
                self.items.remove(&s)
            })
            .collect()
    }

    fn forget(&self, seq: u64) {
        if let Some(dir) = &self.dir {
            let _ = fs::remove_file(dir.join(format!("{seq:020}.json")));
        }
    }

    /// Drop what has waited past [`RETAIN_MS`]; what was dropped.
    pub fn prune(&mut self, now: u64) -> Vec<Item> {
        self.take_where(|i| now.saturating_sub(i.at) > RETAIN_MS)
    }

    fn take_where(&mut self, f: impl Fn(&Item) -> bool) -> Vec<Item> {
        let seqs: Vec<u64> = self
            .items
            .iter()
            .filter(|(_, i)| f(i))
            .map(|(s, _)| *s)
            .collect();
        seqs.into_iter()
            .filter_map(|s| {
                self.forget(s);
                self.items.remove(&s)
            })
            .collect()
    }
}

/// `bytes` at `path` durably: a temporary file fsynced, renamed, and the directory fsynced.
fn put(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

// ── the router ──────────────────────────────────────────────────────────────

struct Router {
    fleet: Mutex<Fleet>,
    conns: Mutex<HashMap<String, Arc<Conn>>>,
    /// The roster each agent was last sent, so it is sent again only when it changes.
    sent: Mutex<HashMap<String, Value>>,
    queue: Mutex<Queue>,
}

impl Router {
    /// One line for `.beat`: the time, how many containers answer, how many messages wait.
    /// Made only by taking every lock the router's work takes, one at a time, so a router
    /// wedged on one stops beating, and infra's health check sees it as stale rather than as
    /// a process that is merely running.
    fn pulse(&self) -> String {
        let (up, all) = {
            let fleet = lock(&self.fleet);
            (fleet.up.values().filter(|u| **u).count(), fleet.up.len())
        };
        drop(lock(&self.conns));
        drop(lock(&self.sent));
        let waiting = lock(&self.queue).len();
        format!(
            "{} {up}/{all} answering, {waiting} waiting\n",
            utc(clock::now())
        )
    }
}

/// Where waiting messages are kept: `HUB_QUEUE`, else `$HOME/queue`. A persisted volume, since
/// every deploy recreates the hub.
pub fn queue_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HUB_QUEUE").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    PathBuf::from(home).join("queue")
}

/// `claude-sessions router`: serve for ever.
pub fn serve() -> io::Result<()> {
    let mut hosts = hub::hosts();
    hosts.retain(|h| {
        let ok = peer::is_name(h);
        if !ok {
            say(&format!(
                "{h:?} is not relayed: not a name a container may have"
            ));
        }
        ok
    });
    let dir = queue_dir();
    let (queue, skipped) = Queue::open(&dir)?;
    for name in skipped {
        say(&format!(
            "{name} in the queue could not be read; left there"
        ));
    }
    say(&format!(
        "relaying for {}; {} waiting in {}",
        hosts.join(" "),
        queue.len(),
        dir.display()
    ));
    let router = Arc::new(Router {
        fleet: Mutex::new(Fleet::default()),
        conns: Mutex::new(HashMap::new()),
        sent: Mutex::new(HashMap::new()),
        queue: Mutex::new(queue),
    });
    for host in hosts {
        let router = router.clone();
        std::thread::spawn(move || watch(&router, &host));
    }
    {
        let router = router.clone();
        let beat = dir.join(".beat");
        std::thread::spawn(move || {
            loop {
                if let Err(e) = put(&beat, router.pulse().as_bytes()) {
                    say(&format!("{} could not be written: {e}", beat.display()));
                }
                std::thread::sleep(BEAT);
            }
        });
    }
    loop {
        for item in lock(&router.queue).prune(clock::now()) {
            say(&format!(
                "{} to {}: dropped, kept past {} days",
                item.from,
                item.to,
                RETAIN_MS / 86_400_000
            ));
        }
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn noop() {}

/// Hold `host`'s agent for ever: asked again when it goes, after 15 s at first
/// (`CLAUDE_SESSIONS_RETRY_MS`) and at most two minutes, and said once each way in the log.
fn watch(router: &Router, host: &str) {
    let first = crate::run::retry();
    let mut wait = first;
    let mut down: Option<String> = None;
    loop {
        let (tx, rx) = mpsc::channel();
        let conn = hub::connect(host, tx, noop);
        lock(&router.conns).insert(host.to_string(), conn);
        // A new agent holds no stand-ins yet: it is sent the roster whatever was sent before.
        lock(&router.sent).remove(host);
        for ev in rx {
            match ev {
                Event::Snapshot(_, snap) => {
                    if down.take().is_some() {
                        say(&format!("{host} is answering"));
                    }
                    wait = first;
                    {
                        let mut fleet = lock(&router.fleet);
                        fleet.slots.insert(host.to_string(), slots_of(&snap));
                        fleet.up.insert(host.to_string(), true);
                    }
                    rosters(router);
                    flush(router);
                }
                Event::Relay(msg) => relayed(router, host, &msg),
                Event::Down(_, why) => {
                    lock(&router.fleet).up.insert(host.to_string(), false);
                    if down.as_deref() != Some(why.as_str()) {
                        say(&format!(
                            "{host} is not answering: {why}; its messages wait"
                        ));
                    }
                    down = Some(why);
                    rosters(router);
                    break;
                }
            }
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(120).max(first));
    }
}

/// Send each answering agent its roster, where it changed.
fn rosters(router: &Router) {
    let fleet = lock(&router.fleet).clone();
    let conns = lock(&router.conns).clone();
    for (host, conn) in conns {
        if !fleet.up.get(&host).copied().unwrap_or(false) {
            continue;
        }
        let entries = Value::Arr(
            fleet
                .roster_for(&host)
                .iter()
                .map(Entry::to_value)
                .collect(),
        );
        if lock(&router.sent).get(&host) == Some(&entries) {
            continue;
        }
        let mut msg = Value::obj();
        msg.set("roster", entries.clone());
        if conn.tell(&msg).is_ok() {
            lock(&router.sent).insert(host, entries);
        }
    }
}

/// Hand on what was waiting for slots now live.
fn flush(router: &Router) {
    let fleet = lock(&router.fleet).clone();
    let ready = lock(&router.queue)
        .take(|to| peer::split_addr(to).is_some_and(|(repo, slot)| fleet.reachable(repo, slot)));
    for item in ready {
        let repo = peer::split_addr(&item.to)
            .map(|(r, _)| r.to_string())
            .unwrap_or_default();
        send(router, &repo, item, "was waiting");
    }
}

/// `item` to `repo`'s agent, or back in the queue when it cannot be told.
fn send(router: &Router, repo: &str, item: Item, note: &str) {
    let conn = lock(&router.conns).get(repo).cloned();
    let told = conn.map(|c| c.tell(&item.deliver()));
    match told {
        Some(Ok(())) => say(&format!(
            "{} to {}: {} handed on{}",
            item.from,
            item.to,
            item.kind(),
            if note.is_empty() {
                String::new()
            } else {
                format!(" ({note})")
            }
        )),
        _ => keep(router, item, &format!("{repo} could not be told")),
    }
}

fn keep(router: &Router, item: Item, why: &str) {
    if !item.is_message() {
        say(&format!(
            "{} to {}: {} dropped: {why}",
            item.from,
            item.to,
            item.kind()
        ));
        return;
    }
    let (from, to) = (item.from.clone(), item.to.clone());
    match lock(&router.queue).push(item) {
        Ok(()) => say(&format!("{from} to {to}: message kept: {why}")),
        Err(e) => say(&format!("{from} to {to}: message dropped: {e}")),
    }
}

/// What `host`'s relay said.
fn relayed(router: &Router, host: &str, msg: &Value) {
    if let Some(p) = msg.get("peer") {
        let fleet = lock(&router.fleet).clone();
        match route(&fleet, host, p, clock::now()) {
            Route::Deliver(repo, item) => send(router, &repo, item, ""),
            Route::Keep(item) => keep(router, item, "not live now"),
            Route::Drop(why) => {
                let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
                say(&format!(
                    "{host}/{} to {}: dropped: {why}",
                    s("from"),
                    s("to")
                ));
            }
        }
    } else if let Some(b) = msg.get("bounce") {
        let s = |k: &str| {
            b.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let Some(frame) = b.get("frame") else { return };
        let item = Item {
            to: format!("{host}/{}", s("to")),
            from: s("from"),
            frame: frame.clone(),
            at: clock::now(),
        };
        keep(router, item, &format!("bounced: {}", s("why")));
    } else if let Some(r) = msg.get("relay") {
        if let Some(off) = r.get("off").and_then(Value::as_str) {
            say(&format!("{host}'s relay is off: {off}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Row;

    fn slot(name: &str, live: bool) -> Slot {
        Slot {
            slot: name.into(),
            title: format!("{name} title"),
            busy: false,
            live,
        }
    }

    fn fleet() -> Fleet {
        let mut f = Fleet::default();
        f.slots.insert("a".into(), vec![slot("claude-1", true)]);
        f.slots.insert(
            "b".into(),
            vec![slot("claude-1", true), slot("claude-2", false)],
        );
        f.slots.insert("c".into(), vec![slot("claude-1", true)]);
        f.up.insert("a".into(), true);
        f.up.insert("b".into(), true);
        f.up.insert("c".into(), false);
        f
    }

    fn peer_msg(from: &str, to: &str, ty: &str) -> Value {
        json::parse(&format!(
            r#"{{"from":"{from}","to":"{to}","frame":{{"type":"{ty}","action":"peer_message_status","message":{{"content":"x"}}}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_frame_goes_to_a_live_slot_now_and_waits_for_one_that_is_not() {
        let f = fleet();
        match route(&f, "a", &peer_msg("claude-1", "b/claude-1", "user"), 5) {
            Route::Deliver(repo, item) => {
                assert_eq!(repo, "b");
                assert_eq!(item.from, "a/claude-1", "the hub's own say of who sent it");
                assert_eq!(item.to, "b/claude-1");
                assert_eq!(item.at, 5);
            }
            other => panic!("calibration: {other:?}"),
        }
        // Offloaded, or its container not answering: a message waits.
        assert!(matches!(
            route(&f, "a", &peer_msg("claude-1", "b/claude-2", "user"), 0),
            Route::Keep(_)
        ));
        assert!(matches!(
            route(&f, "a", &peer_msg("claude-1", "c/claude-1", "user"), 0),
            Route::Keep(_)
        ));
        // A control frame is about now.
        assert!(matches!(
            route(&f, "a", &peer_msg("claude-1", "b/claude-2", "control"), 0),
            Route::Drop(_)
        ));
        assert!(matches!(
            route(&f, "a", &peer_msg("claude-1", "b/claude-1", "control"), 0),
            Route::Deliver(..)
        ));
    }

    #[test]
    fn a_sender_must_be_a_live_slot_and_a_target_a_known_one_elsewhere() {
        let f = fleet();
        for (host, p, why) in [
            (
                "a",
                peer_msg("claude-9", "b/claude-1", "user"),
                "a/claude-9 is not a live slot",
            ),
            (
                "b",
                peer_msg("claude-2", "a/claude-1", "user"),
                "b/claude-2 is not a live slot",
            ),
            (
                "a",
                peer_msg("claude-1", "b/claude-9", "user"),
                "no slot b/claude-9",
            ),
            (
                "a",
                peer_msg("claude-1", "a/claude-1", "user"),
                "its own container",
            ),
            ("a", peer_msg("claude-1", "b", "user"), "is not an address"),
            (
                "a",
                peer_msg("claude-1", "../x/y", "user"),
                "is not an address",
            ),
            (
                "a",
                peer_msg("claude-1", "b/claude-1", "auth"),
                "not a message",
            ),
        ] {
            match route(&f, host, &p, 0) {
                Route::Drop(w) => assert!(w.contains(why), "{w:?} should say {why:?}"),
                other => panic!("{p:?} from {host}: {other:?}"),
            }
        }
    }

    #[test]
    fn each_agent_is_told_of_every_other_containers_slots() {
        let f = fleet();
        let names = |h: &str| -> Vec<(String, bool)> {
            f.roster_for(h)
                .into_iter()
                .map(|e| (e.addr(), e.live))
                .collect()
        };
        assert_eq!(
            names("a"),
            vec![
                ("b/claude-1".into(), true),
                ("b/claude-2".into(), false),
                ("c/claude-1".into(), false),
            ],
            "its own slots are not in it; a down container's are, not live"
        );
        assert_eq!(names("c").len(), 3);
    }

    #[test]
    fn a_containers_addresses_are_its_claude_slots_running_or_offloaded() {
        let row = |key: RowKey, f: fn(&mut Row)| {
            let mut r = Row {
                host: "a".into(),
                key,
                wants_you: false,
                busy: true,
                unread: false,
                attached: false,
                offloaded: false,
                closed: false,
                archived: false,
                title: "t".into(),
                age: String::new(),
                at: 0,
                nested: false,
                remote: false,
            };
            f(&mut r);
            r
        };
        let snap = Snapshot {
            header: crate::ui::Header {
                host: "a".into(),
                memory: None,
                load: None,
            },
            workspace: String::new(),
            rows: vec![
                row(RowKey::Slot("claude-1".into()), |_| {}),
                row(RowKey::Slot("claude-2".into()), |r| {
                    r.offloaded = true;
                    r.closed = true;
                }),
                row(RowKey::Slot("claude-3".into()), |r| r.closed = true),
                row(RowKey::Slot("claude-4".into()), |r| r.archived = true),
                row(RowKey::Slot("pi-1".into()), |_| {}),
                row(RowKey::Socket("other".into()), |_| {}),
            ],
        };
        let got: Vec<(String, bool)> = slots_of(&snap)
            .into_iter()
            .map(|s| (s.slot, s.live))
            .collect();
        assert_eq!(
            got,
            vec![("claude-1".into(), true), ("claude-2".into(), false)]
        );
    }

    #[test]
    fn the_beat_says_who_answers_and_what_waits() {
        let mut fleet = fleet();
        fleet.up.insert("d".into(), false);
        let mut queue = Queue::memory();
        queue
            .push(Item {
                to: "b/claude-2".into(),
                from: "a/claude-1".into(),
                frame: json::parse(r#"{"type":"user"}"#).unwrap(),
                at: 0,
            })
            .unwrap();
        let router = Router {
            fleet: Mutex::new(fleet),
            conns: Mutex::new(HashMap::new()),
            sent: Mutex::new(HashMap::new()),
            queue: Mutex::new(queue),
        };
        let line = router.pulse();
        assert!(line.ends_with(" 2/4 answering, 1 waiting\n"), "{line:?}");
        assert!(line.starts_with("20"), "a UTC date first: {line:?}");
    }

    #[test]
    fn the_queue_keeps_messages_across_a_restart_until_their_slot_takes_them() {
        let dir = std::env::temp_dir().join(format!("cs-router-q-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let item = |to: &str, at: u64| Item {
            to: to.into(),
            from: "a/claude-1".into(),
            frame: json::parse(r#"{"type":"user","message":{"content":"x"}}"#).unwrap(),
            at,
        };
        {
            let (mut q, skipped) = Queue::open(&dir).unwrap();
            assert!(skipped.is_empty());
            q.push(item("b/claude-2", 1)).unwrap();
            q.push(item("b/claude-3", 2)).unwrap();
            q.push(item("b/claude-2", 3)).unwrap();
        }
        fs::write(dir.join("00000000000000000099.json"), "not json").unwrap();
        let (mut q, skipped) = Queue::open(&dir).unwrap();
        assert_eq!(skipped, vec!["00000000000000000099.json".to_string()]);
        assert_eq!(q.len(), 3, "survived the restart");
        let got = q.take(|to| to == "b/claude-2");
        assert_eq!(
            got.iter().map(|i| i.at).collect::<Vec<_>>(),
            vec![1, 3],
            "oldest first"
        );
        assert_eq!(q.len(), 1);
        let (q2, _) = Queue::open(&dir).unwrap();
        assert_eq!(q2.len(), 1, "taken means gone from disk too");
        // One past the limit is refused; and age prunes.
        let mut m = Queue::memory();
        for i in 0..MAX_QUEUE {
            m.push(item("b/claude-2", i as u64)).unwrap();
        }
        assert!(m.push(item("b/claude-2", 0)).is_err());
        assert!(
            m.push(item("b/claude-3", 0)).is_ok(),
            "another slot's own limit"
        );
        // Sent at 0 to 9, and the other slot's at 0: eleven have waited too long.
        assert_eq!(m.prune(RETAIN_MS + 10).len(), 11);
        let _ = fs::remove_dir_all(&dir);
    }
}
