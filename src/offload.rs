//! The offloader: stop a slot nobody is using, so its memory goes back to the container, and
//! leave everything needed to bring it back.
//!
//! **It needs evidence to act, never to hold off.** Every rule below is a reason to keep a
//! slot, and anything this pass cannot see — an unreadable `/proc`, a socket that is not
//! there, a record with no pid — keeps it too. A slot wrongly kept costs memory until the next
//! pass; a slot wrongly stopped costs whatever the owner was in the middle of.
//!
//! **The decision and the kill happen under one hold of the slot's lock.** The record is
//! re-read after the lock is taken, the decision is made from that copy, the slot is marked
//! `offloading` before the first signal and `offloaded` after the last, and only then is the
//! lock let go. That is what stops the menu resuming a slot between "idle" and "dead", and
//! what makes the `SessionEnd` the kill provokes read as an offload rather than an `/exit`.
//! That hook cannot take the lock we are holding, gives up after its 400 ms, and logs that it
//! did; the offloader writes `offloaded` itself, so nothing is lost but a `hook.log` line.
//!
//! Run as a pass: `claude-sessions offload`, from whatever timer the box uses. `--dry-run`
//! decides and reports without signalling anything.

use crate::abduco;
use crate::clock::{self, Millis};
use crate::lockfile;
use crate::mem;
use crate::procinfo::{self, Proc};
use crate::registry::{self, SlotRecord, State};
use crate::signal::{self, SIGKILL, SIGTERM, Sent};
use std::fmt;
use std::io::{self, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Idle for this long after `Stop` (owner, 2026-10-01). The old script's hour floor existed
/// only because self-scheduled wake-ups were invisible; the timer records make them visible.
pub const IDLE_AFTER_STOP_MS: Millis = 10 * 60 * 1000;

/// From `TERM` to `KILL`. Claude Code's `SessionEnd` hooks share a 1.5 s budget, so a clean
/// exit is over well inside this; the margin is for a slow disk, not a slow process.
pub const TERM_GRACE: Duration = Duration::from_secs(5);

/// From `KILL` to giving up and saying so. `KILL` cannot be caught, so a process still here
/// after this is stuck in the kernel, and waiting longer will not change that.
pub const KILL_GRACE: Duration = Duration::from_secs(3);

/// For the abduco server to notice its command has gone and exit on its own, which it does.
const ABDUCO_GRACE: Duration = Duration::from_secs(2);

/// What this pass could see of a slot's process, gathered before deciding.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// The recorded pid is alive with its recorded start time.
    pub alive: bool,
    /// The owner-execute bit on the slot's abduco socket, or `None` when there is no socket.
    /// Only ever read for a slot already known to be alive — see `abduco.rs`.
    pub attached: Option<bool>,
    /// Whether `/proc` could be listed at all. Without it there is no knowing what is
    /// running under the slot.
    pub table_readable: bool,
    /// The first descendant that is not a `claude`: a background build, a dev server, a
    /// shell. Its existence means work is running that a stop would kill.
    pub foreign_descendant: Option<String>,
}

/// Why a slot was kept. Every variant is a reason to do nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    NotLive(State),
    NoPid,
    Gone,
    NotResumable,
    NeedsYou,
    PendingTimer,
    NotStopped,
    TooRecent { left_ms: Millis },
    Attached,
    NoSocket,
    ProcUnreadable,
    Running(String),
}

impl fmt::Display for Hold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hold::NotLive(s) => write!(f, "{}", s.as_str()),
            Hold::NoPid => write!(f, "no pid recorded, so nothing can be checked"),
            Hold::Gone => write!(f, "process already gone — reconcile's job, not ours"),
            Hold::NotResumable => write!(
                f,
                "no conversation id or directory recorded, so it could not be brought back"
            ),
            Hold::NeedsYou => write!(f, "waiting for you"),
            Hold::PendingTimer => write!(f, "a timer is pending"),
            Hold::NotStopped => write!(f, "something happened since it last went idle"),
            Hold::TooRecent { left_ms } => {
                write!(f, "idle, offloadable in {}s", left_ms.div_ceil(1000))
            }
            Hold::Attached => write!(f, "attached"),
            Hold::NoSocket => write!(f, "no abduco socket, so attached cannot be ruled out"),
            Hold::ProcUnreadable => write!(f, "could not list /proc to see what runs under it"),
            Hold::Running(what) => write!(f, "{what} is running under it"),
        }
    }
}

/// The rule, pure: may this slot be stopped now? `Ok` carries how long it has been idle.
///
/// Offloadable when: live · its process alive · resumable · nothing waiting for you · no
/// pending timer, whoever set it · a `Stop`, or a start at the prompt, is the latest thing that
/// happened · idle past the threshold · detached · nothing but Claude Code under it.
pub fn decide(rec: &SlotRecord, now: Millis, seen: &Seen) -> Result<Millis, Hold> {
    // `Offloading` is a pass that died between deciding and finishing. Deciding again is
    // right: if the slot is still idle the job is finished, and if a SessionStart has since
    // made it live, it is no longer `Offloading`.
    if !matches!(rec.state, State::Live | State::Offloading) {
        return Err(Hold::NotLive(rec.state));
    }
    if rec.pid.is_none() || rec.proc_start.is_none() {
        return Err(Hold::NoPid);
    }
    if !seen.alive {
        return Err(Hold::Gone);
    }
    // Stopping a slot that could not be resumed would be closing it with extra steps.
    if rec.session_id.is_none() || rec.cwd.is_none() {
        return Err(Hold::NotResumable);
    }
    if rec.needs_you {
        return Err(Hold::NeedsYou);
    }
    if rec.has_pending_timer(now) {
        return Err(Hold::PendingTimer);
    }
    // `Stop` and a start at the prompt each set `last_activity_ms` to their own time, and
    // everything that happens afterwards — a prompt, a nested claude's events, a compaction —
    // moves `last_activity_ms` past them. So "the latest thing that happened left it idle" is
    // exactly this, with `busy` as belt and braces. A resumed slot the owner looked at and
    // left is as idle as one that finished a turn.
    let stop = match rec.last_stop_ms.max(rec.ready_ms) {
        Some(stop) if stop >= rec.last_activity_ms && !rec.busy => stop,
        _ => return Err(Hold::NotStopped),
    };
    let idle = now.saturating_sub(stop);
    if idle < IDLE_AFTER_STOP_MS {
        return Err(Hold::TooRecent {
            left_ms: IDLE_AFTER_STOP_MS - idle,
        });
    }
    match seen.attached {
        Some(true) => return Err(Hold::Attached),
        None => return Err(Hold::NoSocket),
        Some(false) => {}
    }
    if !seen.table_readable {
        return Err(Hold::ProcUnreadable);
    }
    if let Some(what) = &seen.foreign_descendant {
        return Err(Hold::Running(what.clone()));
    }
    Ok(idle)
}

/// Gather what `decide` needs for one slot.
pub fn look(rec: &SlotRecord, table: Option<&[Proc]>) -> Seen {
    let (Some(pid), Some(start)) = (rec.pid, rec.proc_start) else {
        return Seen::default();
    };
    let alive = procinfo::is_alive(pid, start);
    Seen {
        alive,
        attached: if alive {
            abduco::socket_for(&rec.slot).map(|s| s.attached_bit)
        } else {
            None
        },
        table_readable: table.is_some(),
        foreign_descendant: table.and_then(|t| foreign_descendant(t, pid)),
    }
}

/// The first descendant of `pid` that is not Claude Code's own, described for a log line.
///
/// A zombie holds no memory and does no work, so it does not count. A nested `claude` does
/// not either: it is the slot's own subagent or `claude -p`, and if it were doing anything
/// the slot's `last_activity_ms` would be later than its `Stop`.
fn foreign_descendant(table: &[Proc], pid: u32) -> Option<String> {
    procinfo::descendants(table, pid)
        .into_iter()
        .find(|p| !is_claude_machinery(&p.comm) && p.state != 'Z')
        .map(|p| format!("{} (pid {})", p.comm, p.pid))
}

/// Claude Code's own processes, which are the slot rather than work running under it.
///
/// `claude.exe` is the name its helper processes have carried (seen 2026-08-25 by infra's old
/// offloader, which measured and exempted it). They are gone with the agent view off, but if
/// they came back and counted as work, every slot would be held forever and nothing would
/// ever be offloaded (issue #2). `node` is deliberately NOT here: it is how real work — a dev
/// server, a build — shows up, and the old script dropped it for exactly that reason.
fn is_claude_machinery(comm: &str) -> bool {
    matches!(comm, "claude" | "claude.exe")
}

/// How a stop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    ByTerm,
    ByKill,
    /// It had gone before the first signal reached it.
    AlreadyGone,
}

/// `TERM`, grace, `KILL`, grace — each signal sent only to the recorded process.
pub fn stop(
    pid: u32,
    start: u64,
    term_grace: Duration,
    kill_grace: Duration,
) -> io::Result<Stopped> {
    if signal::send(pid, start, SIGTERM)? == Sent::Gone {
        return Ok(Stopped::AlreadyGone);
    }
    if wait_gone(pid, start, term_grace) {
        return Ok(Stopped::ByTerm);
    }
    if signal::send(pid, start, SIGKILL)? == Sent::Gone {
        return Ok(Stopped::ByTerm);
    }
    if wait_gone(pid, start, kill_grace) {
        return Ok(Stopped::ByKill);
    }
    Err(io::Error::other(format!(
        "pid {pid} is still alive {kill_grace:?} after KILL; it is stuck in the kernel"
    )))
}

fn wait_gone(pid: u32, start: u64, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if !procinfo::is_alive(pid, start) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(20));
    }
}

/// The abduco server above a slot's claude, if that is what its parent is.
pub fn abduco_server(table: Option<&[Proc]>, pid: u32) -> Option<(u32, u64)> {
    let ppid = table?.iter().find(|p| p.pid == pid)?.ppid;
    let parent = table?.iter().find(|p| p.pid == ppid)?;
    (parent.comm == "abduco").then_some((parent.pid, parent.start))
}

/// After the claude is gone, make sure its abduco server and socket went with it.
///
/// The server exits by itself when its command does, and takes its socket with it, so this
/// is normally a wait that ends at once. A server still there after its grace gets `TERM`.
/// The socket is removed only once the recorded server is known dead — a killed server
/// leaves its socket behind with the attached bit set, and a socket left like that would show
/// the slot as attached to a menu that has not been taught otherwise.
pub fn teardown_abduco(slot: &str, server: Option<(u32, u64)>) -> Vec<String> {
    let mut notes = Vec::new();
    let Some((spid, sstart)) = server else {
        notes.push("no abduco server above it; socket left for reconcile".into());
        return notes;
    };
    if !wait_gone(spid, sstart, ABDUCO_GRACE) {
        match signal::send(spid, sstart, SIGTERM) {
            Ok(Sent::Delivered) => notes.push(format!("abduco server {spid} lingered; sent TERM")),
            Ok(Sent::Gone) => {}
            Err(e) => notes.push(format!("abduco server {spid}: {e}")),
        }
        if !wait_gone(spid, sstart, ABDUCO_GRACE) {
            notes.push(format!("abduco server {spid} still running; socket left"));
            return notes;
        }
    }
    if let Some(sock) = abduco::socket_for(slot) {
        match std::fs::remove_file(&sock.path) {
            Ok(()) => notes.push("stale socket removed".into()),
            Err(e) => notes.push(format!("stale socket: {e}")),
        }
    }
    notes
}

/// One pass over every slot, plus the orphan sweep.
pub fn run(dry_run: bool) -> io::Result<()> {
    let m = mem::read();
    if let Some(free) = m.headroom() {
        println!("memory: {} MB free in this container", free / (1024 * 1024));
    }
    let mut offloaded = 0usize;
    // One snapshot of /proc for the whole pass, taken holding no lock. Reading every
    // process's stat and cmdline is the slow part of a pass, and a hook that arrives while a
    // slot's lock is held has to wait for it — issue #1: a prompt dropped that way leaves a
    // busy claude reading as idle, and ten minutes later the offloader would stop it.
    let table = procinfo::table();
    for listed in registry::all()? {
        if !matches!(listed.state, State::Live | State::Offloading) {
            continue;
        }
        // First decision from the snapshot and the record as listed, holding nothing. Every
        // slot that is kept — nearly all of them, nearly always — ends here.
        let idle = match decide(&listed, clock::now(), &look(&listed, table.as_deref())) {
            Ok(idle) => idle,
            Err(hold) => {
                println!("{}: kept — {hold}", listed.slot);
                continue;
            }
        };
        // A dry run writes nothing, so it has nothing to protect and takes no lock at all.
        if dry_run {
            println!("{}: would offload, idle {}m", listed.slot, idle / 60_000);
            continue;
        }
        // A candidate. Now the lock, held from here through the kill as the design requires,
        // and the decision made again from what is true under it.
        let slot = listed.slot;
        let _lock = match lockfile::SlotLock::acquire(
            &registry::lock_path(&slot),
            lockfile::INTERACTIVE_WAIT,
        ) {
            Ok(l) => l,
            // Somebody else is acting on it — the menu opening it, another pass. Theirs.
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                println!("{slot}: kept — its lock is busy; next pass");
                continue;
            }
            Err(e) => return Err(e),
        };
        // Re-read under the lock: the copy listed above may be stale by now. The snapshot is
        // taken again too — a stop is about to follow, and it must not be decided on a
        // process tree read before the lock. This one read under the lock is paid only by a
        // slot that is about to be stopped, and the lock is held through the stop anyway.
        let Some(mut rec) = registry::load(&slot)? else {
            continue;
        };
        let fresh = procinfo::table();
        let idle = match decide(&rec, clock::now(), &look(&rec, fresh.as_deref())) {
            Ok(idle) => idle,
            Err(hold) => {
                println!("{slot}: kept — {hold}");
                continue;
            }
        };
        if offload_one(&mut rec, idle, fresh.as_deref())? {
            offloaded += 1;
        }
    }

    sweep_orphans();
    println!(
        "offload: {offloaded} slot(s) offloaded{}",
        if dry_run { " (dry run)" } else { "" }
    );
    Ok(())
}

/// Stop one slot whose decision has already been made, with its lock held by the caller.
fn offload_one(rec: &mut SlotRecord, idle: Millis, table: Option<&[Proc]>) -> io::Result<bool> {
    match offload_quiet(rec, idle, table)? {
        Ok(line) => {
            println!("{line}");
            Ok(true)
        }
        Err(why) => {
            println!("{why}");
            Ok(false)
        }
    }
}

/// The stop itself, logged to `offload.log` and never printed: `Ok` is the line saying what
/// was done, `Err` the line saying why the slot was left `offloading`.
///
/// Split from `offload_one` for the menu, which offloads a slot to make room when the owner
/// accepts mockup 4's offer. It runs with the terminal in raw mode on the menu's own screen,
/// where a stray `println!` would land in the middle of the drawing.
pub fn offload_quiet(
    rec: &mut SlotRecord,
    idle: Millis,
    table: Option<&[Proc]>,
) -> io::Result<Result<String, String>> {
    let (Some(pid), Some(start)) = (rec.pid, rec.proc_start) else {
        return Ok(Err(format!(
            "{}: no pid recorded; nothing stopped",
            rec.slot
        )));
    };
    let server = abduco_server(table, pid);

    // Written BEFORE the signal: the SessionEnd it provokes must read as an offload.
    rec.state = State::Offloading;
    rec.updated_ms = clock::now();
    registry::store(rec)?;

    let how = match stop(pid, start, TERM_GRACE, KILL_GRACE) {
        Ok(how) => how,
        Err(e) => {
            // Left `offloading`: the next pass decides again, and reconcile finishes it if
            // the process does die.
            let line = format!("{}: stop failed: {e}", rec.slot);
            log(&line);
            return Ok(Err(line));
        }
    };
    let notes = teardown_abduco(&rec.slot, server);

    rec.state = State::Offloaded;
    rec.busy = false;
    rec.updated_ms = clock::now();
    registry::store(rec)?;

    let line = format!(
        "{}: offloaded pid {pid} after {}m idle ({}){}{}",
        rec.slot,
        idle / 60_000,
        match how {
            Stopped::ByTerm => "TERM",
            Stopped::ByKill => "needed KILL",
            Stopped::AlreadyGone => "already gone",
        },
        if notes.is_empty() { "" } else { "; " },
        notes.join("; ")
    );
    log(&line);
    Ok(Ok(line))
}

// ── the orphan sweep: logging only ──────────────────────────────────────────

/// A transient daemon whose spawner has gone, with everything under it.
#[derive(Debug)]
pub struct Orphan<'a> {
    pub daemon: &'a Proc,
    /// What its parent is now, if it has one in the table.
    pub parent: Option<&'a Proc>,
    pub tree: Vec<&'a Proc>,
}

/// `… daemon run --origin transient`, in either spelling of the flag.
fn is_transient_daemon(p: &Proc) -> bool {
    let a = &p.args;
    let has = |x: &str, y: &str| a.windows(2).any(|w| w[0] == x && w[1] == y);
    has("daemon", "run")
        && (has("--origin", "transient") || a.iter().any(|s| s == "--origin=transient"))
}

/// The transient daemons whose spawner is gone, pure over a snapshot.
///
/// **Gone is inferred from the parent, and that inference has not met a real daemon.** A
/// process whose parent dies is reparented to init or a subreaper, so a transient daemon
/// whose parent is no longer a `claude` has lost the session that started it. If the daemon
/// turns out to detach on purpose — parent 1 from birth — every one of them will show up
/// here, live or not. That is exactly what a week of log-only running is for: the log line
/// carries the daemon's whole command line so a spawner pid in it, if there is one, can be
/// used instead.
pub fn orphans(table: &[Proc]) -> Vec<Orphan<'_>> {
    table
        .iter()
        .filter(|p| p.state != 'Z' && is_transient_daemon(p))
        .filter_map(|daemon| {
            let parent = table.iter().find(|p| p.pid == daemon.ppid);
            let spawner_alive =
                parent.is_some_and(|p| p.comm == "claude" && !is_transient_daemon(p));
            (!spawner_alive).then(|| Orphan {
                daemon,
                parent,
                tree: procinfo::descendants(table, daemon.pid),
            })
        })
        .collect()
}

/// Log what the sweep WOULD kill, and kill nothing. It is armed only after the owner has
/// read a week of this — and arming it is a code change, not a flag, so it cannot happen by
/// accident from a timer's command line.
fn sweep_orphans() {
    let Some(table) = procinfo::table() else {
        println!("sweep: could not list /proc; nothing to report");
        return;
    };
    for o in orphans(&table) {
        let parent = match o.parent {
            Some(p) => format!("parent now {} (pid {})", p.comm, p.pid),
            None => format!("parent pid {} gone", o.daemon.ppid),
        };
        let tree: Vec<String> = o
            .tree
            .iter()
            .map(|p| {
                let role = ["bg-pty-host", "bg-spare"]
                    .into_iter()
                    .find(|r| p.args.iter().any(|a| a.contains(r)));
                format!(
                    "{} {}{}",
                    p.pid,
                    p.comm,
                    role.map(|r| format!(" [{r}]")).unwrap_or_default()
                )
            })
            .collect();
        let line = format!(
            "sweep: WOULD KILL pid {} start {}, {parent}; under it: [{}]; argv: {}",
            o.daemon.pid,
            o.daemon.start,
            tree.join(", "),
            o.daemon.args.join(" ")
        );
        log(&line);
        println!("{line}");
    }
}

/// The offloader's own log, beside the registry. Every stop and every would-be sweep is
/// written here, because the owner reads it to decide whether to arm the sweep — and a timer
/// running this every few minutes has nowhere else to put it.
fn log(line: &str) {
    let dir = registry::dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("offload.log"))
    {
        let _ = writeln!(f, "{} {}", clock::now(), line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Timer;

    const NOW: Millis = 100 * 60 * 1000;

    /// A slot that SHOULD be offloaded — the calibration case every hold is a change from.
    fn idle() -> (SlotRecord, Seen) {
        let mut r = SlotRecord::new("claude-1", 0);
        r.pid = Some(100);
        r.proc_start = Some(7);
        r.session_id = Some("conv".into());
        r.cwd = Some("/workspace".into());
        let stop = NOW - IDLE_AFTER_STOP_MS - 1;
        r.last_stop_ms = Some(stop);
        r.last_activity_ms = stop;
        let seen = Seen {
            alive: true,
            attached: Some(false),
            table_readable: true,
            foreign_descendant: None,
        };
        (r, seen)
    }

    #[test]
    fn calibration_an_idle_detached_slot_is_offloadable() {
        let (rec, seen) = idle();
        assert_eq!(decide(&rec, NOW, &seen), Ok(IDLE_AFTER_STOP_MS + 1));
    }

    #[test]
    fn ten_minutes_is_the_line() {
        let (mut rec, seen) = idle();
        rec.last_stop_ms = Some(NOW - IDLE_AFTER_STOP_MS + 1_000);
        rec.last_activity_ms = rec.last_stop_ms.unwrap();
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::TooRecent { left_ms: 1_000 })
        );
        rec.last_stop_ms = Some(NOW - IDLE_AFTER_STOP_MS);
        rec.last_activity_ms = rec.last_stop_ms.unwrap();
        assert!(
            decide(&rec, NOW, &seen).is_ok(),
            "exactly ten minutes is enough"
        );
    }

    #[test]
    fn attached_or_unknowable_attachment_keeps_it() {
        let (rec, mut seen) = idle();
        seen.attached = Some(true);
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::Attached));
        seen.attached = None;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NoSocket));
    }

    #[test]
    fn waiting_for_you_keeps_it() {
        let (mut rec, seen) = idle();
        rec.needs_you = true;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NeedsYou));
    }

    #[test]
    fn a_pending_timer_keeps_it_whoever_set_it_and_a_fired_one_does_not() {
        let (mut rec, seen) = idle();
        rec.timers.push(Timer {
            id: "wakeup".into(),
            due_ms: Some(NOW + 1),
            recurring: false,
        });
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::PendingTimer));
        rec.timers[0].due_ms = None;
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::PendingTimer),
            "a due time we could not read counts as pending"
        );
        rec.timers[0].due_ms = Some(NOW - 1);
        assert!(decide(&rec, NOW, &seen).is_ok(), "it has fired");
    }

    #[test]
    fn anything_after_the_stop_keeps_it() {
        let (mut rec, seen) = idle();
        // A nested claude's event, a prompt, a SessionStart: each moves activity past the Stop.
        rec.last_activity_ms = rec.last_stop_ms.unwrap() + 1;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotStopped));
        let (mut rec, seen) = idle();
        rec.busy = true;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotStopped));
        let (mut rec, seen) = idle();
        rec.last_stop_ms = None;
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::NotStopped),
            "never stopped"
        );
    }

    #[test]
    fn a_slot_started_at_its_prompt_and_left_is_idle_like_one_that_stopped() {
        let (mut rec, seen) = idle();
        let at = rec.last_stop_ms.take().unwrap();
        rec.ready_ms = Some(at);
        assert!(
            decide(&rec, NOW, &seen).is_ok(),
            "opened and never prompted, ten minutes ago: offloadable"
        );
        // Calibration: a prompt after it means it is working, not idle.
        rec.last_activity_ms = at + 1;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotStopped));
        // And neither readiness nor a Stop at all is still no evidence.
        rec.ready_ms = None;
        rec.last_activity_ms = at;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotStopped));
    }

    #[test]
    fn work_running_under_it_keeps_it() {
        let (rec, mut seen) = idle();
        seen.foreign_descendant = Some("cargo (pid 9)".into());
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::Running("cargo (pid 9)".into()))
        );
        seen.foreign_descendant = None;
        seen.table_readable = false;
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::ProcUnreadable),
            "not being able to look is not the same as nothing being there"
        );
    }

    #[test]
    fn a_slot_that_could_not_be_brought_back_is_kept() {
        let (mut rec, seen) = idle();
        rec.session_id = None;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotResumable));
        let (mut rec, seen) = idle();
        rec.cwd = None;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotResumable));
    }

    #[test]
    fn only_a_live_process_is_a_candidate() {
        let (mut rec, mut seen) = idle();
        seen.alive = false;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::Gone));
        seen.alive = true;
        for s in [State::Offloaded, State::Closed] {
            rec.state = s;
            assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotLive(s)));
        }
        rec.state = State::Offloading;
        assert!(
            decide(&rec, NOW, &seen).is_ok(),
            "a pass that died mid-offload is finished by the next one"
        );
        rec.state = State::Live;
        rec.pid = None;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NoPid));
    }

    fn proc(pid: u32, ppid: u32, comm: &str, args: &str) -> Proc {
        Proc {
            pid,
            ppid,
            comm: comm.into(),
            start: u64::from(pid) * 10,
            state: 'S',
            args: args.split_whitespace().map(str::to_string).collect(),
        }
    }

    #[test]
    fn nested_claudes_and_zombies_are_not_foreign_but_a_shell_is() {
        let mut table = vec![
            proc(100, 50, "claude", "claude"),
            proc(101, 100, "claude", "claude -p hello"),
            proc(102, 100, "bash", "bash -c true"),
        ];
        table[2].state = 'Z';
        assert_eq!(foreign_descendant(&table, 100), None);
        table.push(proc(103, 101, "cargo", "cargo build"));
        assert_eq!(
            foreign_descendant(&table, 100).as_deref(),
            Some("cargo (pid 103)"),
            "found under a nested claude, not just directly under the slot"
        );
    }

    #[test]
    fn claude_exe_helpers_are_the_slot_but_node_is_work() {
        let mut table = vec![
            proc(100, 50, "claude", "claude"),
            proc(101, 100, "claude.exe", "claude.exe --helper"),
        ];
        assert_eq!(
            foreign_descendant(&table, 100),
            None,
            "a claude.exe helper must not hold the slot (issue #2)"
        );
        // Calibration: real work beside it still holds the slot, and node counts as real work.
        table.push(proc(102, 101, "node", "node server.js"));
        assert_eq!(
            foreign_descendant(&table, 100).as_deref(),
            Some("node (pid 102)")
        );
    }

    #[test]
    fn a_transient_daemon_is_an_orphan_only_once_its_claude_is_gone() {
        let table = vec![
            proc(100, 50, "claude", "claude"),
            proc(200, 100, "claude", "claude daemon run --origin transient"),
            proc(201, 200, "claude", "claude bg-pty-host"),
            proc(300, 1, "claude", "claude daemon run --origin=transient"),
            proc(301, 300, "claude", "claude bg-spare"),
            proc(400, 1, "claude", "claude daemon run --origin persistent"),
        ];
        let found = orphans(&table);
        assert_eq!(
            found.len(),
            1,
            "only the reparented transient one: {found:?}"
        );
        assert_eq!(found[0].daemon.pid, 300);
        assert_eq!(
            found[0].tree.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [301]
        );
    }

    #[test]
    fn stop_ends_a_process_that_ignores_term() {
        // Calibration: one that honours TERM goes on TERM.
        let mut polite = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn");
        let pid = polite.id();
        let start = procinfo::start_time(pid).unwrap();
        let how = stop(pid, start, Duration::from_secs(2), Duration::from_secs(2)).unwrap();
        assert_eq!(how, Stopped::ByTerm);
        polite.wait().ok();

        let mut stubborn = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .spawn()
            .expect("spawn");
        let pid = stubborn.id();
        let start = procinfo::start_time(pid).unwrap();
        // Let the shell install its trap before it is tested.
        sleep(Duration::from_millis(200));
        let how = stop(
            pid,
            start,
            Duration::from_millis(300),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(how, Stopped::ByKill);
        stubborn.wait().ok();
    }
}
