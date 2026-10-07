//! Activity, measured: whether a slot is doing anything, read from the kernel rather than
//! from Claude Code (design.md § *Activity, measured*). Measurement only, as of 0.4.6: every
//! offload pass says what this rule would do beside what the offloader did — including the
//! freeze it would hold from pass to pass, and what would thaw it — so the rule can be judged
//! on the fleet's own sessions before it decides anything.
//!
//! **Why the kernel.** The offloader's verdicts rest on reading Claude Code: hook payloads,
//! the task list's filters, which agents announce themselves. Eight review rounds of #10 kept
//! finding corners of that, and a self-update can move any of them. What a process *does* —
//! bytes it reads and writes, how often it wakes — means the same on every Claude Code
//! version and every machine.
//!
//! **What is counted.** Bytes through `read`/`write` — `rchar + wchar` in `/proc/<pid>/io`:
//! the terminal claude repaints, the transcripts it writes, pipes to its tools. The same count
//! on a Pi 4 and an x86 box, where CPU time is not. The kernel counts by call: `read`/`write`
//! on a socket count, but `send`/`recv` do not, and claude's own network uses those — so to
//! these the bytes of every TCP socket a slot's processes hold are added, from the kernel's
//! own per-socket count (`src/sockdiag.rs`). File I/O through a mapping stays invisible. CPU
//! time and wake-ups are logged beside the bytes for the data, not judged. The measured figures, the membership rule,
//! the line and what the rule cannot see are in design.md, which is the one record of them.
//!
//! **Which way it errs.** Anything not known counts as active: a slot's first reading, a
//! member that left since the last reading, a process that is there but cannot be read, a
//! window as long as the quiet period, an attachment `zmx` could not report. A process not
//! seen last time is counted whole. Concurrent passes may each write the state; the later
//! write wins and the other's window is simply measured again.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clock::Millis;
use crate::json::{self, Value};
use crate::procinfo::Proc;
use crate::registry::{SlotRecord, State};

/// The variable every process of a slot carries: zmx sets it to the session's name for the
/// program it runs, and so for everything that program starts — a slot this tool started and
/// one somebody attached by hand alike.
pub const VAR: &str = "ZMX_SESSION";

/// The shortest window a reading counts over. A pass run by hand straight after the timer's
/// would otherwise measure a few seconds, too short to say anything; it is left for the next.
pub const MIN_WINDOW_MS: Millis = 60_000;

/// How long a slot must go with no window over its budget — a minute's worth of bytes at its
/// line — before the rule would act: the offloader's own.
pub const QUIET_FOR_MS: Millis = crate::offload::IDLE_AFTER_STOP_MS;

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
    /// Voluntary context switches, summed over the process's live threads.
    pub wakeups: u64,
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
    /// Held by the recorded claude and no other member. One a child holds too is the child's,
    /// so its traffic would thaw a claude frozen alone, the safe way.
    pub claude: bool,
}

/// The recorded claude the rule would have frozen alone, and since when. A freeze belongs to
/// one process: a claude offloaded and resumed, or one that crashed, ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frozen {
    pub at: Millis,
    pub pid: u32,
    pub start: u64,
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
    pub frozen: Option<Frozen>,
}

/// Bytes one part of a slot moved in a window: through its processes' `read`/`write`, and
/// through the TCP sockets it holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moved {
    pub proc: u64,
    pub tcp: u64,
}

impl Moved {
    pub fn total(self) -> u64 {
        self.proc + self.tcp
    }
}

/// A window split between the recorded claude and everything else in the slot: what would
/// still run, and so could thaw it, were claude frozen alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Split {
    /// Claude's own bytes, with those of any process it started in the window.
    pub claude: Moved,
    pub rest: Moved,
    /// A process claude started in the window, by name: claude's doing, which a frozen claude
    /// could not have done, so never a thaw.
    pub claude_change: Option<String>,
    /// A member claude did not start in the window started, or a member other than claude
    /// exited — whoever started it.
    pub rest_change: Option<&'static str>,
    /// Members other than the recorded claude.
    pub others: usize,
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
    pub wakeups: Option<f64>,
    /// CPU milliseconds per second over the window (0 on a first reading) — logged, not
    /// judged: it scales with the device.
    pub cpu: f64,
    pub window_ms: Millis,
    pub floor: Option<f64>,
    pub line: f64,
    pub quiet_ms: Millis,
    pub procs: usize,
    /// Why there is no TCP figure, when there is none — and then `rate` is the process bytes
    /// alone, as 0.4.5 judged it.
    pub tcp: Result<Tcp, &'static str>,
    pub split: Split,
}

/// What a window may carry and still be quiet: a minute's worth of bytes at the line.
pub fn budget(line: f64) -> f64 {
    line * (MIN_WINDOW_MS as f64 / 1000.0)
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
    /// The slot's TCP sockets, or why they were not read.
    pub net: Result<Vec<Sock>, &'static str>,
    /// The recorded claude, while it is alive.
    pub claude: Option<(u32, u64)>,
    /// Members whose parent is the recorded claude, with their names: what only a running
    /// claude could have started.
    pub forked: Vec<(u32, String)>,
}

/// One pass's step for one slot, pure. The caller has already left a window shorter than
/// [`MIN_WINDOW_MS`] for the next pass.
///
/// **Whose a change is.** A member new since the last reading whose parent is the recorded
/// claude was started by claude: its bytes are claude's, and it is [`Split::claude_change`],
/// never a thaw — a stopped process starts nothing. A new member anything else started is the
/// rest's. A member that exited is the rest's whoever started it: `sleep N && gh run view`
/// ends with claude's own `bash` exiting, and that is the thaw working.
pub fn step(prev: Option<&SlotState>, now: Millis, snap: Snapshot) -> (SlotState, Measure) {
    let Snapshot {
        readings,
        left,
        net,
        claude,
        forked,
    } = snap;
    let is_claude = |pid: u32, start: u64| claude == Some((pid, start));
    let started_by_claude = |pid: u32| {
        forked
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, name)| name.clone())
    };
    let procs = readings.len();
    let mut split = Split {
        others: readings
            .iter()
            .filter(|r| !is_claude(r.pid, r.start))
            .count(),
        ..Split::default()
    };
    let Some(prev) = prev else {
        let m = Measure {
            rate: None,
            wakeups: None,
            cpu: 0.0,
            window_ms: 0,
            floor: None,
            line: line(None),
            quiet_ms: 0,
            procs,
            tcp: Err("first reading"),
            split,
        };
        let state = SlotState {
            at: now,
            procs: readings,
            last_active: now,
            minima: Vec::new(),
            sockets: net.unwrap_or_default(),
            frozen: None,
        };
        return (state, m);
    };
    let window_ms = now.saturating_sub(prev.at);
    let (mut wakeups, mut cpu, mut threads_gone) = (0u64, 0u64, false);
    for r in &readings {
        let seen = prev
            .procs
            .iter()
            .find(|p| p.pid == r.pid && p.start == r.start);
        let moved = match seen {
            Some(p) => {
                cpu += r.cpu.saturating_sub(p.cpu);
                // A thread that exited takes its count with it, so this sum can fall; a window
                // where it did has no honest figure.
                threads_gone |= r.wakeups < p.wakeups;
                wakeups += r.wakeups.saturating_sub(p.wakeups);
                r.bytes.saturating_sub(p.bytes)
            }
            // Not seen last time: everything it ever did falls in this window, as far as
            // anyone can tell. Counting it all errs towards active.
            None => {
                wakeups += r.wakeups;
                cpu += r.cpu;
                r.bytes
            }
        };
        if is_claude(r.pid, r.start) {
            split.claude.proc += moved;
        } else if let Some(name) = started_by_claude(r.pid).filter(|_| seen.is_none()) {
            split.claude.proc += moved;
            split.claude_change.get_or_insert(name);
        } else {
            split.rest.proc += moved;
            if seen.is_none() {
                split.rest_change = Some("a process started");
            }
        }
    }
    // A member seen last time and gone now: what it did since went to whoever reaped it — a
    // member's counters, or init's for an orphan — so this window's bytes are not known whole.
    // So does one gone before it was read; one of those claude started since the last
    // reading was claude's doing, like any other it started.
    let gone: Vec<&Reading> = prev
        .procs
        .iter()
        .filter(|p| {
            !readings
                .iter()
                .any(|r| r.pid == p.pid && r.start == p.start)
        })
        .collect();
    let vanished = !left.is_empty() || !gone.is_empty();
    for p in &gone {
        if !is_claude(p.pid, p.start) {
            split.rest_change = Some("a process exited");
        }
    }
    for &pid in &left {
        let new = !prev.procs.iter().any(|p| p.pid == pid);
        match started_by_claude(pid) {
            Some(name) if new => {
                split.claude_change.get_or_insert(name);
            }
            _ if claude.is_some_and(|(c, _)| c == pid) => {}
            _ => split.rest_change = Some("a process exited"),
        }
    }
    // Sockets the same way, with one difference: one closed since the last reading is not
    // counted at all, rather than making the window active. Its bytes since are lost with it,
    // and claude's connection pool closes idle sockets as a matter of course, so the rule that
    // keeps process churn active would keep every slot. A turn shows anyway, in the screen it
    // redraws and the transcript it appends.
    let (tcp, net) = match net {
        Ok(net) => {
            let mut bytes = 0u64;
            for s in &net {
                let moved = match prev.sockets.iter().find(|p| p.inode == s.inode) {
                    // An inode reused by a newer socket starts below the old one's count.
                    Some(p) if s.bytes >= p.bytes => s.bytes - p.bytes,
                    _ => s.bytes,
                };
                bytes += moved;
                if s.claude {
                    split.claude.tcp += moved;
                } else {
                    split.rest.tcp += moved;
                }
            }
            let closed = prev
                .sockets
                .iter()
                .filter(|p| !net.iter().any(|s| s.inode == p.inode))
                .count();
            let tcp = Tcp {
                rate: bytes as f64 / (window_ms as f64 / 1000.0),
                sockets: net.len(),
                closed,
            };
            (Ok(tcp), net)
        }
        // Not read this pass: the last reading stands, so the next window holds these bytes.
        Err(why) => (Err(why), prev.sockets.clone()),
    };
    let bytes = split.claude.total() + split.rest.total();
    let secs = window_ms as f64 / 1000.0;
    let rate = bytes as f64 / secs;
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
    // Active above a minute's worth of bytes at the line, not above the line on average: a turn
    // that starts in a window's last seconds would be averaged away. Every window is at least a
    // minute, so this covers the average too. A window as long as the quiet period cannot say
    // when in it the bytes fell, and one a member left is not known whole: both active.
    let budget = budget(line);
    let unknown = window_ms >= QUIET_FOR_MS || vanished;
    let last_active = if bytes as f64 > budget || unknown {
        now
    } else {
        prev.last_active
    };
    // Only a window known whole teaches the floor: one spanning an offload or a gap, or one a
    // member left, holds a rate that never happened — a resumed claude's lifetime bytes over
    // the whole offload would read as a quiet the slot never had.
    if !unknown {
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
        sockets: net,
        frozen: prev.frozen,
    };
    let m = Measure {
        rate: Some(rate),
        wakeups: (!threads_gone).then(|| wakeups as f64 / secs),
        // USER_HZ is 100 on every Linux, so a tick is 10 ms.
        cpu: cpu as f64 * 10.0 / secs,
        window_ms,
        floor,
        line,
        quiet_ms: now.saturating_sub(last_active),
        procs,
        tcp,
        split,
    };
    (state, m)
}

/// What the rule would do with a slot, given its window and whether a client is attached
/// (`None`: zmx could not say).
pub fn verdict(m: &Measure, attached: Option<bool>) -> String {
    match (m.rate, attached) {
        (None, _) => keep("first reading"),
        (_, Some(true)) => keep("attached"),
        (_, None) => keep("attachment unknown"),
        (Some(_), Some(false)) if m.quiet_ms >= QUIET_FOR_MS => {
            "the activity rule would freeze it".to_string()
        }
        (Some(_), Some(false)) => keep(&format!("quiet under {}m", QUIET_FOR_MS / 60_000)),
    }
}

fn keep(why: &str) -> String {
    format!("the activity rule would keep it: {why}")
}

/// The freeze the rule would make, carried from pass to pass: **claude alone is frozen**, and
/// everything else in the slot keeps running, so a tool waiting locally — `sleep N && gh run
/// view`, `tail -f`, a build — still does what it is waiting to do, and that is what thaws
/// claude: a member claude did not start starting, a member exiting, or the others moving more
/// than the window's budget. An attach thaws it too. Nothing is frozen yet; this says what
/// would have been.
///
/// **What claude itself did meanwhile** — started a process, or moved more than the budget —
/// is what a freeze would have stopped, a timer of its own above all, and the line says so.
/// When something thawed it in the same window, the line says both and claims no order:
/// claude's move may have followed the thaw. So `which the freeze would have stopped` appears
/// exactly when nothing would have thawed it, which is what makes it countable.
///
/// `claude` is the recorded claude while it is alive, and `parent` the name of its parent: a
/// claude whose parent is not zmx was started from a shell, whose job control would take the
/// terminal back from a stopped claude, so it is never frozen.
pub fn shadow(
    prev: Option<&Frozen>,
    m: &Measure,
    attached: Option<bool>,
    claude: Option<(u32, u64)>,
    parent: Option<&str>,
    now: Millis,
) -> (Option<Frozen>, String) {
    let prev = prev.filter(|f| claude == Some((f.pid, f.start)));
    if let Some(f) = prev {
        let budget = budget(m.line);
        let thaw = match attached {
            Some(true) => Some("attached".to_string()),
            None => Some("attachment unknown".to_string()),
            Some(false) => m.split.rest_change.map(str::to_string).or_else(|| {
                (m.split.rest.total() as f64 > budget)
                    .then(|| format!("its other processes moved {} B", m.split.rest.total()))
            }),
        };
        let own = own_doing(&m.split, budget);
        return match thaw {
            Some(why) => {
                let mut said = format!("the activity rule would thaw it: {why}");
                if let Some(own) = own {
                    said.push_str(&format!(
                        "; in the same window claude itself {own}, which may have followed the thaw"
                    ));
                }
                (None, said)
            }
            None => {
                let mut said = format!(
                    "the activity rule would have claude frozen, {}m so far",
                    now.saturating_sub(f.at) / 60_000
                );
                if let Some(own) = own {
                    said.push_str(&format!(
                        ", and claude itself {own}, which the freeze would have stopped"
                    ));
                }
                (Some(*f), said)
            }
        };
    }
    let said = verdict(m, attached);
    if m.rate.is_none() || m.quiet_ms < QUIET_FOR_MS || attached != Some(false) {
        return (None, said);
    }
    let Some((pid, start)) = claude else {
        return (None, keep("no recorded claude to freeze"));
    };
    if parent != Some("zmx") {
        return (
            None,
            keep(&format!(
                "claude's parent is {}, not zmx, and a shell's job control would take the terminal from a stopped claude",
                parent.unwrap_or("unknown")
            )),
        );
    }
    let said = match m.split.others {
        0 => said,
        n => format!("{said} — claude alone, leaving {n} other process(es) running"),
    };
    (
        Some(Frozen {
            at: now,
            pid,
            start,
        }),
        said,
    )
}

/// What claude itself did in a window, if anything a frozen claude could not have: started a
/// process, or moved more than the budget — with the TCP part said, since a turn moves bytes
/// over the network and housekeeping (a config re-read, the status line) mostly does not.
fn own_doing(split: &Split, budget: f64) -> Option<String> {
    let moved = (split.claude.total() as f64 > budget).then(|| {
        format!(
            "moved {} B ({} B of it tcp)",
            split.claude.total(),
            split.claude.tcp
        )
    });
    match (&split.claude_change, moved) {
        (Some(name), Some(moved)) => Some(format!("started {name} and {moved}")),
        (Some(name), None) => Some(format!("started {name}")),
        (None, moved) => moved,
    }
}

/// A pass's line for one slot: the window, then what the rule would do.
pub fn describe(slot: &str, m: &Measure, said: &str) -> String {
    let what = match m.rate {
        Some(r) => format!(
            "{:.0} B/s over {}s{}, {} wakeups/s, cpu {:.1} ms/s, {} process(es), line {:.0} B/s ({:.0} B a window){}, quiet {}m",
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
            m.wakeups.map_or("?".to_string(), |w| format!("{w:.1}")),
            m.cpu,
            m.procs,
            m.line,
            budget(m.line),
            m.floor
                .map(|f| format!(" from floor {f:.0}"))
                .unwrap_or_default(),
            m.quiet_ms / 60_000
        ),
        None => format!("{} process(es) read", m.procs),
    };
    format!("{slot}: measured — {what}; {said}")
}

/// One offload pass's measurement of every live slot: each slot's line, and the state stored
/// for the next pass. Holds no lock and writes nothing but its own file, so it changes nothing
/// the offloader decides.
pub fn pass(records: &[SlotRecord], table: Option<&[Proc]>, now: Millis) -> Vec<String> {
    let Some(table) = table else {
        return vec!["activity: /proc could not be listed; nothing measured".to_string()];
    };
    let env = by_slot(table);
    let sessions = crate::zmx::sessions();
    let prev = load();
    let mut lines = Vec::new();
    let tcp = if records.iter().any(|r| r.state == State::Live) {
        crate::sockdiag::tcp()
    } else {
        Ok(BTreeMap::new())
    };
    let tcp = tcp.map_err(|e| {
        lines.push(format!(
            "activity: tcp sockets could not be read ({e}); every slot's bytes are read/write alone, as 0.4.5 judged them"
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
        if rec.state != State::Live {
            continue;
        }
        // A reading from the future — the clock stepped back, or a pass that stored while this
        // one was reading zmx — is no reading; its floor is still the slot's.
        let old = stored.filter(|o| o.at <= now);
        if let Some(o) = old.filter(|o| now.saturating_sub(o.at) < MIN_WINDOW_MS) {
            lines.push(format!(
                "{}: measured — {}s since the last reading, too short; the next pass counts it",
                rec.slot,
                now.saturating_sub(o.at) / 1000
            ));
            continue;
        }
        let root = rec.pid.zip(rec.proc_start);
        let claude =
            root.filter(|&(pid, start)| table.iter().any(|p| p.pid == pid && p.start == start));
        let pids = members(table, env.get(&rec.slot), root);
        let snap = match snapshot(table, &pids, claude, tcp.as_ref().map_err(|e| *e)) {
            Ok(snap) => snap,
            Err(unreadable) => {
                let thaw = if stored.is_some_and(|o| o.frozen.is_some()) {
                    ", and thaw it"
                } else {
                    ""
                };
                lines.push(format!(
                    "{}: measured — {unreadable} of its processes could not be read; the activity rule would keep it: unknown{thaw}",
                    rec.slot
                ));
                // The span measured again next pass, and counted as active: unknown is.
                if let Some(o) = old {
                    next.insert(
                        rec.slot.clone(),
                        SlotState {
                            last_active: now,
                            frozen: None,
                            ..o.clone()
                        },
                    );
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
        let attached = sessions.as_ref().and_then(|all| {
            all.iter()
                .find(|s| s.name == rec.slot && s.answered)
                .map(|s| s.attached)
        });
        let parent = claude.and_then(|(pid, _)| {
            let ppid = table.iter().find(|p| p.pid == pid)?.ppid;
            Some(table.iter().find(|p| p.pid == ppid)?.comm.as_str())
        });
        // Carried as the floor is, past a reading from the future: the freeze is the slot's
        // until something thaws it.
        let (frozen, said) = shadow(
            stored.and_then(|o| o.frozen.as_ref()),
            &m,
            attached,
            claude,
            parent,
            now,
        );
        state.frozen = frozen;
        lines.push(describe(&rec.slot, &m, &said));
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
    lines
}

/// Read one slot's members: their counters, the TCP sockets they hold, which of them claude
/// started, and which had gone before they could be read. `Err` is how many are there but
/// cannot be read — unknown, which the caller counts as active.
fn snapshot(
    table: &[Proc],
    pids: &[u32],
    claude: Option<(u32, u64)>,
    tcp: Result<&BTreeMap<u64, u64>, &'static str>,
) -> Result<Snapshot, usize> {
    let (mut readings, mut left, mut unreadable) = (Vec::new(), Vec::new(), 0usize);
    let gone = |pid: u32| !std::path::Path::new(&format!("/proc/{pid}")).exists();
    // Each socket once however many members share it, and claude's only if every holder is
    // claude: one a child holds too is the child's, so its traffic would thaw a claude frozen
    // alone, the safe way.
    let mut held: BTreeMap<u64, bool> = BTreeMap::new();
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
        let mine = claude == Some((r.pid, r.start));
        readings.push(r);
        if net_err.is_some() {
            continue;
        }
        match crate::sockdiag::held(pid) {
            Some(inodes) => {
                for i in inodes {
                    held.entry(i).and_modify(|c| *c &= mine).or_insert(mine);
                }
            }
            // Gone between its counters and its descriptors: a member that left, as above.
            None if gone(pid) => left.push(pid),
            None => net_err = Some("a member's descriptors could not be listed"),
        }
    }
    if unreadable > 0 {
        return Err(unreadable);
    }
    let net = match net_err {
        Some(why) => Err(why),
        None => tcp.map(|all| {
            held.into_iter()
                // Not a TCP socket: a Unix or UDP one, which carries no count.
                .filter_map(|(inode, claude)| {
                    all.get(&inode).map(|&bytes| Sock {
                        inode,
                        bytes,
                        claude,
                    })
                })
                .collect()
        }),
    };
    let forked = claude.map_or_else(Vec::new, |(c, _)| {
        table
            .iter()
            .filter(|p| p.ppid == c && pids.contains(&p.pid))
            .map(|p| (p.pid, p.comm.clone()))
            .collect()
    });
    Ok(Snapshot {
        readings,
        left,
        net,
        claude,
        forked,
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

/// A slot's members: those carrying its variable — if the recorded claude is known, only
/// those younger than it — and the recorded claude, only if it is still the process that was
/// recorded, by start time, with its descendants. Not this process and not a running
/// `claude-sessions`: a menu or a pass in the slot's shell would count its own reading of
/// `/proc`. Not `zmx` either: a daemon a slot's tool started keeps the slot's `ZMX_SESSION`,
/// and its reads are the other session's terminal, not this slot's work. (One that has exited is already in its parent's counters — the kernel folds a
/// reaped child's bytes in — which is why the status line raises every floor a little.)
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
    let mut wakeups = 0u64;
    if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for t in tasks.flatten() {
            if let Ok(status) = std::fs::read_to_string(t.path().join("status")) {
                wakeups += status
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("voluntary_ctxt_switches:")?
                            .trim()
                            .parse::<u64>()
                            .ok()
                    })
                    .unwrap_or(0);
            }
        }
    }
    Some(Reading {
        pid,
        start,
        bytes,
        wakeups,
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
                            Value::num(r.wakeups as f64),
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
                        Value::Arr(vec![
                            Value::num(k.inode as f64),
                            Value::num(k.bytes as f64),
                            Value::num(u8::from(k.claude)),
                        ])
                    })
                    .collect(),
            ),
        );
        if let Some(f) = s.frozen {
            v.set(
                "frozen",
                Value::Arr(vec![
                    Value::num(f.at as f64),
                    Value::num(f.pid),
                    Value::num(f.start as f64),
                ]),
            );
        }
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
                [pid, start, bytes, wakeups, cpu] => Some(Reading {
                    pid: pid as u32,
                    start: start as u64,
                    bytes: bytes as u64,
                    wakeups: wakeups as u64,
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
                [inode, bytes, claude] => Some(Sock {
                    inode: inode as u64,
                    bytes: bytes as u64,
                    claude: claude != 0.0,
                }),
                _ => None,
            })
            .collect();
        let frozen = s.get("frozen").and_then(Value::as_arr).and_then(|a| {
            match a.iter().filter_map(Value::as_u64).collect::<Vec<_>>()[..] {
                [at, pid, start] => Some(Frozen {
                    at,
                    pid: u32::try_from(pid).ok()?,
                    start,
                }),
                _ => None,
            }
        });
        out.insert(
            slot.clone(),
            SlotState {
                at,
                procs,
                last_active,
                minima,
                sockets,
                frozen,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading with sockets read and none held, and no claude recorded: every member is one
    /// of the others. `left` adds a member gone before it was read.
    fn plain(readings: Vec<Reading>, left: bool) -> Snapshot {
        Snapshot {
            readings,
            left: if left { vec![99] } else { vec![] },
            net: Ok(vec![]),
            claude: None,
            forked: vec![],
        }
    }

    /// A reading with claude at pid 1, these sockets, and nothing claude started.
    fn with_claude(readings: Vec<Reading>, net: Result<Vec<Sock>, &'static str>) -> Snapshot {
        Snapshot {
            readings,
            left: vec![],
            net,
            claude: CLAUDE,
            forked: vec![],
        }
    }

    fn r(pid: u32, start: u64, bytes: u64) -> Reading {
        Reading {
            pid,
            start,
            bytes,
            wakeups: bytes / 100,
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
        assert!(verdict(&m, Some(false)).contains("would keep"));
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
    fn wakeups_that_fell_have_no_figure() {
        let s = step(
            None,
            T0,
            plain(
                vec![Reading {
                    pid: 1,
                    start: 7,
                    bytes: 0,
                    wakeups: 500,
                    cpu: 0,
                }],
                false,
            ),
        )
        .0;
        let (_, m) = step(
            Some(&s),
            T0 + 100_000,
            plain(
                vec![Reading {
                    pid: 1,
                    start: 7,
                    bytes: 0,
                    wakeups: 400,
                    cpu: 0,
                }],
                false,
            ),
        );
        assert_eq!(m.wakeups, None);
        assert!(describe("claude-1", &m, &verdict(&m, Some(false))).contains("? wakeups/s"));
        let (_, m) = step(
            Some(&s),
            T0 + 100_000,
            plain(
                vec![Reading {
                    pid: 1,
                    start: 7,
                    bytes: 0,
                    wakeups: 600,
                    cpu: 0,
                }],
                false,
            ),
        );
        assert_eq!(m.wakeups, Some(1.0));
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
        // both directions: the same rule calls the quiet slot freezable and the busy one not.
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
        assert!(
            verdict(&m, Some(false)).contains("would freeze"),
            "{}",
            verdict(&m, Some(false))
        );
        assert!(verdict(&m, Some(true)).contains("would keep"), "attached");
        assert!(
            verdict(&m, None).contains("would keep"),
            "attachment unknown"
        );
        at += 180_000;
        bytes += 180 * 15_000;
        let (_, m) = step(Some(&s), at, plain(vec![r(1, 7, bytes)], false));
        assert_eq!(m.quiet_ms, 0);
        assert!(verdict(&m, Some(false)).contains("quiet under 10m"));
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
                sockets: vec![sock(4_000_000_123, 987_654, true), sock(9, 0, false)],
                frozen: Some(Frozen {
                    at: T0 - 60_000,
                    pid: 10,
                    start: 77,
                }),
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
        // A 0.4.5 file, with neither key: read whole, with no sockets and no freeze.
        let old = from_json(
            &json::parse(
                r#"{"claude-1":{"at":5,"last_active":4,"procs":[[1,7,0,0,0]],"minima":[[1000,87.5]]}}"#,
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

    fn sock(inode: u64, bytes: u64, claude: bool) -> Sock {
        Sock {
            inode,
            bytes,
            claude,
        }
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
            frozen: None,
        }
    }

    const CLAUDE: Option<(u32, u64)> = Some((1, 7));

    fn moved(proc: u64, tcp: u64) -> Moved {
        Moved { proc, tcp }
    }

    #[test]
    fn socket_bytes_count_as_the_slots_and_are_split_by_holder() {
        // 70 KB through claude's socket by send/recv, which no process counter shows: over the
        // budget, so the window is active. Calibration: the same window with the socket idle
        // is quiet.
        let s = quiet_pair(vec![sock(50, 1_000, true), sock(60, 0, false)]);
        let quiet = || vec![r(1, 7, 18_000), r(2, 8, 0)];
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            with_claude(
                quiet(),
                Ok(vec![sock(50, 71_000, true), sock(60, 0, false)]),
            ),
        );
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        assert_eq!(
            (m.split.claude, m.split.rest),
            (moved(18_000, 70_000), moved(0, 0))
        );
        assert_eq!(m.tcp.as_ref().map(|t| t.rate.round()), Ok(389.0));
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            with_claude(quiet(), Ok(vec![sock(50, 1_000, true), sock(60, 0, false)])),
        );
        assert!(m.quiet_ms > QUIET_FOR_MS, "calibration: {m:?}");
        // The child's socket is the child's: what would still run with claude frozen.
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            with_claude(
                quiet(),
                Ok(vec![sock(50, 1_000, true), sock(60, 5_000, false)]),
            ),
        );
        assert_eq!(
            (m.split.claude, m.split.rest),
            (moved(18_000, 0), moved(0, 5_000))
        );
    }

    #[test]
    fn a_closed_socket_is_uncounted_and_an_unread_one_waits_for_the_next_window() {
        let s = quiet_pair(vec![sock(50, 1_000, true), sock(51, 900_000, true)]);
        let quiet = || vec![r(1, 7, 18_000), r(2, 8, 0)];
        // Socket 51 closed: its last bytes are lost, and the window stays quiet — claude's
        // connection pool closing an idle socket is not a turn. A new socket counts whole, and
        // one whose inode was reused by a newer socket starts over.
        let (n, m) = step(
            Some(&s),
            T0 + 180_000,
            with_claude(
                quiet(),
                Ok(vec![sock(50, 500, true), sock(52, 2_000, true)]),
            ),
        );
        assert_eq!(m.split.claude, moved(18_000, 2_500), "{m:?}");
        assert_eq!(m.tcp.as_ref().map(|t| (t.sockets, t.closed)), Ok((2, 1)));
        assert!(m.quiet_ms > QUIET_FOR_MS, "{m:?}");
        assert!(
            describe("claude-1", &m, "x").contains("(tcp 14 B/s, 2 socket(s), 1 closed uncounted)")
        );
        // Not read: the bytes are the processes' alone, the line says why, and the last
        // sockets stand, so the next window that reads them holds what moved meanwhile.
        let (n, m) = step(
            Some(&n),
            T0 + 360_000,
            with_claude(quiet(), Err("the dump was refused")),
        );
        assert_eq!(m.split.claude, moved(0, 0), "{m:?}");
        assert!(describe("claude-1", &m, "x").contains("(tcp ?: the dump was refused)"));
        let (_, m) = step(
            Some(&n),
            T0 + 540_000,
            with_claude(
                quiet(),
                Ok(vec![sock(50, 500, true), sock(52, 72_000, true)]),
            ),
        );
        assert_eq!(m.split.claude, moved(0, 70_000));
        assert_eq!(m.quiet_ms, 0, "{m:?}");
    }

    #[test]
    fn a_member_gone_before_its_sockets_were_listed_leaves_the_tcp_figure_whole() {
        // The race itself — a member exiting between its counters and its descriptors — is not
        // unit-testable; `snapshot` reports it as a member that left, and this is what `step`
        // makes of that: a thaw-worthy exit and an active window, with the TCP figure intact.
        let s = quiet_pair(vec![sock(50, 1_000, true)]);
        let snap = Snapshot {
            left: vec![2],
            ..with_claude(
                vec![r(1, 7, 18_000), r(2, 8, 0)],
                Ok(vec![sock(50, 1_000, true)]),
            )
        };
        let (_, m) = step(Some(&s), T0 + 180_000, snap);
        assert!(m.tcp.is_ok(), "{m:?}");
        assert_eq!(m.split.rest_change, Some("a process exited"));
        assert_eq!(m.quiet_ms, 0);
    }

    /// The would-be freeze from a quiet, detached window of a claude with one child.
    fn frozen_pair() -> (SlotState, Frozen) {
        let s = quiet_pair(vec![]);
        let (n, m) = step(
            Some(&s),
            T0 + 180_000,
            with_claude(vec![r(1, 7, 18_000), r(2, 8, 0)], Ok(vec![])),
        );
        let (f, said) = shadow(None, &m, Some(false), CLAUDE, Some("zmx"), T0 + 180_000);
        assert_eq!(
            said,
            "the activity rule would freeze it — claude alone, leaving 1 other process(es) running"
        );
        (n, f.expect("frozen"))
    }

    /// One pass three minutes after [`frozen_pair`]'s freeze.
    fn after_freeze(snap: Snapshot, attached: Option<bool>) -> (Option<Frozen>, String) {
        let (s, f) = frozen_pair();
        let at = T0 + 360_000;
        let (_, m) = step(Some(&s), at, snap);
        shadow(Some(&f), &m, attached, CLAUDE, Some("zmx"), at)
    }

    #[test]
    fn claude_is_frozen_alone_only_when_quiet_detached_and_under_zmx() {
        let (_, f) = frozen_pair();
        assert_eq!((f.pid, f.start, f.at), (1, 7, T0 + 180_000));
        let s = quiet_pair(vec![]);
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            with_claude(vec![r(1, 7, 18_000), r(2, 8, 0)], Ok(vec![])),
        );
        let refused = |attached, claude, parent| shadow(None, &m, attached, claude, parent, T0);
        let (f, said) = refused(Some(false), CLAUDE, Some("fish"));
        assert!(
            f.is_none() && said.contains("parent is fish, not zmx"),
            "{said}"
        );
        let (f, said) = refused(Some(false), None, Some("zmx"));
        assert!(f.is_none() && said.contains("no recorded claude"), "{said}");
        let (f, said) = refused(Some(true), CLAUDE, Some("zmx"));
        assert!(f.is_none() && said.ends_with("keep it: attached"), "{said}");
        // A slot with claude alone in it says so as 0.4.5 did.
        let (_, m) = step(
            Some(&SlotState {
                procs: vec![r(1, 7, 0)],
                ..quiet_pair(vec![])
            }),
            T0 + 180_000,
            with_claude(vec![r(1, 7, 18_000)], Ok(vec![])),
        );
        let (f, said) = shadow(None, &m, Some(false), CLAUDE, Some("zmx"), T0);
        assert!(f.is_some());
        assert_eq!(said, "the activity rule would freeze it");
    }

    #[test]
    fn what_still_runs_thaws_a_frozen_claude() {
        let (_, f) = frozen_pair();
        let pass = |readings, net, attached| after_freeze(with_claude(readings, Ok(net)), attached);
        // Calibration: nothing changed, so it stays frozen.
        let (still, said) = pass(vec![r(1, 7, 36_000), r(2, 8, 0)], vec![], Some(false));
        assert_eq!(still, Some(f));
        assert_eq!(
            said,
            "the activity rule would have claude frozen, 3m so far"
        );
        let thawed = |(f, said): (Option<Frozen>, String), why: &str| {
            assert!(f.is_none(), "{said}");
            assert_eq!(said, format!("the activity rule would thaw it: {why}"));
        };
        // `sleep 600 && gh run view`: the sleep exits.
        thawed(
            pass(vec![r(1, 7, 36_000)], vec![], Some(false)),
            "a process exited",
        );
        // ... and gh starts, under the bash claude started before the freeze. (The same
        // readings with gh as claude's own child are claude's doing, not a thaw: the
        // calibration half of `a_process_claude_itself_started_is_claudes_not_a_thaw`.)
        thawed(
            pass(
                vec![r(1, 7, 36_000), r(2, 8, 0), r(3, 9, 0)],
                vec![],
                Some(false),
            ),
            "a process started",
        );
        // `tail -f` on a log that gets a line, or a child's own network.
        thawed(
            pass(vec![r(1, 7, 36_000), r(2, 8, 70_000)], vec![], Some(false)),
            "its other processes moved 70000 B",
        );
        thawed(
            pass(
                vec![r(1, 7, 36_000), r(2, 8, 0)],
                vec![sock(60, 70_000, false)],
                Some(false),
            ),
            "its other processes moved 70000 B",
        );
        thawed(
            pass(vec![r(1, 7, 36_000), r(2, 8, 0)], vec![], Some(true)),
            "attached",
        );
        thawed(
            pass(vec![r(1, 7, 36_000), r(2, 8, 0)], vec![], None),
            "attachment unknown",
        );
    }

    #[test]
    fn a_process_claude_itself_started_is_claudes_not_a_thaw() {
        // Claude's timer fired and it ran `bash`: a stopped claude starts nothing, so this is
        // what the freeze would have stopped, and its bytes are claude's.
        let (_, f) = frozen_pair();
        let snap = Snapshot {
            forked: vec![(3, "bash".to_string())],
            ..with_claude(
                vec![r(1, 7, 36_000), r(2, 8, 0), r(3, 9, 1_000)],
                Ok(vec![]),
            )
        };
        let (still, said) = after_freeze(snap.clone(), Some(false));
        assert_eq!(still, Some(f));
        assert_eq!(
            said,
            "the activity rule would have claude frozen, 3m so far, and claude itself started bash, which the freeze would have stopped"
        );
        let (s, _) = frozen_pair();
        let (_, m) = step(Some(&s), T0 + 360_000, snap);
        assert_eq!(
            (m.split.claude, m.split.rest),
            (moved(19_000, 0), moved(0, 0))
        );
        // One claude started that was gone before it was read is claude's too.
        let snap = Snapshot {
            left: vec![3],
            forked: vec![(3, "git".to_string())],
            ..with_claude(vec![r(1, 7, 36_000), r(2, 8, 0)], Ok(vec![]))
        };
        let (still, said) = after_freeze(snap, Some(false));
        assert!(
            still.is_some() && said.contains("claude itself started git"),
            "{said}"
        );
    }

    #[test]
    fn a_thaw_and_claudes_own_move_in_one_window_are_both_said() {
        // The sleep exited and claude moved: which came first is unknowable in a window, so the
        // line claims no order — and never says "would have stopped", which is kept for the
        // windows where nothing thawed it.
        for attached in [Some(false), Some(true)] {
            let (f, said) = after_freeze(
                with_claude(vec![r(1, 7, 18_000 + 540_000)], Ok(vec![])),
                attached,
            );
            assert!(f.is_none(), "{said}");
            let why = if attached == Some(true) {
                "attached"
            } else {
                "a process exited"
            };
            assert_eq!(
                said,
                format!(
                    "the activity rule would thaw it: {why}; in the same window claude itself moved 540000 B (0 B of it tcp), which may have followed the thaw"
                )
            );
        }
    }

    #[test]
    fn claude_moving_while_frozen_is_said_and_another_claude_ends_the_freeze() {
        let (s, f) = frozen_pair();
        let at = T0 + 360_000;
        // Its own timer fired: claude moved, its child did not. A freeze would have stopped
        // that, and nothing in the rule would have thawed it — the line says so, with the TCP
        // part, which is what tells a turn from housekeeping.
        let (_, m) = step(
            Some(&s),
            at,
            with_claude(
                vec![r(1, 7, 18_000 + 500_000), r(2, 8, 0)],
                Ok(vec![sock(50, 40_000, true)]),
            ),
        );
        let (still, said) = shadow(Some(&f), &m, Some(false), CLAUDE, Some("zmx"), at);
        assert_eq!(still, Some(f));
        assert_eq!(
            said,
            "the activity rule would have claude frozen, 3m so far, and claude itself moved 540000 B (40000 B of it tcp), which the freeze would have stopped"
        );
        // Resumed after an offload: a different claude, so the old freeze is gone, and the new
        // one is judged afresh — here, attached.
        let (gone, said) = shadow(Some(&f), &m, Some(true), Some((5, 70)), Some("zmx"), at);
        assert_eq!(gone, None);
        assert!(said.ends_with("keep it: attached"), "{said}");
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
