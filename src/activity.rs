//! Activity, measured: whether a slot is doing anything, read from the kernel rather than
//! from Claude Code (design.md § *Activity, measured*). The offloader stops a slot only once
//! this has read it quiet for [`QUIET_FOR_MS`].
//!
//! **Why the kernel.** What a process *does* — bytes it reads and writes, CPU it uses — means
//! the same on every Claude Code version and every machine; what Claude Code says in its hooks
//! moves with each self-update.
//!
//! **What is counted.** Bytes through `read`/`write` — `rchar + wchar` in `/proc/<pid>/io`: the
//! terminal claude repaints, the transcripts it writes, pipes to its tools. The kernel counts by
//! call: `read`/`write` on a socket count, but `send`/`recv` do not, and claude's own network
//! uses those — so to these the bytes of every TCP socket a slot's processes hold are added,
//! from the kernel's own per-socket count (`src/sockdiag.rs`). CPU time is judged beside the
//! bytes against a fixed line. File I/O through a mapping stays invisible, and so does a
//! process waiting without moving either: that is what `claude-sessions keepalive` is for.
//!
//! **Which way it errs.** Anything not known counts as active: a slot's first reading, a
//! member that left since the last reading, a process that is there but cannot be read, a
//! window as long as the quiet period. A process not seen last time is counted whole.
//! Concurrent passes may each write the state; the later write wins and the other's window is
//! simply measured again.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clock::Millis;
use crate::json::{self, Value};
use crate::procinfo::Proc;
use crate::registry::SlotRecord;

/// The variable every process of a slot carries: zmx sets it to the session's name for the
/// program it runs, and so for everything that program starts — a slot this tool started and
/// one somebody attached by hand alike.
pub const VAR: &str = "ZMX_SESSION";

/// The shortest window a reading counts over. A pass run by hand straight after the timer's
/// would otherwise measure a few seconds, too short to say anything; it is left for the next.
pub const MIN_WINDOW_MS: Millis = 60_000;

/// How long a slot must go with no window over its budget before the offloader stops it
/// (owner, 2026-10-01).
pub const QUIET_FOR_MS: Millis = 10 * 60 * 1000;

/// CPU milliseconds per second a slot may use and still be quiet: about eight times an idle
/// claude's on the fleet (6 ms/s on a Pi, design.md), and well under any turn.
pub const CPU_LINE: f64 = 50.0;

const FLOOR_FACTOR: f64 = 10.0;
const LINE_MIN: f64 = 512.0;
const LINE_MAX: f64 = 4096.0;
const FLOOR_HOURS: u64 = 24;
const HOUR_MS: Millis = 60 * 60 * 1000;

/// One process's counters at one moment. `start` tells a reused pid from the process that
/// had it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub pid: u32,
    pub start: u64,
    /// `rchar + wchar`: every byte through `read` and `write` — files, the terminal, pipes.
    /// Sockets only through `read`/`write`: `send`/`recv` bypass these counters, so claude's
    /// own network traffic is not in them (design.md § Activity, measured).
    pub bytes: u64,
    /// CPU time in clock ticks, its own and its reaped children's (`utime + stime + cutime +
    /// cstime`).
    pub cpu: u64,
}

/// One TCP socket a slot's processes hold, with its bytes both ways from the kernel's own
/// count (`src/sockdiag.rs`): what `send`/`recv` move, which [`Reading::bytes`] cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sock {
    pub inode: u64,
    pub bytes: u64,
}

/// What one slot's state carries from pass to pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotState {
    pub at: Millis,
    pub procs: Vec<Reading>,
    pub last_active: Millis,
    /// The quietest rate seen in each hour of the last day, as `(hour, bytes per second)`.
    pub minima: Vec<(u64, f64)>,
    pub sockets: Vec<Sock>,
}

/// The window's TCP figures. Their bytes are already in [`Measure::rate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Tcp {
    pub rate: f64,
    pub sockets: usize,
    /// Sockets held last time and closed since: their last bytes are not counted (design.md).
    pub closed: usize,
}

/// One slot's window, as a pass reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Measure {
    /// Bytes per second over the window; `None` on a slot's first reading.
    pub rate: Option<f64>,
    /// CPU milliseconds over the window (0 on a first reading).
    pub cpu_ms: f64,
    /// Why the window counts as active whatever it measured, when it does.
    pub unknown: Option<&'static str>,
    pub window_ms: Millis,
    pub floor: Option<f64>,
    pub line: f64,
    pub quiet_ms: Millis,
    pub procs: usize,
    /// Why there is no TCP figure, when there is none — and then `rate` is the process bytes
    /// alone.
    pub tcp: Result<Tcp, &'static str>,
}

/// What a window may carry and still be quiet: a minute's worth of bytes at the line.
pub fn budget(line: f64) -> f64 {
    line * (MIN_WINDOW_MS as f64 / 1000.0)
}

/// The CPU milliseconds a window may use and still be quiet: a minute's worth at [`CPU_LINE`].
pub fn cpu_budget() -> f64 {
    CPU_LINE * (MIN_WINDOW_MS as f64 / 1000.0)
}

/// The line a slot's rate is held against, from its floor.
pub fn line(floor: Option<f64>) -> f64 {
    floor.map_or(LINE_MIN, |f| (f * FLOOR_FACTOR).clamp(LINE_MIN, LINE_MAX))
}

/// What one pass read of one slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub readings: Vec<Reading>,
    /// Members in the pass's `/proc` snapshot that were gone before they could be read.
    pub left: Vec<u32>,
    /// The TCP sockets the slot's members hold, or why they were not read.
    pub net: Result<Vec<Sock>, &'static str>,
}

/// The window's TCP figures and the sockets to remember.
///
/// A socket closed since the last reading is not counted at all, rather than making the window
/// active as a member that left does. Its bytes since are lost with it, and claude's
/// connection pool closes idle sockets as a matter of course, so that rule would keep every
/// slot; a turn shows anyway, in the screen it redraws and the transcript it appends.
fn tcp_window(
    prev: &[Sock],
    net: Result<Vec<Sock>, &'static str>,
    window_ms: Millis,
) -> (u64, Result<Tcp, &'static str>, Vec<Sock>) {
    let net = match net {
        Ok(net) => net,
        // Not read this pass: the last reading stands, so the next window holds these bytes.
        Err(why) => return (0, Err(why), prev.to_vec()),
    };
    let bytes: u64 = net
        .iter()
        .map(|s| match prev.iter().find(|p| p.inode == s.inode) {
            // An inode reused by a newer socket starts below the old one's count.
            Some(p) if s.bytes >= p.bytes => s.bytes - p.bytes,
            _ => s.bytes,
        })
        .sum();
    let closed = prev
        .iter()
        .filter(|p| !net.iter().any(|s| s.inode == p.inode))
        .count();
    let tcp = Tcp {
        rate: bytes as f64 / (window_ms as f64 / 1000.0),
        sockets: net.len(),
        closed,
    };
    (bytes, Ok(tcp), net)
}

/// One pass's step for one slot, pure. The caller has already left a window shorter than
/// [`MIN_WINDOW_MS`] for the next pass.
pub fn step(prev: Option<&SlotState>, now: Millis, snap: Snapshot) -> (SlotState, Measure) {
    let Snapshot {
        readings,
        left,
        net,
    } = snap;
    let procs = readings.len();
    let Some(prev) = prev else {
        let m = Measure {
            rate: None,
            cpu_ms: 0.0,
            unknown: None,
            window_ms: 0,
            floor: None,
            line: line(None),
            quiet_ms: 0,
            procs,
            tcp: Err("first reading"),
        };
        let state = SlotState {
            at: now,
            procs: readings,
            last_active: now,
            minima: Vec::new(),
            sockets: net.unwrap_or_default(),
        };
        return (state, m);
    };
    let window_ms = now.saturating_sub(prev.at);
    let (mut bytes, mut cpu) = (0u64, 0u64);
    for r in &readings {
        match prev
            .procs
            .iter()
            .find(|p| p.pid == r.pid && p.start == r.start)
        {
            Some(p) => {
                bytes += r.bytes.saturating_sub(p.bytes);
                cpu += r.cpu.saturating_sub(p.cpu);
            }
            // Not seen last time: everything it ever did falls in this window, as far as
            // anyone can tell. Counting it all errs towards active.
            None => {
                bytes += r.bytes;
                cpu += r.cpu;
            }
        }
    }
    // A member seen last time and gone now: what it did since went to whoever reaped it — a
    // member's counters, or init's for an orphan — so this window is not known whole. So is
    // one gone before it was read.
    let vanished = !left.is_empty()
        || prev.procs.iter().any(|p| {
            !readings
                .iter()
                .any(|r| r.pid == p.pid && r.start == p.start)
        });
    let (tcp_bytes, tcp, sockets) = tcp_window(&prev.sockets, net, window_ms);
    bytes += tcp_bytes;
    let secs = window_ms as f64 / 1000.0;
    let rate = bytes as f64 / secs;
    // USER_HZ is 100 on every Linux, so a tick is 10 ms.
    let cpu_ms = cpu as f64 * 10.0;
    // The line comes from the history *before* this window: a window never sets its own bar,
    // so a slot with none is held to the low line, as unknown should be.
    let hour = now / HOUR_MS;
    let mut minima: Vec<(u64, f64)> = prev
        .minima
        .iter()
        .copied()
        .filter(|&(h, _)| h + FLOOR_HOURS > hour)
        .collect();
    let floor = minima.iter().map(|&(_, m)| m).reduce(f64::min);
    let line = line(floor);
    // Active above a minute's worth at the line, not above the line on average: a turn that
    // starts in a window's last seconds would be averaged away. Every window is at least a
    // minute, so this covers the average too. A window as long as the quiet period cannot say
    // when in it anything fell, and one a member left is not known whole: both active.
    let unknown = if window_ms >= QUIET_FOR_MS {
        Some("a window as long as the quiet period")
    } else if vanished {
        Some("a process left")
    } else {
        None
    };
    let busy = bytes as f64 > budget(line) || cpu_ms > cpu_budget();
    let last_active = if busy || unknown.is_some() {
        now
    } else {
        prev.last_active
    };
    // Only a window known whole teaches the floor: one spanning an offload or a gap, or one a
    // member left, holds a rate that never happened.
    if unknown.is_none() {
        match minima.last_mut() {
            Some((h, m)) if *h == hour => *m = m.min(rate),
            _ => minima.push((hour, rate)),
        }
    }
    let state = SlotState {
        at: now,
        procs: readings,
        last_active,
        minima,
        sockets,
    };
    let m = Measure {
        rate: Some(rate),
        cpu_ms,
        unknown,
        window_ms,
        floor,
        line,
        quiet_ms: now.saturating_sub(last_active),
        procs,
        tcp,
    };
    (state, m)
}

/// A slot with members there but unreadable: its line, and the state to store. Unknown counts
/// as active, so the span is measured again next pass from the last reading.
pub fn unknown(
    slot: &str,
    unreadable: usize,
    stored: Option<&SlotState>,
    now: Millis,
) -> (String, Option<SlotState>) {
    let line = format!(
        "{slot}: measured — {unreadable} of its processes could not be read; active: unknown"
    );
    let state = stored.map(|o| SlotState {
        last_active: now,
        ..o.clone()
    });
    (line, state)
}

/// A pass's line for one slot.
pub fn describe(slot: &str, m: &Measure) -> String {
    let what = match m.rate {
        Some(r) => format!(
            "{:.0} B/s over {}s{}, cpu {:.0} ms ({:.0} ms a window), {} process(es), line {:.0} B/s ({:.0} B a window){}{}, quiet {}m",
            r,
            m.window_ms / 1000,
            match &m.tcp {
                Ok(t) => format!(
                    " (tcp {:.0} B/s, {} socket(s){})",
                    t.rate,
                    t.sockets,
                    match t.closed {
                        0 => String::new(),
                        n => format!(", {n} closed uncounted"),
                    }
                ),
                Err(why) => format!(" (tcp ?: {why})"),
            },
            m.cpu_ms,
            cpu_budget(),
            m.procs,
            m.line,
            budget(m.line),
            m.floor
                .map(|f| format!(" from floor {f:.0}"))
                .unwrap_or_default(),
            m.unknown
                .map(|why| format!(", active: {why}"))
                .unwrap_or_default(),
            m.quiet_ms / 60_000
        ),
        None => format!("{} process(es) read, a first reading", m.procs),
    };
    format!("{slot}: measured — {what}")
}

/// One offload pass's measurement of every running slot: each slot's line, and the states this
/// pass read, for it to decide on. Everything known is also stored, for the menu ([`load`]).
/// Holds no lock and writes nothing but its own file.
pub fn pass(
    records: &[SlotRecord],
    table: Option<&[Proc]>,
    now: Millis,
) -> (Vec<String>, BTreeMap<String, SlotState>) {
    let mut read_now = BTreeMap::new();
    let Some(table) = table else {
        let line = "activity: /proc could not be listed; nothing measured".to_string();
        return (vec![line], read_now);
    };
    let env = by_slot(table);
    let prev = load();
    let mut lines = Vec::new();
    let tcp = if records.iter().any(|r| r.state.is_running()) {
        crate::sockdiag::tcp()
    } else {
        Ok(BTreeMap::new())
    };
    let tcp = tcp.map_err(|e| {
        lines.push(format!(
            "activity: tcp sockets could not be read ({e}); every slot's bytes are read/write alone"
        ));
        "the dump was refused"
    });
    let mut next = BTreeMap::new();
    for rec in records {
        let stored = prev.get(&rec.slot);
        // The state belongs to the slot's name for as long as it has a record, and is carried
        // as it was unless a reading below replaces it: an offloaded slot, a crashed one whose
        // processes are gone, a pass too soon after the last. A resume under the same name
        // must not relearn its floor from its first, busy windows.
        if let Some(o) = stored {
            next.insert(rec.slot.clone(), o.clone());
        }
        if !rec.state.is_running() {
            continue;
        }
        // A reading from the future — the clock stepped back, or a pass that stored while this
        // one was reading — is no reading; its floor is still the slot's.
        let old = stored.filter(|o| o.at <= now);
        if let Some(o) = old.filter(|o| now.saturating_sub(o.at) < MIN_WINDOW_MS) {
            lines.push(format!(
                "{}: measured — {}s since the last reading, too short; the next pass counts it",
                rec.slot,
                now.saturating_sub(o.at) / 1000
            ));
            continue;
        }
        let pids = members(table, env.get(&rec.slot), rec.pid.zip(rec.proc_start));
        let snap = match snapshot(&pids, tcp.as_ref().map_err(|e| *e)) {
            Ok(snap) => snap,
            Err(unreadable) => {
                let (line, state) = unknown(&rec.slot, unreadable, stored, now);
                lines.push(line);
                // Stored for the menu, but not a reading this pass took: the pass keeps the slot.
                if let Some(state) = state {
                    next.insert(rec.slot.clone(), state);
                }
                continue;
            }
        };
        if snap.readings.is_empty() {
            continue;
        }
        let (mut state, m) = step(old, now, snap);
        if old.is_none() {
            if let Some(o) = stored {
                state.minima = o.minima.clone();
            }
        }
        lines.push(describe(&rec.slot, &m));
        read_now.insert(rec.slot.clone(), state.clone());
        next.insert(rec.slot.clone(), state);
    }
    if let Err(e) = store(&next) {
        // The last saved state stays, floors and all: counters are cumulative, so the next
        // pass's window from it holds every byte of the one that could not save — and a longer
        // window is held to the same budget, so it errs active.
        lines.push(format!(
            "activity: state not saved ({e}); the next pass measures from the last saved reading"
        ));
    }
    (lines, read_now)
}

/// Read one slot's members: their counters, the TCP sockets they hold, and which had gone
/// before they could be read. `Err` is how many are there but cannot be read — unknown, which
/// the caller counts as active.
fn snapshot(
    pids: &[u32],
    tcp: Result<&BTreeMap<u64, u64>, &'static str>,
) -> Result<Snapshot, usize> {
    let (mut readings, mut left, mut unreadable) = (Vec::new(), Vec::new(), 0usize);
    let gone = |pid: u32| !std::path::Path::new(&format!("/proc/{pid}")).exists();
    let mut held: Vec<u64> = Vec::new();
    let mut net_err = tcp.err();
    for &pid in pids {
        let Some(r) = read(pid) else {
            // Gone since the snapshot: its bytes went to whoever reaped it, perhaps after that
            // one was read — a member that left. There but unreadable is unknown.
            if gone(pid) {
                left.push(pid);
            } else {
                unreadable += 1;
            }
            continue;
        };
        readings.push(r);
        if net_err.is_some() {
            continue;
        }
        match crate::sockdiag::held(pid) {
            Some(inodes) => held.extend(inodes),
            // Gone between its counters and its descriptors: a member that left, as above.
            None if gone(pid) => left.push(pid),
            None => net_err = Some("a member's descriptors could not be listed"),
        }
    }
    if unreadable > 0 {
        return Err(unreadable);
    }
    held.sort_unstable();
    held.dedup();
    let net = match net_err {
        Some(why) => Err(why),
        // Not a TCP socket: a Unix or UDP one, which carries no count.
        None => tcp.map(|all| {
            held.into_iter()
                .filter_map(|inode| all.get(&inode).map(|&bytes| Sock { inode, bytes }))
                .collect()
        }),
    };
    Ok(Snapshot {
        readings,
        left,
        net,
    })
}

/// The slot a process belongs to, by [`VAR`] in its environment, if it carries one.
fn slot_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    raw.split(|b| *b == 0).find_map(|kv| {
        let v = kv.strip_prefix(VAR.as_bytes())?.strip_prefix(b"=")?;
        Some(String::from_utf8_lossy(v).to_string())
    })
}

/// Every process of each slot in a snapshot, by the environment.
pub fn by_slot(table: &[Proc]) -> BTreeMap<String, Vec<u32>> {
    let mut out: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for p in table {
        if let Some(slot) = slot_of(p.pid) {
            out.entry(slot).or_default().push(p.pid);
        }
    }
    out
}

/// A slot's members: those carrying its variable — if the recorded claude is known, only those
/// younger than it — and the recorded claude, only if it is still the process that was
/// recorded, by start time, with its descendants. Not this process and not a running
/// `claude-sessions`: a menu or a pass in the slot's shell would count its own reading of
/// `/proc`. Not `zmx` either: a daemon a slot's tool started keeps the slot's `ZMX_SESSION`,
/// and its reads are the other session's terminal, not this slot's work. (One that has exited
/// is already in its parent's counters — the kernel folds a reaped child's bytes in — which is
/// why the status line raises every floor a little.)
pub fn members(table: &[Proc], env: Option<&Vec<u32>>, claude: Option<(u32, u64)>) -> Vec<u32> {
    let mut pids: Vec<u32> = env.cloned().unwrap_or_default();
    let root =
        claude.filter(|&(pid, start)| table.iter().any(|p| p.pid == pid && p.start == start));
    if let Some((root, start)) = root {
        // Everything the current claude started is younger than it — strictly: claude boots for
        // longer than a clock tick before it starts anything, while whatever ran it may share
        // its tick. An older process carrying the name is an earlier claude's orphan, in a slot
        // name since reused, or whatever started this one, and not this slot's work.
        pids.retain(|&pid| table.iter().any(|p| p.pid == pid && p.start > start));
        pids.push(root);
        pids.extend(
            crate::procinfo::descendants(table, root)
                .iter()
                .map(|p| p.pid),
        );
    }
    let me = std::process::id();
    pids.retain(|&pid| {
        pid != me
            && !table
                .iter()
                .any(|p| p.pid == pid && (p.comm == "claude-sessions" || p.comm == "zmx"))
    });
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// One process's counters now, or `None` if it has gone or cannot be read.
pub fn read(pid: u32) -> Option<Reading> {
    let start = crate::procinfo::start_time(pid)?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After `comm`, field 3 is the first; utime..cstime are fields 14–17.
    let cpu: u64 = stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .skip(11)
        .take(4)
        .filter_map(|f| f.parse::<u64>().ok())
        .sum();
    let io = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let field = |k: &str| {
        io.lines().find_map(|l| {
            l.strip_prefix(k)?
                .strip_prefix(':')?
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    let bytes = field("rchar")?.saturating_add(field("wchar")?);
    Some(Reading {
        pid,
        start,
        bytes,
        cpu,
    })
}

/// Where the state lives, beside the registry. Not `.json`: the registry reads every
/// `*.json` there as a slot.
pub fn state_path() -> PathBuf {
    crate::registry::dir().join("activity.state")
}

pub fn load() -> BTreeMap<String, SlotState> {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| json::parse(&s).ok())
        .map(|v| from_json(&v))
        .unwrap_or_default()
}

/// Written whole and renamed into place, so a reader never sees half of it.
pub fn store(all: &BTreeMap<String, SlotState>) -> std::io::Result<()> {
    let path = state_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(".activity.{}.tmp", std::process::id()));
    let done = std::fs::write(&tmp, json::to_string_pretty(&to_json(all)))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

fn to_json(all: &BTreeMap<String, SlotState>) -> Value {
    let mut o = Value::obj();
    for (slot, s) in all {
        let mut v = Value::obj();
        v.set("at", Value::num(s.at as f64));
        v.set("last_active", Value::num(s.last_active as f64));
        v.set(
            "procs",
            Value::Arr(
                s.procs
                    .iter()
                    .map(|r| {
                        Value::Arr(vec![
                            Value::num(r.pid),
                            Value::num(r.start as f64),
                            Value::num(r.bytes as f64),
                            Value::num(r.cpu as f64),
                        ])
                    })
                    .collect(),
            ),
        );
        v.set(
            "minima",
            Value::Arr(
                s.minima
                    .iter()
                    .map(|&(h, m)| Value::Arr(vec![Value::num(h as f64), Value::num(m)]))
                    .collect(),
            ),
        );
        v.set(
            "sockets",
            Value::Arr(
                s.sockets
                    .iter()
                    .map(|k| {
                        Value::Arr(vec![Value::num(k.inode as f64), Value::num(k.bytes as f64)])
                    })
                    .collect(),
            ),
        );
        o.set(slot, v);
    }
    o
}

fn from_json(v: &Value) -> BTreeMap<String, SlotState> {
    let mut out = BTreeMap::new();
    let Value::Obj(slots) = v else {
        return out;
    };
    for (slot, s) in slots {
        let num = |k: &str| s.get(k).and_then(Value::as_u64);
        let (Some(at), Some(last_active)) = (num("at"), num("last_active")) else {
            continue;
        };
        let rows = |k: &str| -> Vec<Vec<f64>> {
            s.get(k)
                .and_then(Value::as_arr)
                .unwrap_or_default()
                .iter()
                .filter_map(|r| {
                    r.as_arr()
                        .map(|a| a.iter().filter_map(Value::as_f64).collect())
                })
                .collect()
        };
        let procs = rows("procs")
            .into_iter()
            .filter_map(|r| match r[..] {
                [pid, start, bytes, cpu] => Some(Reading {
                    pid: pid as u32,
                    start: start as u64,
                    bytes: bytes as u64,
                    cpu: cpu as u64,
                }),
                _ => None,
            })
            .collect();
        let minima = rows("minima")
            .into_iter()
            .filter_map(|r| match r[..] {
                [h, m] => Some((h as u64, m)),
                _ => None,
            })
            .collect();
        let sockets = rows("sockets")
            .into_iter()
            .filter_map(|r| match r[..] {
                [inode, bytes] => Some(Sock {
                    inode: inode as u64,
                    bytes: bytes as u64,
                }),
                _ => None,
            })
            .collect();
        out.insert(
            slot.clone(),
            SlotState {
                at,
                procs,
                last_active,
                minima,
                sockets,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading with sockets read and none held. `left` adds a member gone before it was read.
    fn plain(readings: Vec<Reading>, left: bool) -> Snapshot {
        Snapshot {
            readings,
            left: if left { vec![99] } else { vec![] },
            net: Ok(vec![]),
        }
    }

    /// A reading holding these sockets.
    fn with_net(readings: Vec<Reading>, net: Result<Vec<Sock>, &'static str>) -> Snapshot {
        Snapshot {
            readings,
            left: vec![],
            net,
        }
    }

    fn r(pid: u32, start: u64, bytes: u64) -> Reading {
        Reading {
            pid,
            start,
            bytes,
            cpu: 0,
        }
    }

    const T0: Millis = 1_000 * HOUR_MS;

    #[test]
    fn the_line_follows_the_floor_between_its_bounds() {
        assert_eq!(
            line(None),
            LINE_MIN,
            "no floor yet: the low line, so more counts as active"
        );
        assert_eq!(line(Some(100.0)), 1000.0);
        assert_eq!(line(Some(5.0)), LINE_MIN, "a near-silent floor");
        assert_eq!(line(Some(50_000.0)), LINE_MAX, "a slot only ever seen busy");
    }

    #[test]
    fn a_first_reading_is_active() {
        let (s, m) = step(None, T0, plain(vec![r(1, 7, 5_000)], false));
        assert_eq!((m.rate, m.quiet_ms, s.last_active), (None, 0, T0));
    }

    #[test]
    fn a_window_is_held_to_the_line_from_before_it() {
        // A slot with no history: its first window at 2 KB/s is held to the low line and is
        // active — it must not set its own bar at ten times itself. Calibration: a slot whose
        // floor of 100 was learned earlier holds 54 KB over three minutes under its 60 KB budget and
        // calls it quiet.
        let s = step(None, T0, plain(vec![r(1, 7, 0)], false)).0;
        let (_, m) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, 360_000)], false));
        assert_eq!((m.floor, m.line, m.quiet_ms), (None, LINE_MIN, 0), "{m:?}");
        let learned = SlotState {
            minima: vec![(T0 / HOUR_MS, 100.0)],
            ..s
        };
        let (_, m) = step(
            Some(&learned),
            T0 + 180_000,
            plain(vec![r(1, 7, 54_000)], false),
        );
        assert_eq!((m.floor, m.line), (Some(100.0), 1000.0));
        assert_eq!(m.quiet_ms, 180_000);
    }

    #[test]
    fn a_turn_in_a_windows_last_seconds_is_not_averaged_away() {
        // Quiet since T0 at a floor of 100, then 15 s of streaming at the end of a 3-minute
        // window: 870 B/s on average, under the line of 1000, but far more than a minute's
        // worth at it. Calibration: the same window idle throughout is quiet.
        let s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0 - 10 * 60_000,
            minima: vec![(T0 / HOUR_MS, 100.0)],
            ..Default::default()
        };
        let burst = 165 * 100 + 15 * 9_342;
        let (_, m) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, burst)], false));
        assert!(m.rate.unwrap() < m.line, "{m:?}");
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            plain(vec![r(1, 7, 180 * 100)], false),
        );
        assert!(m.quiet_ms > QUIET_FOR_MS, "{m:?}");
    }

    #[test]
    fn a_window_as_long_as_the_quiet_period_counts_as_active() {
        // Two hours with few bytes, under even a minute's budget: more likely a clock step or a
        // stalled timer than two hours of quiet, and when in it anything fell is unknown — so
        // active. Calibration: the same bytes over three minutes are quiet.
        let s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0,
            minima: vec![(T0 / HOUR_MS, 100.0)],
            ..Default::default()
        };
        let (_, m) = step(
            Some(&s),
            T0 + 2 * HOUR_MS,
            plain(vec![r(1, 7, 36_000)], false),
        );
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, 36_000)], false));
        assert_eq!(m.quiet_ms, 180_000, "{m:?}");
    }

    #[test]
    fn a_window_a_member_left_counts_as_active() {
        // A child seen last pass and gone now took its last bytes to whoever reaped it — init,
        // for an orphan — so the window is not known whole. Calibration: the same quiet window
        // with the child still there is quiet.
        let s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0), r(2, 8, 0)],
            last_active: T0,
            minima: vec![(T0 / HOUR_MS, 100.0)],
            ..Default::default()
        };
        let (_, m) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, 18_000)], false));
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            plain(vec![r(1, 7, 18_000), r(2, 8, 0)], false),
        );
        assert_eq!(m.quiet_ms, 180_000, "{m:?}");
    }

    #[test]
    fn a_member_gone_before_it_was_read_counts_as_active() {
        let s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0,
            minima: vec![(T0 / HOUR_MS, 100.0)],
            ..Default::default()
        };
        let (_, m) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, 18_000)], true));
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, 18_000)], false));
        assert_eq!(m.quiet_ms, 180_000, "calibration: {m:?}");
    }

    #[test]
    fn a_window_across_an_offload_does_not_teach_the_floor() {
        // The slot's state from before an offload, ten hours later, with a resumed claude: every
        // member is new, so the window is active — and its rate, a new claude's bytes over ten
        // hours, never happened, so the floor keeps what it had. Calibration: a whole window of
        // the same claude does teach it.
        let s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0,
            minima: vec![(T0 / HOUR_MS, 250.0)],
            ..Default::default()
        };
        let (n, m) = step(
            Some(&s),
            T0 + 10 * HOUR_MS,
            plain(vec![r(9, 99, 360_000)], false),
        );
        assert_eq!(m.quiet_ms, 0);
        assert_eq!(n.minima, vec![(T0 / HOUR_MS, 250.0)], "{:?}", n.minima);
        let (n, _) = step(Some(&s), T0 + 180_000, plain(vec![r(1, 7, 18_000)], false));
        assert_eq!(n.minima, vec![(T0 / HOUR_MS, 100.0)]);
        // Each half of "not known whole" on its own: the same claude over two hours, and a
        // member that left a three-minute window. Either would teach a floor under 250.
        let (n, _) = step(
            Some(&s),
            T0 + 2 * HOUR_MS,
            plain(vec![r(1, 7, 36_000)], false),
        );
        assert_eq!(
            n.minima,
            vec![(T0 / HOUR_MS, 250.0)],
            "long window: {:?}",
            n.minima
        );
        let two = SlotState {
            procs: vec![r(1, 7, 0), r(2, 8, 0)],
            ..s
        };
        let (n, _) = step(Some(&two), T0 + 180_000, plain(vec![r(1, 7, 6_000)], false));
        assert_eq!(
            n.minima,
            vec![(T0 / HOUR_MS, 250.0)],
            "member left: {:?}",
            n.minima
        );
    }

    #[test]
    fn a_slot_with_a_member_it_cannot_read_is_active() {
        let stored = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0 - 11 * 60_000,
            ..SlotState::default()
        };
        let now = T0 + 180_000;
        let quiet = |s: &SlotState| crate::offload::quiet_for(Some(s), (1, 7), now);
        assert_eq!(quiet(&stored), Some(11 * 60_000), "calibration");
        let (line, state) = unknown("claude-1", 1, Some(&stored), now);
        assert!(line.ends_with("active: unknown"), "{line}");
        let state = state.unwrap();
        assert_eq!(quiet(&state), Some(0));
        assert_eq!(
            state.at, T0,
            "the next window still starts at the last reading"
        );
    }

    #[test]
    fn members_are_the_recorded_claude_by_start_time_and_never_this_tool() {
        let me = std::process::id();
        let proc = |pid, ppid, comm: &str, start| Proc {
            pid,
            ppid,
            comm: comm.into(),
            start,
            state: 'S',
            args: vec![],
        };
        let table = vec![
            proc(100, 1, "claude", 7),
            proc(101, 100, "bash", 9),
            proc(102, 100, "claude-sessions", 9),
            proc(me, 100, "cs", 9),
            proc(103, 1, "zmx", 10),
        ];
        assert_eq!(members(&table, None, Some((100, 7))), vec![100, 101]);
        assert_eq!(
            members(&table, None, Some((100, 8))),
            Vec::<u32>::new(),
            "pid 100 was reused"
        );
        assert_eq!(
            members(&table, Some(&vec![102, me, 101, 103]), None),
            vec![101]
        );
        // An orphan of an earlier claude in this slot name, older than the current one.
        let mut table = table;
        table.push(proc(50, 1, "sleep", 3));
        table.push(proc(150, 1, "cargo", 12));
        table.push(proc(160, 1, "sh", 7)); // whatever started claude, in its own tick
        assert_eq!(
            members(&table, Some(&vec![50, 150, 160]), Some((100, 7))),
            vec![100, 101, 150]
        );
        assert_eq!(
            members(&table, Some(&vec![50, 150]), None),
            vec![50, 150],
            "no claude to compare"
        );
    }

    #[test]
    fn idle_bytes_go_quiet_and_a_turn_wakes_it() {
        // 100 B/s for ten minutes in 3-minute passes, then a turn at 15 KB/s. Calibration in
        // both directions: the same rule calls the quiet slot quiet and the busy one not.
        let mut s = step(None, T0, plain(vec![r(1, 7, 0)], false)).0;
        let mut bytes = 0;
        let mut at = T0;
        let mut last = None;
        for _ in 0..5 {
            at += 180_000;
            bytes += 18_000;
            let (n, m) = step(Some(&s), at, plain(vec![r(1, 7, bytes)], false));
            s = n;
            last = Some(m);
        }
        let m = last.unwrap();
        assert_eq!(m.floor, Some(100.0));
        assert_eq!(m.line, 1000.0);
        assert!(m.quiet_ms >= QUIET_FOR_MS, "{m:?}");
        at += 180_000;
        bytes += 180 * 15_000;
        let (_, m) = step(Some(&s), at, plain(vec![r(1, 7, bytes)], false));
        assert_eq!(m.quiet_ms, 0);
    }

    #[test]
    fn a_new_process_counts_whole_and_a_reused_pid_is_a_new_process() {
        let s = step(None, T0, plain(vec![r(1, 7, 1_000)], false)).0;
        // pid 1 again but started later: a different process, all of its bytes counted; and
        // a child seen for the first time, all of its own.
        let (_, m) = step(
            Some(&s),
            T0 + 100_000,
            plain(vec![r(1, 9, 2_000), r(2, 8, 3_000)], false),
        );
        assert_eq!(m.rate, Some(50.0));
        // The same process: only what it did since.
        let (_, m) = step(Some(&s), T0 + 100_000, plain(vec![r(1, 7, 1_500)], false));
        assert_eq!(m.rate, Some(5.0));
    }

    #[test]
    fn the_floor_is_the_last_days_quietest_hour_and_forgets_older_ones() {
        let mut s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0,
            minima: vec![(T0 / HOUR_MS - 30, 1.0)],
            ..Default::default()
        };
        s.minima.push((T0 / HOUR_MS - 2, 80.0));
        let (n, m) = step(Some(&s), T0 + 100_000, plain(vec![r(1, 7, 20_000)], false));
        assert_eq!(
            m.floor,
            Some(80.0),
            "the 30-hour-old minimum has gone: {:?}",
            n.minima
        );
        assert_eq!(n.minima.len(), 2);
    }

    #[test]
    fn the_state_survives_its_file() {
        let mut all = BTreeMap::new();
        all.insert(
            "claude-1".to_string(),
            SlotState {
                at: T0,
                procs: vec![r(10, 77, 123_456_789)],
                last_active: T0 - 5,
                minima: vec![(1000, 87.5)],
                sockets: vec![sock(4_000_000_123, 987_654), sock(9, 0)],
            },
        );
        all.insert(
            "claude-2".to_string(),
            SlotState {
                at: T0,
                procs: vec![r(11, 78, 5)],
                last_active: T0,
                minima: vec![],
                ..Default::default()
            },
        );
        let back = from_json(&json::parse(&json::to_string_pretty(&to_json(&all))).unwrap());
        assert_eq!(back, all);
        // A file with no sockets key: read whole, with none.
        let old = from_json(
            &json::parse(
                r#"{"claude-1":{"at":5,"last_active":4,"procs":[[1,7,0,0]],"minima":[[1000,87.5]]}}"#,
            )
            .unwrap(),
        );
        assert_eq!(
            old["claude-1"],
            SlotState {
                at: 5,
                procs: vec![r(1, 7, 0)],
                last_active: 4,
                minima: vec![(1000, 87.5)],
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_slots_processes_are_found_by_their_environment_however_reparented() {
        // A slot name no other test uses, a process carrying it, one that double-forked away
        // from its parent to init, and one without it. Calibration: the last is not found.
        let slot = format!("claude-9{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("cs-activity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("orphan");
        let mut direct = std::process::Command::new("sleep")
            .arg("30")
            .env(VAR, &slot)
            .spawn()
            .unwrap();
        let st = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}", pidfile.display()))
            .env(VAR, &slot)
            .status()
            .unwrap();
        assert!(st.success());
        let orphan: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let table = crate::procinfo::table().unwrap();
        let found = by_slot(&table).remove(&slot).unwrap_or_default();
        let _ = std::process::Command::new("kill")
            .args(["-9", &orphan.to_string()])
            .status();
        let _ = direct.kill();
        let _ = other.kill();
        let _ = direct.wait();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found.contains(&direct.id()), "{found:?}");
        assert!(found.contains(&orphan), "the double-forked one: {found:?}");
        assert!(
            !found.contains(&other.id()),
            "calibration: no variable, not found"
        );
    }

    fn sock(inode: u64, bytes: u64) -> Sock {
        Sock { inode, bytes }
    }

    /// Claude at pid 1 and a child at pid 2, read three minutes ago, quiet for eleven minutes
    /// at a floor of 100: a minute's budget at the line of 1,000 is 60,000 B.
    fn quiet_pair(sockets: Vec<Sock>) -> SlotState {
        SlotState {
            at: T0,
            procs: vec![r(1, 7, 0), r(2, 8, 0)],
            last_active: T0 - 11 * 60_000,
            minima: vec![(T0 / HOUR_MS, 100.0)],
            sockets,
        }
    }

    #[test]
    fn socket_bytes_count_as_the_slots() {
        // 70 KB through a socket by send/recv, which no process counter shows: over the budget,
        // so the window is active. Calibration: the same window with the socket idle is quiet.
        let s = quiet_pair(vec![sock(50, 1_000), sock(60, 0)]);
        let quiet = || vec![r(1, 7, 18_000), r(2, 8, 0)];
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            with_net(quiet(), Ok(vec![sock(50, 71_000), sock(60, 0)])),
        );
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        assert_eq!(m.tcp.as_ref().map(|t| t.rate.round()), Ok(389.0));
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            with_net(quiet(), Ok(vec![sock(50, 1_000), sock(60, 0)])),
        );
        assert!(m.quiet_ms > QUIET_FOR_MS, "calibration: {m:?}");
    }

    #[test]
    fn a_closed_socket_is_uncounted_and_an_unread_one_waits_for_the_next_window() {
        let s = quiet_pair(vec![sock(50, 1_000), sock(51, 900_000)]);
        let quiet = || vec![r(1, 7, 18_000), r(2, 8, 0)];
        // Socket 51 closed: its last bytes are lost, and the window stays quiet — claude's
        // connection pool closing an idle socket is not a turn. A new socket counts whole, and
        // one whose inode was reused by a newer socket starts over.
        let (n, m) = step(
            Some(&s),
            T0 + 180_000,
            with_net(quiet(), Ok(vec![sock(50, 500), sock(52, 2_000)])),
        );
        assert_eq!(m.tcp.as_ref().map(|t| (t.sockets, t.closed)), Ok((2, 1)));
        assert_eq!(m.rate.map(f64::round), Some(114.0), "{m:?}");
        assert!(m.quiet_ms > QUIET_FOR_MS, "{m:?}");
        assert!(describe("claude-1", &m).contains("(tcp 14 B/s, 2 socket(s), 1 closed uncounted)"));
        // Not read: the bytes are the processes' alone, the line says why, and the last
        // sockets stand, so the next window that reads them holds what moved meanwhile.
        let (n, m) = step(
            Some(&n),
            T0 + 360_000,
            with_net(quiet(), Err("the dump was refused")),
        );
        assert_eq!(m.rate, Some(0.0), "{m:?}");
        assert!(describe("claude-1", &m).contains("(tcp ?: the dump was refused)"));
        let (_, m) = step(
            Some(&n),
            T0 + 540_000,
            with_net(quiet(), Ok(vec![sock(50, 500), sock(52, 72_000)])),
        );
        assert_eq!(m.quiet_ms, 0, "{m:?}");
    }

    #[test]
    fn a_member_gone_before_its_sockets_were_listed_leaves_the_tcp_figure_whole() {
        // The race itself — a member exiting between its counters and its descriptors — is not
        // unit-testable; `snapshot` reports it as a member that left, and this is what `step`
        // makes of that: an active window, with the TCP figure intact.
        let s = quiet_pair(vec![sock(50, 1_000)]);
        let snap = Snapshot {
            left: vec![2],
            ..with_net(vec![r(1, 7, 18_000), r(2, 8, 0)], Ok(vec![sock(50, 1_000)]))
        };
        let (_, m) = step(Some(&s), T0 + 180_000, snap);
        assert!(m.tcp.is_ok(), "{m:?}");
        assert_eq!(m.quiet_ms, 0);
    }

    #[test]
    fn cpu_over_its_line_is_active_without_a_byte() {
        // Three minutes of computing that writes nothing: 6 s of CPU, 33 ms/s on average but
        // more than a minute's worth at the 50 ms/s line. Calibration: an idle claude's 6 ms/s.
        let s = quiet_pair(vec![]);
        let cpu = |ticks| Reading {
            cpu: ticks,
            ..r(1, 7, 0)
        };
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            plain(vec![cpu(600), r(2, 8, 0)], false),
        );
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        assert!(describe("claude-1", &m).contains("cpu 6000 ms (3000 ms a window)"));
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            plain(vec![cpu(108), r(2, 8, 0)], false),
        );
        assert!(m.quiet_ms > QUIET_FOR_MS, "calibration: {m:?}");
    }

    #[test]
    fn bytes_through_a_process_are_counted() {
        let me = std::process::id();
        let a = read(me).unwrap();
        // Written and read back: either half of the count missing falls short.
        let path = std::env::temp_dir().join(format!("cs-activity-w-{me}"));
        std::fs::write(&path, vec![0u8; 100_000]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 100_000);
        let b = read(me).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(b.bytes >= a.bytes + 200_000, "{a:?} {b:?}");
        assert_eq!(a.start, b.start);
    }
}
