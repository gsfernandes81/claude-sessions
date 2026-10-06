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
//! **Why bytes, not CPU.** A model turn is a stream from the API, and claude writes its
//! transcript as it goes; both are bytes through `read`/`write`, counted in `/proc/<pid>/io`
//! the same on a Pi 4 and an x86 box. CPU time is not: the same work costs a slow CPU more
//! of it. Measured on 2.1.291 (2026-10-06): idle at the prompt, 60–130 B/s; a turn or a
//! streaming reply, 14–26 KB/s. Wake-ups — voluntary context switches, a count of times the
//! process blocked — are logged beside them as a second opinion, not used.
//!
//! **Which processes.** Everything a slot's claude starts inherits `CLAUDE_SESSIONS_SLOT`,
//! including a process that double-forks away from it to init, so the environment finds them
//! however they were reparented — plus the recorded claude and its descendants, should one
//! have cleared it. A dead child's counters are folded into its parent's when it is reaped,
//! so a tool that ran and exited between passes is still counted through claude.
//!
//! **The line.** Each slot learns its own floor — its quietest window of the last day — and
//! counts as active above ten times that, held between 512 B/s and 4 KB/s. A future Claude
//! Code that idles noisier raises its own floor; the cap keeps a slot that has only ever been
//! seen busy from setting a line real work could fall under, and the base keeps a near-silent
//! floor from making a stray read look like a turn. All three are bytes, so no device enters.
//!
//! **Which way it errs.** Anything not known counts as active: a slot's first reading, a
//! process not seen last time (its whole history is counted), an attachment `zmx` could not
//! report. Concurrent passes may each write the state; the later write wins and the other's
//! window is simply measured again.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clock::Millis;
use crate::json::{self, Value};
use crate::procinfo::Proc;
use crate::registry::{SlotRecord, State};

/// The variable every process of a slot carries, set by the launch line.
pub const VAR: &str = "CLAUDE_SESSIONS_SLOT";

/// The shortest window a reading counts over. A pass run by hand straight after the timer's
/// would otherwise measure a few seconds, too short to say anything; it is left for the next.
pub const MIN_WINDOW_MS: Millis = 60_000;

/// How long a slot must stay under its line before the rule would act: the offloader's own.
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
    /// `rchar + wchar`: every byte through `read` and `write`, sockets and files alike.
    pub bytes: u64,
    /// Voluntary context switches, summed over the process's threads.
    pub wakeups: u64,
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
    pub window_ms: Millis,
    pub floor: Option<f64>,
    pub line: f64,
    pub quiet_ms: Millis,
    pub procs: usize,
}

/// The line a slot's rate is held against, from its floor.
pub fn line(floor: Option<f64>) -> f64 {
    floor.map_or(LINE_MIN, |f| (f * FLOOR_FACTOR).clamp(LINE_MIN, LINE_MAX))
}

/// One pass's step for one slot, pure. `None` when the window since the last reading is too
/// short to count: the caller keeps the old state, so the next pass measures the whole span.
pub fn step(
    prev: Option<&SlotState>,
    now: Millis,
    readings: Vec<Reading>,
) -> Option<(SlotState, Measure)> {
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
            window_ms: 0,
            floor: None,
            line: line(None),
            quiet_ms: 0,
            procs,
        };
        return Some((state, m));
    };
    let window_ms = now.saturating_sub(prev.at);
    if window_ms < MIN_WINDOW_MS {
        return None;
    }
    let (mut bytes, mut wakeups) = (0u64, 0u64);
    for r in &readings {
        match prev
            .procs
            .iter()
            .find(|p| p.pid == r.pid && p.start == r.start)
        {
            Some(p) => {
                bytes += r.bytes.saturating_sub(p.bytes);
                wakeups += r.wakeups.saturating_sub(p.wakeups);
            }
            // Not seen last time: everything it ever did falls in this window, as far as
            // anyone can tell. Counting it all errs towards active.
            None => {
                bytes += r.bytes;
                wakeups += r.wakeups;
            }
        }
    }
    let secs = window_ms as f64 / 1000.0;
    let rate = bytes as f64 / secs;
    let hour = now / HOUR_MS;
    let mut minima: Vec<(u64, f64)> = prev
        .minima
        .iter()
        .copied()
        .filter(|&(h, _)| h + FLOOR_HOURS > hour)
        .collect();
    match minima.last_mut() {
        Some((h, m)) if *h == hour => *m = m.min(rate),
        _ => minima.push((hour, rate)),
    }
    let floor = minima.iter().map(|&(_, m)| m).reduce(f64::min);
    let line = line(floor);
    let last_active = if rate > line { now } else { prev.last_active };
    let state = SlotState {
        at: now,
        procs: readings,
        last_active,
        minima,
    };
    let m = Measure {
        rate: Some(rate),
        wakeups: Some(wakeups as f64 / secs),
        window_ms,
        floor,
        line,
        quiet_ms: now.saturating_sub(last_active),
        procs,
    };
    Some((state, m))
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
    let what = match (m.rate, m.wakeups) {
        (Some(r), Some(w)) => format!(
            "{:.0} B/s over {}s, {:.1} wakeups/s, {} process(es), line {:.0} B/s{}, quiet {}m",
            r,
            m.window_ms / 1000,
            w,
            m.procs,
            m.line,
            m.floor
                .map(|f| format!(" from floor {f:.0}"))
                .unwrap_or_default(),
            m.quiet_ms / 60_000
        ),
        _ => format!("{} process(es) read", m.procs),
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
        let pids = members(table, env.get(&rec.slot), rec.pid);
        let readings: Vec<Reading> = pids.into_iter().filter_map(read).collect();
        if readings.is_empty() {
            continue;
        }
        let old = prev.get(&rec.slot);
        let Some((state, m)) = step(old, now, readings) else {
            lines.push(format!(
                "{}: measured — {}s since the last reading, too short; the next pass counts it",
                rec.slot,
                now.saturating_sub(old.map_or(now, |o| o.at)) / 1000
            ));
            if let Some(o) = old {
                next.insert(rec.slot.clone(), o.clone());
            }
            continue;
        };
        let attached = sessions.as_ref().and_then(|all| {
            all.iter()
                .find(|s| s.name == rec.slot && s.answered)
                .map(|s| s.attached)
        });
        lines.push(describe(&rec.slot, &m, attached));
        next.insert(rec.slot.clone(), state);
    }
    if let Err(e) = store(&next) {
        lines.push(format!(
            "activity: state not saved ({e}); the next pass starts over"
        ));
    }
    lines
}

/// `CLAUDE_SESSIONS_SLOT` in a process's environment, if it carries one.
fn slot_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    raw.split(|b| *b == 0).find_map(|kv| {
        let v = kv.strip_prefix(VAR.as_bytes())?.strip_prefix(b"=")?;
        Some(String::from_utf8_lossy(v).to_string())
    })
}

/// Every process of each slot in a snapshot, by the environment. Not this process, and not
/// another `claude-sessions`: a hook or a pass run from a slot's own shell carries the
/// variable too, and reading `/proc` would count as that slot's activity.
pub fn by_slot(table: &[Proc]) -> BTreeMap<String, Vec<u32>> {
    let me = std::process::id();
    let mut out: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for p in table {
        if p.pid == me || p.comm == "claude-sessions" {
            continue;
        }
        if let Some(slot) = slot_of(p.pid) {
            out.entry(slot).or_default().push(p.pid);
        }
    }
    out
}

/// A slot's members: those carrying its variable, and the recorded claude with its
/// descendants, should any have cleared it.
pub fn members(table: &[Proc], env: Option<&Vec<u32>>, claude: Option<u32>) -> Vec<u32> {
    let mut pids: Vec<u32> = env.cloned().unwrap_or_default();
    if let Some(root) = claude.filter(|&pid| table.iter().any(|p| p.pid == pid)) {
        pids.push(root);
        pids.extend(
            crate::procinfo::descendants(table, root)
                .iter()
                .map(|p| p.pid),
        );
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// One process's counters now, or `None` if it has gone or cannot be read.
pub fn read(pid: u32) -> Option<Reading> {
    let start = crate::procinfo::start_time(pid)?;
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
    std::fs::write(&tmp, json::to_string_pretty(&to_json(all)))?;
    std::fs::rename(&tmp, path)
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
                [pid, start, bytes, wakeups] => Some(Reading {
                    pid: pid as u32,
                    start: start as u64,
                    bytes: bytes as u64,
                    wakeups: wakeups as u64,
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
    fn a_first_reading_is_active_and_a_short_window_is_left_for_the_next_pass() {
        let (s, m) = step(None, T0, vec![r(1, 7, 5_000)]).unwrap();
        assert_eq!((m.rate, m.quiet_ms, s.last_active), (None, 0, T0));
        assert!(verdict(&m, Some(false)).contains("would keep"));
        assert!(step(Some(&s), T0 + MIN_WINDOW_MS - 1, vec![r(1, 7, 5_001)]).is_none());
    }

    #[test]
    fn idle_bytes_go_quiet_and_a_turn_wakes_it() {
        // 100 B/s for ten minutes in 3-minute passes, then a turn at 15 KB/s. Calibration in
        // both directions: the same rule calls the quiet slot freezable and the busy one not.
        let mut s = step(None, T0, vec![r(1, 7, 0)]).unwrap().0;
        let mut bytes = 0;
        let mut at = T0;
        let mut last = None;
        for _ in 0..5 {
            at += 180_000;
            bytes += 18_000;
            let (n, m) = step(Some(&s), at, vec![r(1, 7, bytes)]).unwrap();
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
        let (_, m) = step(Some(&s), at, vec![r(1, 7, bytes)]).unwrap();
        assert_eq!(m.quiet_ms, 0);
        assert!(verdict(&m, Some(false)).contains("quiet under 10m"));
    }

    #[test]
    fn a_new_process_counts_whole_and_a_reused_pid_is_a_new_process() {
        let s = step(None, T0, vec![r(1, 7, 1_000)]).unwrap().0;
        // pid 1 again but started later: a different process, all of its bytes counted; and
        // a child seen for the first time, all of its own.
        let (_, m) = step(Some(&s), T0 + 100_000, vec![r(1, 9, 2_000), r(2, 8, 3_000)]).unwrap();
        assert_eq!(m.rate, Some(50.0));
        // The same process: only what it did since.
        let (_, m) = step(Some(&s), T0 + 100_000, vec![r(1, 7, 1_500)]).unwrap();
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
        let (n, m) = step(Some(&s), T0 + 100_000, vec![r(1, 7, 20_000)]).unwrap();
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
        let _ = std::fs::read(format!("/proc/{me}/maps"));
        std::fs::write(
            std::env::temp_dir().join(format!("cs-activity-w-{me}")),
            vec![0u8; 100_000],
        )
        .unwrap();
        let b = read(me).unwrap();
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("cs-activity-w-{me}")));
        assert!(b.bytes >= a.bytes + 100_000, "{a:?} {b:?}");
        assert_eq!(a.start, b.start);
    }
}
