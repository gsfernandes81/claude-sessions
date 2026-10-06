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
//! **A slot with no conversation on disk is closed, not offloaded** (issue #5). A slot opened
//! and never spoken to, or `/clear`ed and left, has a `session_id` and nothing behind it to
//! resume (`transcript::has_exchange`). It is stopped by the same path, but
//! the record says `closed`: an `offloaded` row promises a resume that would exit at once.
//!
//! Run as a pass: `claude-sessions offload`, from whatever timer the box uses. `--dry-run`
//! decides and reports without signalling anything.

use crate::clock::{self, Millis};
use crate::lockfile;
use crate::mem;
use crate::procinfo::{self, Proc};
use crate::registry::{self, SlotRecord, State};
use crate::signal::{self, SIGKILL, SIGTERM, Sent};
use crate::zmx;
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

/// For zmx to drop a session whose program has gone. It removes the session at once; this is
/// margin. Its daemon process lingers about 2.4 s more and exits by itself (measured on zmx
/// 0.8.1), which nothing waits for.
const ZMX_GRACE: Duration = Duration::from_secs(2);

/// What this pass could see of a slot's process, gathered before deciding.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// The recorded pid is alive with its recorded start time.
    pub alive: bool,
    /// Whether a client is attached to the slot's zmx session, or `None` when zmx did not
    /// answer for it. Only ever read for a slot already known to be alive.
    pub attached: Option<bool>,
    /// Whether `/proc` could be listed at all. Without it there is no knowing what is
    /// running under the slot.
    pub table_readable: bool,
    /// The first descendant that is not a `claude`: a background build, a dev server, a
    /// shell. Its existence means work is running that a stop would kill.
    pub foreign_descendant: Option<String>,
    /// The record's current conversation has a transcript on disk
    /// ([`SlotRecord::has_conversation`]). Without one, a stop is a close.
    pub conversation: bool,
    /// The newest write to the conversation's transcript or to any of its subagents'
    /// (`<conversation>/subagents/…`), as file modification time. A turn writes as it goes,
    /// whatever started it, and a background subagent writes its own transcript, so a write
    /// later than the last `Stop` is activity no hook reported (issue #9).
    pub last_write_ms: Option<Millis>,
    /// When the owner's Esc ended the last turn, read from the transcript's trailing
    /// `[Request interrupted by user…]` entry. An interrupt fires no hook, so without this a
    /// slot interrupted mid-turn read `busy` — or waiting for you, if the Esc answered a
    /// permission prompt — until its next turn ended.
    pub interrupted_at: Option<Millis>,
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
    Background(Vec<String>),
    Attached,
    NoSession,
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
            Hold::Background(tasks) => {
                write!(f, "background work running: {}", tasks.join("; "))
            }
            Hold::Attached => write!(f, "attached"),
            Hold::NoSession => write!(
                f,
                "zmx did not answer for it, so attached cannot be ruled out"
            ),
            Hold::ProcUnreadable => write!(f, "could not list /proc to see what runs under it"),
            Hold::Running(what) => write!(f, "{what} is running under it"),
        }
    }
}

/// What to do with a slot that may be stopped. Both carry how long it has been idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Stop it and mark it `offloaded`, to be resumed from its row.
    Offload { idle: Millis },
    /// Stop it and mark it `closed`: there is no conversation on disk to resume.
    Close { idle: Millis },
}

impl Verdict {
    pub fn idle(self) -> Millis {
        match self {
            Verdict::Offload { idle } | Verdict::Close { idle } => idle,
        }
    }
}

/// The rule, pure, and what follows from it: keep the slot, offload it, or close it.
///
/// A slot that may be stopped ([`decide`]) is closed rather than offloaded when its record's
/// conversation has no transcript on disk — a slot opened and never prompted.
pub fn judge(rec: &SlotRecord, now: Millis, seen: &Seen) -> Result<Verdict, Hold> {
    let idle = decide(rec, now, seen)?;
    Ok(if seen.conversation {
        Verdict::Offload { idle }
    } else {
        Verdict::Close { idle }
    })
}

/// The rule, pure: may this slot be stopped now? `Ok` carries how long it has been idle.
/// Whether that stop is an offload or a close is [`judge`]'s answer, not this one's.
///
/// Stoppable when: live · its process alive · resumable · nothing waiting for you · no
/// pending timer, whoever set it · a `Stop`, or a start at the prompt, is the latest activity
/// (`SubagentStart`/`SubagentStop` are not activity) · idle past the threshold · detached · nothing but Claude Code under it.
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
    // An Esc after the latest activity the hooks recorded ended that turn, whatever they say: a
    // prompt or a permission request it interrupted is gone, and claude is at its prompt.
    let esc = rec.esc_ended(seen.interrupted_at);
    if rec.needs_you && esc.is_none() {
        return Err(Hold::NeedsYou);
    }
    if rec.has_pending_timer(now) {
        return Err(Hold::PendingTimer);
    }
    // `Stop` and a start at the prompt each set `last_activity_ms` to their own time, and
    // everything that happens afterwards — a prompt, a nested claude's events, a compaction —
    // moves `last_activity_ms` past them. So "the latest activity left it idle" is
    // exactly this, with `busy` as belt and braces. A resumed slot the owner looked at and
    // left is as idle as one that finished a turn.
    let stop = match (esc, rec.last_stop_ms.max(rec.ready_ms)) {
        (Some(at), _) => at,
        (None, Some(stop)) if stop >= rec.last_activity_ms && !rec.busy => stop,
        _ => return Err(Hold::NotStopped),
    };
    // The turn ended, but what it started in the background has not (issue #9): a background
    // subagent, a Workflow run, a cloud session — listed by `Stop`, and kept between `Stop`s by
    // `SubagentStart`/`SubagentStop` (issue #10). None is a process of its own to see.
    if !rec.background.is_empty() {
        return Err(Hold::Background(rec.background.clone()));
    }
    // Idle from the last thing written, not only the last thing a hook said: a turn started by
    // a fired wake-up or a finished task's notification may announce itself to no hook, and a
    // subagent's transcript moves while the parent's record sits still.
    let since = stop.max(seen.last_write_ms.unwrap_or(0));
    let idle = now.saturating_sub(since);
    if idle < IDLE_AFTER_STOP_MS {
        return Err(Hold::TooRecent {
            left_ms: IDLE_AFTER_STOP_MS - idle,
        });
    }
    match seen.attached {
        Some(true) => return Err(Hold::Attached),
        None => return Err(Hold::NoSession),
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

/// Gather what `decide` and `judge` need for one slot.
pub fn look(rec: &SlotRecord, table: Option<&[Proc]>) -> Seen {
    let (Some(pid), Some(start)) = (rec.pid, rec.proc_start) else {
        return Seen::default();
    };
    let alive = procinfo::is_alive(pid, start);
    Seen {
        alive,
        attached: if alive {
            zmx::session_for(&rec.slot)
                .flatten()
                .filter(|s| s.answered)
                .map(|s| s.attached)
        } else {
            None
        },
        table_readable: table.is_some(),
        foreign_descendant: table.and_then(|t| foreign_descendant(t, pid)),
        conversation: rec.has_conversation(),
        last_write_ms: rec.conversation_path().and_then(|p| last_write(&p)),
        // Read only when the hooks left the slot mid-turn or waiting: an idle slot's tail
        // would say nothing new.
        interrupted_at: (rec.busy || rec.needs_you)
            .then(|| rec.conversation_path())
            .flatten()
            .and_then(|p| crate::transcript::interrupted_at(&p)),
    }
}

/// The newest modification time of a conversation's transcript and everything under its
/// `subagents/` directory — subagent transcripts, and Workflow runs a level or two deeper.
/// Bounded in depth: the layout is Claude Code's, and an unexpected tree must cost a few
/// `stat`s, not a walk of the disk.
pub fn last_write(transcript: &std::path::Path) -> Option<Millis> {
    fn mtime(p: &std::path::Path) -> Option<Millis> {
        let t = std::fs::metadata(p).ok()?.modified().ok()?;
        Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as Millis)
    }
    fn walk(dir: &std::path::Path, depth: u32, newest: &mut Option<Millis>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if let Some(t) = mtime(&p) {
                *newest = (*newest).max(Some(t));
            }
            if depth > 0 && e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&p, depth - 1, newest);
            }
        }
    }
    let mut newest = mtime(transcript);
    walk(
        &transcript.with_extension("").join("subagents"),
        3,
        &mut newest,
    );
    newest
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

/// Whether a slot's claude is its zmx daemon's direct child — the shape every slot has.
pub fn under_zmx(table: Option<&[Proc]>, pid: u32) -> bool {
    let Some(table) = table else { return false };
    let Some(ppid) = table.iter().find(|p| p.pid == pid).map(|p| p.ppid) else {
        return false;
    };
    table.iter().any(|p| p.pid == ppid && p.comm == "zmx")
}

/// After the claude is gone, make sure zmx dropped its session.
///
/// zmx removes a session the moment its program exits, so this is normally one listing. Its
/// daemon lingers a couple of seconds and exits by itself; nothing waits for that. A slot whose
/// claude was not under zmx — its daemon killed — has no session to wait for.
pub fn teardown_zmx(slot: &str, was_under_zmx: bool) -> Vec<String> {
    let mut notes = Vec::new();
    if !was_under_zmx {
        notes.push("no zmx daemon above it".into());
        return notes;
    }
    let deadline = Instant::now() + ZMX_GRACE;
    loop {
        match zmx::session_for(slot) {
            Some(None) => return notes,
            _ if Instant::now() >= deadline => {
                notes.push("zmx still lists its session".into());
                return notes;
            }
            _ => sleep(Duration::from_millis(50)),
        }
    }
}

/// One pass over every slot, plus the orphan sweep.
pub fn run(dry_run: bool) -> io::Result<()> {
    let m = mem::read();
    if let Some(free) = m.headroom() {
        // The cache is said apart, so a reader can tell it from use (claude-sessions#7): the
        // free figure counts it as free, as the kernel would.
        // A cache figure that could not be read is left out rather than said as zero.
        let cache = m
            .reclaimable
            .map(|r| format!(" ({} MB of it reclaimable page cache)", r / (1024 * 1024)))
            .unwrap_or_default();
        say!(
            "memory: {} MB free in this container{cache}",
            free / (1024 * 1024)
        );
    }
    let (mut offloaded, mut closed) = (0usize, 0usize);
    // One snapshot of /proc for the whole pass, taken holding no lock. Reading every
    // process's stat and cmdline is the slow part of a pass, and a hook that arrives while a
    // slot's lock is held has to wait for it — issue #1: a prompt dropped that way leaves a
    // busy claude reading as idle, and ten minutes later the offloader would stop it.
    let table = procinfo::table();
    let records = registry::all()?;
    // What the activity rule would do, beside what this pass does (0.4.5: measured, not
    // acted on). Before any stop, so a slot this pass offloads is measured as it was.
    for line in crate::activity::pass(&records, table.as_deref(), clock::now()) {
        say!("{line}");
    }
    for listed in records {
        if !matches!(listed.state, State::Live | State::Offloading) {
            continue;
        }
        // First decision from the snapshot and the record as listed, holding nothing. Every
        // slot that is kept — nearly all of them, nearly always — ends here.
        let verdict = match judge(&listed, clock::now(), &look(&listed, table.as_deref())) {
            Ok(verdict) => verdict,
            Err(hold) => {
                say!("{}: kept — {hold}", listed.slot);
                continue;
            }
        };
        // A dry run writes nothing, so it has nothing to protect and takes no lock at all.
        if dry_run {
            match verdict {
                Verdict::Offload { idle } => {
                    say!("{}: would offload, idle {}m", listed.slot, idle / 60_000)
                }
                Verdict::Close { idle } => say!(
                    "{}: would close, idle {}m — {NO_CONVERSATION}",
                    listed.slot,
                    idle / 60_000
                ),
            }
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
                say!("{slot}: kept — its lock is busy; next pass");
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
        let verdict = match judge(&rec, clock::now(), &look(&rec, fresh.as_deref())) {
            Ok(verdict) => verdict,
            Err(hold) => {
                say!("{slot}: kept — {hold}");
                continue;
            }
        };
        match stop_quiet(&mut rec, verdict, fresh.as_deref())? {
            Ok(line) => {
                say!("{line}");
                match verdict {
                    Verdict::Offload { .. } => offloaded += 1,
                    Verdict::Close { .. } => closed += 1,
                }
            }
            Err(why) => say!("{why}"),
        }
    }

    sweep_orphans(dry_run);
    say!(
        "offload: {offloaded} slot(s) offloaded, {closed} closed{}",
        if dry_run { " (dry run)" } else { "" }
    );
    Ok(())
}

/// Why a stop is a close, as the pass and its log say it.
const NO_CONVERSATION: &str = "no conversation on disk to resume";

/// The stop itself, an offload or a close as `verdict` says, with the slot's lock held by the
/// caller. Logged to `offload.log` and never printed: `Ok` is the line saying what was done,
/// `Err` the line saying why it was not finished.
///
/// Never printed for the menu, which stops a slot to make room when the owner accepts mockup
/// 4's offer, with the terminal in raw mode on its own screen, where a stray `println!` would
/// land in the middle of the drawing.
pub fn stop_quiet(
    rec: &mut SlotRecord,
    verdict: Verdict,
    table: Option<&[Proc]>,
) -> io::Result<Result<String, String>> {
    let (Some(pid), Some(start)) = (rec.pid, rec.proc_start) else {
        return Ok(Err(format!(
            "{}: no pid recorded; nothing stopped",
            rec.slot
        )));
    };
    let was_under_zmx = under_zmx(table, pid);
    let offload = matches!(verdict, Verdict::Offload { .. });

    // An offload is written BEFORE the signal: the SessionEnd it provokes must read as an
    // offload. A close writes nothing first — that SessionEnd already reads as a close — so a
    // stop that fails leaves the slot `live` for the next pass to decide again.
    if offload {
        rec.state = State::Offloading;
        rec.updated_ms = clock::now();
        registry::store(rec)?;
    }

    let how = match stop(pid, start, TERM_GRACE, KILL_GRACE) {
        Ok(how) => how,
        Err(e) => {
            // Left as it was: the next pass decides again, and reconcile finishes it if the
            // process does die.
            let line = format!("{}: stop failed: {e}", rec.slot);
            log(&line);
            return Ok(Err(line));
        }
    };
    let notes = teardown_zmx(&rec.slot, was_under_zmx);

    rec.state = if offload {
        State::Offloaded
    } else {
        State::Closed
    };
    rec.busy = false;
    rec.updated_ms = clock::now();
    registry::store(rec)?;

    let line = format!(
        "{}: {} pid {pid} after {}m idle ({}){}{}{}",
        rec.slot,
        if offload { "offloaded" } else { "closed" },
        verdict.idle() / 60_000,
        match how {
            Stopped::ByTerm => "TERM",
            Stopped::ByKill => "needed KILL",
            Stopped::AlreadyGone => "already gone",
        },
        if offload {
            String::new()
        } else {
            format!(" — {NO_CONVERSATION}")
        },
        if notes.is_empty() { "" } else { "; " },
        notes.join("; ")
    );
    log(&line);
    Ok(Ok(line))
}

// ── the orphan sweep ────────────────────────────────────────────────────────

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
/// **Gone is inferred from the parent.** A process whose parent dies is reparented to init or a
/// subreaper, so a transient daemon whose parent is no longer a `claude` has lost the session
/// that started it. That inference is only sound because **this tool runs with agent view
/// disabled** (owner, 2026-10-03): agent view's supervisor is *meant* to outlive the session
/// that started it — it keeps background sessions running after the terminal closes, per the
/// vendor docs — and on a box with agent view on, this rule would pick out working
/// supervisors. With it off there is no legitimate one, and a transient daemon left behind is
/// a leak.
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

/// A daemon younger than this is left alone, whatever its parent: a tree caught in the moment
/// between its spawner exiting and its own exit is not a leak yet. The offloader's own
/// threshold, for the same reason.
const SWEEP_MIN_AGE: Duration = Duration::from_millis(IDLE_AFTER_STOP_MS);

/// What the sweep does with one orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepAct {
    /// Too young, or of unknown age: left alone.
    Keep,
    /// A dry run's `WOULD KILL`.
    Report,
    Kill,
}

/// The age check comes first in a dry run too, so a dry run reads exactly as the live sweep
/// would act: infra's gate had to count consecutive passes to make up for a `WOULD KILL`
/// printed for a daemon the live sweep would have left alone (2026-10-03).
fn sweep_act(dry_run: bool, age: Option<Duration>) -> SweepAct {
    match (old_enough(age), dry_run) {
        (false, _) => SweepAct::Keep,
        (true, true) => SweepAct::Report,
        (true, false) => SweepAct::Kill,
    }
}

/// Old enough to sweep? An age that cannot be read keeps the tree: evidence to act, never to
/// hold off.
fn old_enough(age: Option<Duration>) -> bool {
    age.is_some_and(|a| a >= SWEEP_MIN_AGE)
}

/// Stop an orphan's whole tree: `TERM` to every process in it, deepest first so nothing is
/// respawned by its parent, then the grace, then `KILL` to whatever is left. Every signal is
/// sent only to the recorded process (pid and start time, through a pidfd — `signal.rs`).
/// Returns how many processes are still alive afterwards.
fn kill_tree(o: &Orphan<'_>, grace: Duration) -> usize {
    let mut targets: Vec<(u32, u64)> = o.tree.iter().rev().map(|p| (p.pid, p.start)).collect();
    targets.push((o.daemon.pid, o.daemon.start));
    for &(pid, start) in &targets {
        let _ = signal::send(pid, start, SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline
        && targets
            .iter()
            .any(|&(pid, start)| procinfo::is_alive(pid, start))
    {
        sleep(Duration::from_millis(20));
    }
    for &(pid, start) in &targets {
        if procinfo::is_alive(pid, start) {
            let _ = signal::send(pid, start, SIGKILL);
        }
    }
    let settle = Instant::now() + KILL_GRACE;
    while Instant::now() < settle
        && targets
            .iter()
            .any(|&(pid, start)| procinfo::is_alive(pid, start))
    {
        sleep(Duration::from_millis(20));
    }
    targets
        .iter()
        .filter(|&&(pid, start)| procinfo::is_alive(pid, start))
        .count()
}

/// Find the leaked transient daemons and stop them — armed 2026-10-03, on the owner's word and
/// on agent view being disabled wherever this runs (see [`orphans`]). A dry run logs what it
/// would stop and stops nothing.
fn sweep_orphans(dry_run: bool) {
    let Some(table) = procinfo::table() else {
        say!("sweep: could not list /proc; nothing to report");
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
        let what = format!(
            "pid {} start {}, {parent}; under it: [{}]; argv: {}",
            o.daemon.pid,
            o.daemon.start,
            tree.join(", "),
            o.daemon.args.join(" ")
        );
        let age = procinfo::age(o.daemon.start);
        let line = match sweep_act(dry_run, age) {
            SweepAct::Keep => format!(
                "sweep: {}, too young ({}) — {what}",
                if dry_run { "would keep" } else { "kept" },
                age.map_or("age unknown".into(), |a| format!("{}s", a.as_secs()))
            ),
            SweepAct::Report => format!("sweep: WOULD KILL {what}"),
            SweepAct::Kill => match kill_tree(&o, TERM_GRACE) {
                0 => format!("sweep: killed {what}"),
                n => format!("sweep: killed, but {n} process(es) survived KILL — {what}"),
            },
        };
        log(&line);
        say!("{line}");
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
            conversation: true,
            last_write_ms: None,
            interrupted_at: None,
        };
        (r, seen)
    }

    #[test]
    fn background_work_keeps_it_however_long_the_parent_has_been_stopped() {
        // Issue #9. The calibrating case is the idle slot above, which is offloaded.
        let (mut rec, seen) = idle();
        rec.background = vec!["subagent: council reviewer".into()];
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::Background(vec!["subagent: council reviewer".into()]))
        );
        assert_eq!(
            decide(&rec, NOW + 60 * 60 * 1000, &seen),
            Err(Hold::Background(vec!["subagent: council reviewer".into()])),
            "an hour later, still kept"
        );
        rec.background.clear();
        assert!(
            decide(&rec, NOW, &seen).is_ok(),
            "and released once it is gone"
        );
    }

    #[test]
    fn an_esc_ends_the_turn_the_hooks_left_running() {
        // An interrupt fires no hook: the record still says busy from its prompt.
        let (mut rec, mut seen) = idle();
        let prompt = NOW - 2 * IDLE_AFTER_STOP_MS;
        rec.busy = true;
        rec.last_activity_ms = prompt;
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::NotStopped),
            "calibration"
        );
        seen.interrupted_at = Some(prompt + 5_000);
        assert_eq!(decide(&rec, NOW, &seen), Ok(NOW - prompt - 5_000));
        // A fresh Esc counts from itself, as a Stop would.
        seen.interrupted_at = Some(NOW - 60_000);
        rec.last_activity_ms = NOW - 70_000;
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::TooRecent {
                left_ms: IDLE_AFTER_STOP_MS - 60_000
            })
        );
        // A marker older than the latest activity the hooks saw is an earlier turn's.
        seen.interrupted_at = Some(prompt - 1);
        rec.last_activity_ms = prompt;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NotStopped));
    }

    /// Issue #10 end to end, through `events::apply` into `decide`, as 2.1.291 told it under a
    /// pty on 2026-10-06: a turn starts an agent and the owner presses Esc — no hook, only the
    /// transcript's marker. An internal agent nobody announced then sends a `SubagentStop`
    /// carrying the task list. A background agent is on that list until the turn its end wakes
    /// claude for; a foreground one never is, and the Esc cut it off without a `SubagentStop`
    /// of its own (issue #11). Calibration: the same turn with no agent, offloadable throughout.
    #[test]
    fn an_agent_started_in_a_turn_esc_ended_holds_the_slot_while_it_runs() {
        use crate::events::{Binding, Event, apply};
        #[derive(PartialEq)]
        enum Agent {
            None,
            Background,
            Foreground,
        }
        let t0 = NOW - 3 * IDLE_AFTER_STOP_MS;
        let marker = t0 + 4_000;
        let hook = |rec: &mut SlotRecord, body: &str, at: Millis| {
            apply(
                rec,
                &Event::parse(body).unwrap(),
                at,
                Binding::Own,
                Some(100),
                Some(7),
            );
        };
        const RUNNING: &str =
            r#"[{"id":"a90c","type":"subagent","status":"running","description":"look"}]"#;
        for agent in [Agent::None, Agent::Background, Agent::Foreground] {
            let (mut rec, mut seen) = idle();
            hook(
                &mut rec,
                r#"{"hook_event_name":"UserPromptSubmit","prompt":"go"}"#,
                t0,
            );
            if agent != Agent::None {
                hook(
                    &mut rec,
                    r#"{"hook_event_name":"SubagentStart","agent_id":"a90c","agent_type":"general-purpose"}"#,
                    t0 + 1_000,
                );
            }
            seen.interrupted_at = Some(marker);
            assert_eq!(
                rec.esc_ended(seen.interrupted_at),
                Some(marker),
                "the menu's Idle"
            );
            let verdict = decide(&rec, NOW, &seen);
            match agent {
                Agent::None => assert!(verdict.is_ok(), "calibration"),
                _ => assert_eq!(
                    verdict,
                    Err(Hold::Background(vec!["subagent: general-purpose".into()])),
                    "held until something says otherwise"
                ),
            }
            let listed = if agent == Agent::Background {
                RUNNING
            } else {
                "[]"
            };
            hook(
                &mut rec,
                &format!(
                    r#"{{"hook_event_name":"SubagentStop","agent_id":"a941","agent_type":"","background_tasks":{listed}}}"#
                ),
                t0 + 30_000,
            );
            assert_eq!(
                rec.esc_ended(seen.interrupted_at),
                Some(marker),
                "still Idle"
            );
            if agent != Agent::Background {
                assert!(decide(&rec, NOW, &seen).is_ok(), "nothing running");
                continue;
            }
            assert_eq!(
                decide(&rec, NOW, &seen),
                Err(Hold::Background(vec!["subagent: look".into()]))
            );
            // Its own `SubagentStop` still lists it; the notification turn's `Stop` does not.
            let end = t0 + 45_000;
            hook(
                &mut rec,
                &format!(
                    r#"{{"hook_event_name":"SubagentStop","agent_id":"a90c","background_tasks":{RUNNING}}}"#
                ),
                end,
            );
            assert!(matches!(decide(&rec, NOW, &seen), Err(Hold::Background(_))));
            hook(
                &mut rec,
                r#"{"hook_event_name":"UserPromptSubmit","prompt":"<task-notification>"}"#,
                end + 10,
            );
            hook(
                &mut rec,
                r#"{"hook_event_name":"Stop","background_tasks":[]}"#,
                end + 1_000,
            );
            assert_eq!(
                rec.esc_ended(seen.interrupted_at),
                None,
                "that turn is newer"
            );
            assert!(decide(&rec, NOW, &seen).is_ok());
            // Ten minutes after the Esc is under ten after that turn's Stop.
            assert!(
                matches!(
                    decide(&rec, marker + IDLE_AFTER_STOP_MS + 1, &seen),
                    Err(Hold::TooRecent { .. })
                ),
                "idle from that turn, not from the Esc"
            );
        }
    }

    #[test]
    fn an_esc_answers_a_permission_prompt_too() {
        let (mut rec, mut seen) = idle();
        let asked = NOW - 2 * IDLE_AFTER_STOP_MS;
        rec.busy = true;
        rec.needs_you = true;
        rec.last_activity_ms = asked;
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NeedsYou), "calibration");
        seen.interrupted_at = Some(asked + 1_000);
        assert!(decide(&rec, NOW, &seen).is_ok());
    }

    #[test]
    fn a_write_after_the_stop_restarts_the_idle_clock() {
        // A turn started by a fired wake-up or a notification, or a background subagent's own
        // transcript, writes without telling any hook (issue #9).
        let (rec, mut seen) = idle();
        seen.last_write_ms = Some(NOW - 60_000);
        assert_eq!(
            decide(&rec, NOW, &seen),
            Err(Hold::TooRecent {
                left_ms: IDLE_AFTER_STOP_MS - 60_000
            })
        );
        // Calibration: a write from before the stop changes nothing.
        seen.last_write_ms = rec.last_stop_ms.map(|s| s - 1);
        assert_eq!(decide(&rec, NOW, &seen), Ok(IDLE_AFTER_STOP_MS + 1));
    }

    #[test]
    fn the_newest_write_is_found_in_the_transcript_or_any_subagent_under_it() {
        use std::time::{Duration, UNIX_EPOCH};
        let root = std::env::temp_dir().join(format!("cs-lastwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let deep = root.join("conv/subagents/workflows/run-1");
        std::fs::create_dir_all(&deep).unwrap();
        let at = |ms: u64| UNIX_EPOCH + Duration::from_millis(ms);
        let touch = |p: &std::path::Path, ms: u64| {
            std::fs::write(p, "{}\n").unwrap();
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(at(ms))
                .unwrap();
        };
        let transcript = root.join("conv.jsonl");
        touch(&transcript, 1_000_000);
        // Directories carry the time they were made; pin them old so only files decide.
        let pin = |p: &std::path::Path| {
            std::fs::File::open(p).unwrap().set_modified(at(1)).unwrap();
        };
        assert_eq!(
            {
                for d in [
                    deep.as_path(),
                    &root.join("conv/subagents/workflows"),
                    &root.join("conv/subagents"),
                ] {
                    pin(d);
                }
                last_write(&transcript)
            },
            Some(1_000_000),
            "calibration: the transcript alone"
        );
        touch(&root.join("conv/subagents/agent-a1.jsonl"), 2_000_000);
        touch(&deep.join("agent-w1.jsonl"), 3_000_000);
        for d in [
            deep.as_path(),
            &root.join("conv/subagents/workflows"),
            &root.join("conv/subagents"),
        ] {
            pin(d);
        }
        assert_eq!(
            last_write(&transcript),
            Some(3_000_000),
            "a workflow's agent, two levels down"
        );
        assert_eq!(last_write(&root.join("none.jsonl")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn calibration_an_idle_detached_slot_is_offloadable() {
        let (rec, seen) = idle();
        assert_eq!(decide(&rec, NOW, &seen), Ok(IDLE_AFTER_STOP_MS + 1));
        assert_eq!(
            judge(&rec, NOW, &seen),
            Ok(Verdict::Offload {
                idle: IDLE_AFTER_STOP_MS + 1
            })
        );
    }

    #[test]
    fn an_idle_slot_with_no_conversation_on_disk_is_closed_not_offloaded() {
        // Issue #5: opened at its prompt and never spoken to — a session id, no transcript.
        // Stopping it is right; promising a resume is not.
        let (mut rec, mut seen) = idle();
        let at = rec.last_stop_ms.take().unwrap();
        rec.ready_ms = Some(at);
        seen.conversation = false;
        assert_eq!(
            judge(&rec, NOW, &seen),
            Ok(Verdict::Close {
                idle: IDLE_AFTER_STOP_MS + 1
            })
        );
        assert_eq!(
            decide(&rec, NOW, &seen),
            Ok(IDLE_AFTER_STOP_MS + 1),
            "still a slot that may be stopped, which is all the menu's room offer asks"
        );
        // Calibration: the same slot with its transcript there is offloaded.
        seen.conversation = true;
        assert_eq!(
            judge(&rec, NOW, &seen),
            Ok(Verdict::Offload {
                idle: IDLE_AFTER_STOP_MS + 1
            })
        );
    }

    #[test]
    fn no_conversation_never_overrides_a_reason_to_keep() {
        // A close is still a stop: every hold applies to it exactly as to an offload.
        let (rec, mut seen) = idle();
        seen.conversation = false;
        seen.attached = Some(true);
        assert_eq!(judge(&rec, NOW, &seen), Err(Hold::Attached));
        let (mut rec, mut seen) = idle();
        seen.conversation = false;
        rec.last_stop_ms = Some(NOW - 1_000);
        rec.last_activity_ms = NOW - 1_000;
        assert!(matches!(
            judge(&rec, NOW, &seen),
            Err(Hold::TooRecent { .. })
        ));
        let (mut rec, mut seen) = idle();
        seen.conversation = false;
        rec.session_id = None;
        assert_eq!(judge(&rec, NOW, &seen), Err(Hold::NotResumable));
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
        assert_eq!(decide(&rec, NOW, &seen), Err(Hold::NoSession));
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
    fn a_tree_too_young_or_of_unknown_age_is_never_swept() {
        assert!(!old_enough(None), "unknown age keeps it");
        assert!(!old_enough(Some(SWEEP_MIN_AGE - Duration::from_secs(1))));
        assert!(
            old_enough(Some(SWEEP_MIN_AGE)),
            "calibration: old enough is swept"
        );
    }

    #[test]
    fn a_dry_run_sweep_reports_only_what_the_live_one_would_kill() {
        let young = Some(SWEEP_MIN_AGE - Duration::from_secs(1));
        let old = Some(SWEEP_MIN_AGE);
        for dry in [true, false] {
            assert_eq!(sweep_act(dry, young), SweepAct::Keep, "dry run {dry}");
            assert_eq!(sweep_act(dry, None), SweepAct::Keep, "dry run {dry}");
        }
        // Calibration: old enough, the dry run reports exactly what the live sweep kills.
        assert_eq!(sweep_act(true, old), SweepAct::Report);
        assert_eq!(sweep_act(false, old), SweepAct::Kill);
    }

    #[test]
    fn kill_tree_stops_the_daemon_and_everything_under_it() {
        // A stand-in leaked daemon: a shell whose argv reads `daemon run --origin transient`,
        // with two children. Its parent is this test, not a claude, so `orphans` picks it.
        let mut daemon = std::process::Command::new("sh")
            .args([
                "-c",
                "sleep 600 & sleep 600 & wait",
                "daemon",
                "run",
                "--origin",
                "transient",
            ])
            .spawn()
            .expect("spawn");
        let pid = daemon.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let table = loop {
            let t = procinfo::table().unwrap();
            if procinfo::descendants(&t, pid).len() == 2 {
                break t;
            }
            assert!(
                Instant::now() < deadline,
                "the stand-in's children never started"
            );
            sleep(Duration::from_millis(20));
        };
        let found = orphans(&table);
        let o = found
            .iter()
            .find(|o| o.daemon.pid == pid)
            .expect("calibration: the stand-in reads as an orphaned transient daemon");
        let all: Vec<(u32, u64)> = std::iter::once(o.daemon)
            .chain(o.tree.iter().copied())
            .map(|p| (p.pid, p.start))
            .collect();
        assert!(all.iter().all(|&(p, s)| procinfo::is_alive(p, s)));
        assert_eq!(kill_tree(o, Duration::from_secs(2)), 0, "nothing survives");
        assert!(all.iter().all(|&(p, s)| !procinfo::is_alive(p, s)));
        daemon.wait().ok();
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
