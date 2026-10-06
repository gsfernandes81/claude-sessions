//! Activity, measured: whether a slot is doing anything, read from the kernel rather than
//! from Claude Code (design.md § *Activity, measured*). Measurement only, as of 0.4.5: every
//! offload pass says what this rule would do beside what the offloader did, so the rule can
//! be judged on the fleet's own sessions before it decides anything.
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
//! on a socket count, but `send`/`recv` do not, and claude's own network uses those — so it
//! is invisible to these counters, as is file I/O through a mapping. CPU time and wake-ups are
//! logged beside the bytes for the data, not judged. The measured figures, the membership rule,
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

/// What one slot's state carries from pass to pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotState {
    pub at: Millis,
    pub procs: Vec<Reading>,
    pub last_active: Millis,
    /// The quietest rate seen in each hour of the last day, as `(hour, bytes per second)`.
    pub minima: Vec<(u64, f64)>,
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
}

/// What a window may carry and still be quiet: a minute's worth of bytes at the line.
pub fn budget(line: f64) -> f64 {
    line * (MIN_WINDOW_MS as f64 / 1000.0)
}

/// The line a slot's rate is held against, from its floor.
pub fn line(floor: Option<f64>) -> f64 {
    floor.map_or(LINE_MIN, |f| (f * FLOOR_FACTOR).clamp(LINE_MIN, LINE_MAX))
}

/// One pass's step for one slot, pure. The caller has already left a window shorter than
/// [`MIN_WINDOW_MS`] for the next pass. `left` says a member in this pass's snapshot was gone
/// by the time it was read.
pub fn step(
    prev: Option<&SlotState>,
    now: Millis,
    readings: Vec<Reading>,
    left: bool,
) -> (SlotState, Measure) {
    let procs = readings.len();
    let Some(prev) = prev else {
        let state = SlotState {
            at: now,
            procs: readings,
            last_active: now,
            minima: Vec::new(),
        };
        let m = Measure {
            rate: None,
            wakeups: None,
            cpu: 0.0,
            window_ms: 0,
            floor: None,
            line: line(None),
            quiet_ms: 0,
            procs,
        };
        return (state, m);
    };
    let window_ms = now.saturating_sub(prev.at);
    let (mut bytes, mut wakeups, mut cpu, mut threads_gone) = (0u64, 0u64, 0u64, false);
    for r in &readings {
        match prev
            .procs
            .iter()
            .find(|p| p.pid == r.pid && p.start == r.start)
        {
            Some(p) => {
                bytes += r.bytes.saturating_sub(p.bytes);
                cpu += r.cpu.saturating_sub(p.cpu);
                // A thread that exited takes its count with it, so this sum can fall; a window
                // where it did has no honest figure.
                threads_gone |= r.wakeups < p.wakeups;
                wakeups += r.wakeups.saturating_sub(p.wakeups);
            }
            // Not seen last time: everything it ever did falls in this window, as far as
            // anyone can tell. Counting it all errs towards active.
            None => {
                bytes += r.bytes;
                wakeups += r.wakeups;
                cpu += r.cpu;
            }
        }
    }
    // A member seen last time and gone now: what it did since went to whoever reaped it — a
    // member's counters, or init's for an orphan — so this window's bytes are not known whole.
    let vanished = left
        || prev.procs.iter().any(|p| {
            !readings
                .iter()
                .any(|r| r.pid == p.pid && r.start == p.start)
        });
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
    let last_active = if bytes as f64 > budget || window_ms >= QUIET_FOR_MS || vanished {
        now
    } else {
        prev.last_active
    };
    match minima.last_mut() {
        Some((h, m)) if *h == hour => *m = m.min(rate),
        _ => minima.push((hour, rate)),
    }
    let state = SlotState {
        at: now,
        procs: readings,
        last_active,
        minima,
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
    };
    (state, m)
}

/// What the rule would do with a slot, given its window and whether a client is attached
/// (`None`: zmx could not say).
pub fn verdict(m: &Measure, attached: Option<bool>) -> String {
    let keep = |why: &str| format!("the activity rule would keep it: {why}");
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

/// A pass's line for one slot.
pub fn describe(slot: &str, m: &Measure, attached: Option<bool>) -> String {
    let what = match m.rate {
        Some(r) => format!(
            "{:.0} B/s over {}s, {} wakeups/s, cpu {:.1} ms/s, {} process(es), line {:.0} B/s ({:.0} B a window){}, quiet {}m",
            r,
            m.window_ms / 1000,
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
    format!("{slot}: measured — {what}; {}", verdict(m, attached))
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
    let mut next = BTreeMap::new();
    let mut lines = Vec::new();
    for rec in records.iter().filter(|r| r.state == State::Live) {
        // A reading from the future — the clock stepped back — is no reading.
        let old = prev.get(&rec.slot).filter(|o| o.at <= now);
        if let Some(o) = old.filter(|o| now.saturating_sub(o.at) < MIN_WINDOW_MS) {
            lines.push(format!(
                "{}: measured — {}s since the last reading, too short; the next pass counts it",
                rec.slot,
                now.saturating_sub(o.at) / 1000
            ));
            next.insert(rec.slot.clone(), o.clone());
            continue;
        }
        let root = rec.pid.zip(rec.proc_start);
        let (mut readings, mut unreadable, mut left) = (Vec::new(), 0usize, false);
        for pid in members(table, env.get(&rec.slot), root) {
            match read(pid) {
                Some(r) => readings.push(r),
                // Gone since the snapshot is the truth; there but unreadable is unknown.
                None if std::path::Path::new(&format!("/proc/{pid}")).exists() => unreadable += 1,
                // Gone since the snapshot: its bytes went to whoever reaped it, perhaps after
                // that one was read — a member that left, as in `step`.
                None => left = true,
            }
        }
        if unreadable > 0 {
            lines.push(format!(
                "{}: measured — {unreadable} of its processes could not be read; the activity rule would keep it: unknown",
                rec.slot
            ));
            // The span measured again next pass, and counted as active: unknown is.
            if let Some(o) = old {
                next.insert(
                    rec.slot.clone(),
                    SlotState {
                        last_active: now,
                        ..o.clone()
                    },
                );
            }
            continue;
        }
        if readings.is_empty() {
            continue;
        }
        let (state, m) = step(old, now, readings, left);
        let attached = sessions.as_ref().and_then(|all| {
            all.iter()
                .find(|s| s.name == rec.slot && s.answered)
                .map(|s| s.attached)
        });
        lines.push(describe(&rec.slot, &m, attached));
        next.insert(rec.slot.clone(), state);
    }
    if let Err(e) = store(&next) {
        // Gone, so the next pass really does start over — a first reading, which keeps every
        // slot — rather than measuring one window across the pass that could not save.
        let _ = std::fs::remove_file(state_path());
        lines.push(format!(
            "activity: state not saved ({e}); the next pass starts over"
        ));
    }
    lines
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
        out.insert(
            slot.clone(),
            SlotState {
                at,
                procs,
                last_active,
                minima,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (s, m) = step(None, T0, vec![r(1, 7, 5_000)], false);
        assert_eq!((m.rate, m.quiet_ms, s.last_active), (None, 0, T0));
        assert!(verdict(&m, Some(false)).contains("would keep"));
    }

    #[test]
    fn a_window_is_held_to_the_line_from_before_it() {
        // A slot with no history: its first window at 2 KB/s is held to the low line and is
        // active — it must not set its own bar at ten times itself. Calibration: a slot whose
        // floor of 100 was learned earlier holds 54 KB over three minutes under its 60 KB budget and
        // calls it quiet.
        let s = step(None, T0, vec![r(1, 7, 0)], false).0;
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, 360_000)], false);
        assert_eq!((m.floor, m.line, m.quiet_ms), (None, LINE_MIN, 0), "{m:?}");
        let learned = SlotState {
            minima: vec![(T0 / HOUR_MS, 100.0)],
            ..s
        };
        let (_, m) = step(Some(&learned), T0 + 180_000, vec![r(1, 7, 54_000)], false);
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
        };
        let burst = 165 * 100 + 15 * 9_342;
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, burst)], false);
        assert!(m.rate.unwrap() < m.line, "{m:?}");
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, 180 * 100)], false);
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
        };
        let (_, m) = step(Some(&s), T0 + 2 * HOUR_MS, vec![r(1, 7, 36_000)], false);
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, 36_000)], false);
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
        };
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, 18_000)], false);
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(
            Some(&s),
            T0 + 180_000,
            vec![r(1, 7, 18_000), r(2, 8, 0)],
            false,
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
        };
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, 18_000)], true);
        assert_eq!(m.quiet_ms, 0, "{m:?}");
        let (_, m) = step(Some(&s), T0 + 180_000, vec![r(1, 7, 18_000)], false);
        assert_eq!(m.quiet_ms, 180_000, "calibration: {m:?}");
    }

    #[test]
    fn wakeups_that_fell_have_no_figure() {
        let s = step(
            None,
            T0,
            vec![Reading {
                pid: 1,
                start: 7,
                bytes: 0,
                wakeups: 500,
                cpu: 0,
            }],
            false,
        )
        .0;
        let (_, m) = step(
            Some(&s),
            T0 + 100_000,
            vec![Reading {
                pid: 1,
                start: 7,
                bytes: 0,
                wakeups: 400,
                cpu: 0,
            }],
            false,
        );
        assert_eq!(m.wakeups, None);
        assert!(describe("claude-1", &m, Some(false)).contains("? wakeups/s"));
        let (_, m) = step(
            Some(&s),
            T0 + 100_000,
            vec![Reading {
                pid: 1,
                start: 7,
                bytes: 0,
                wakeups: 600,
                cpu: 0,
            }],
            false,
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
        let mut s = step(None, T0, vec![r(1, 7, 0)], false).0;
        let mut bytes = 0;
        let mut at = T0;
        let mut last = None;
        for _ in 0..5 {
            at += 180_000;
            bytes += 18_000;
            let (n, m) = step(Some(&s), at, vec![r(1, 7, bytes)], false);
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
        let (_, m) = step(Some(&s), at, vec![r(1, 7, bytes)], false);
        assert_eq!(m.quiet_ms, 0);
        assert!(verdict(&m, Some(false)).contains("quiet under 10m"));
    }

    #[test]
    fn a_new_process_counts_whole_and_a_reused_pid_is_a_new_process() {
        let s = step(None, T0, vec![r(1, 7, 1_000)], false).0;
        // pid 1 again but started later: a different process, all of its bytes counted; and
        // a child seen for the first time, all of its own.
        let (_, m) = step(
            Some(&s),
            T0 + 100_000,
            vec![r(1, 9, 2_000), r(2, 8, 3_000)],
            false,
        );
        assert_eq!(m.rate, Some(50.0));
        // The same process: only what it did since.
        let (_, m) = step(Some(&s), T0 + 100_000, vec![r(1, 7, 1_500)], false);
        assert_eq!(m.rate, Some(5.0));
    }

    #[test]
    fn the_floor_is_the_last_days_quietest_hour_and_forgets_older_ones() {
        let mut s = SlotState {
            at: T0,
            procs: vec![r(1, 7, 0)],
            last_active: T0,
            minima: vec![(T0 / HOUR_MS - 30, 1.0)],
        };
        s.minima.push((T0 / HOUR_MS - 2, 80.0));
        let (n, m) = step(Some(&s), T0 + 100_000, vec![r(1, 7, 20_000)], false);
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
            },
        );
        let back = from_json(&json::parse(&json::to_string_pretty(&to_json(&all))).unwrap());
        assert_eq!(back, all);
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
