//! What a keypress in the menu does: attach to a slot, resume an offloaded one, start a new
//! one, open a shell, close a slot, or offload one to make room for another.
//!
//! **Every child runs as a child.** `zmx attach` returns when the owner detaches, and the menu
//! comes back instead of the login ending — a fresh login costs a Cloudflare Access handshake
//! on a metered link. The terminal is handed over through [`Terminal`] for exactly as long as
//! the child has it.
//!
//! **Decide under the slot's lock, act after releasing it.** Every decision that could fork a
//! conversation — is it offloaded, is that conversation already running somewhere — is made
//! from a copy of the record re-read under the slot's lock, and whatever the menu then does
//! is written into the record before the lock is let go. The child itself runs with no lock
//! held: an attach can last all day, and the offloader and the hooks need the lock meanwhile.
//! Every wait for a lock is bounded (`INTERACTIVE_WAIT`), because this is the ssh door.
//!
//! **A resumed slot reads `live` with no pid until its `SessionStart` arrives.** That is what
//! keeps a second menu from resuming the same conversation in the gap — it sees `live` — and
//! what keeps the offloader off it — no pid is `Hold::NoPid`. The hook binds the new pid when
//! claude starts; if it never does and the slot is gone within seconds, the resume failed and
//! the record goes back to `offloaded`.
//!
//! **Two environment overrides**, so tests can stand programs in for the real ones:
//! `CLAUDE_SESSIONS_ZMX` names the zmx to run (default `zmx`) and
//! `CLAUDE_SESSIONS_CLAUDE` the claude (default `claude`). An empty value is the default.

use crate::clock::{self, Millis};
use crate::fmt;
use crate::live;
use crate::lockfile::{INTERACTIVE_WAIT, SlotLock};
use crate::mem;
use crate::offload;
use crate::procinfo;
use crate::registry::{self, SlotRecord, State};
use crate::ui::{Dialog, Outcome, Row, RowKey, Terminal};
use crate::zmx;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// A start that is over sooner than this, with no `SessionStart` ever binding a pid, is a
/// start that failed rather than a session the owner used and left. `claude --resume` with a
/// transcript that is gone prints its complaint and exits within a second or two; nobody
/// resumes a conversation, reads it and `/exit`s in under ten.
pub const QUICK_FAIL: Duration = Duration::from_secs(10);

/// How long a `live` record with no pid and no zmx session is taken to be starting
/// somewhere else. Between one menu writing that record and its zmx creating the session
/// there are milliseconds; a record still like that half a minute later is a start whose menu
/// died before it could clean up, and treating it as starting forever would strand the slot.
const STARTING_GRACE: Millis = 30_000;

/// For zmx to drop a session once its command has exited. The client can return a moment
/// before the daemon has removed the session, and a failed resume would otherwise read as
/// still running for want of a few milliseconds.
const SESSION_SETTLE: Duration = Duration::from_secs(1);

/// The most stderr lines a failed resume shows. Mockup 5 has room for two at 40 columns; the
/// renderer cuts to fit, and the last lines are where a program says why it stopped.
const STDERR_LINES: usize = 4;

/// The highest `claude-<n>` the allocator will try. A bound on a loop, not a policy: a box
/// with a thousand slots has a problem this would not be the first to notice.
const MAX_SLOTS: u32 = 999;

/// Between `env` and claude: send claude's stderr to the slot's capture file, then become
/// claude. **The `exec` is load-bearing.** A hook binds to a slot only when its claude is the
/// direct child of the slot's zmx daemon (`bind.rs`), and a shell left in between would make
/// the slot's own claude look nested, so no `SessionStart` would ever bind it. The file path
/// is an argument rather than an environment variable so that nothing of ours leaks into
/// claude's environment, or into every tool call it makes. `tests/launch.rs` runs this exact
/// line under the real hook.
pub const START_WRAP: &str = r#"e=$1; shift; exec "$@" 2>>"$e""#;

/// Set to `1` for every claude a slot starts, each only if the login has not set it already:
/// **the terminal's own scrollback instead of Claude Code's** (owner, 2026-10-04, issue #8).
/// Every slot is reached over ssh, mostly from a phone. Claude Code's default renderer keeps
/// the transcript on the alternate screen in a scrollback of its own, so each swipe is a round
/// trip asking the server to repaint; its mouse tracking makes Termux send swipes to claude
/// rather than scroll its own buffer, in every renderer; and the inline renderer still holds
/// older output in its viewport unless virtual scroll is off too. With all three set, output
/// lands in the terminal's buffer and scrolling is local. Only the first has a settings key,
/// so the launcher, which makes the process, is where the mode is chosen.
///
/// **Only if unset**, so a value the login brings (ssh `SendEnv`) wins over this default —
/// and only an empty value turns a feature back on, since Claude Code reads `=0` as set. The names are read from the Claude Code binary, and a
/// rename would fail silently — passed and never read — so `docs/design.md` gives the check to
/// repeat at each Claude Code pin.
pub const SCROLLBACK: [&str; 3] = [
    "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN",
    "CLAUDE_CODE_DISABLE_MOUSE",
    "CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL",
];

/// What the launcher runs and reads, gathered so tests can substitute each of them.
pub struct Deps {
    pub zmx: String,
    pub claude: String,
    /// `$SHELL`, because some containers set bash and some fish.
    pub shell: String,
    /// The memory reading behind the no-room check.
    pub memory: fn() -> mem::Memory,
    pub quick: Duration,
    /// The container's OOM-kill count (`mem::oom_kills`), read around each start.
    pub oom_kills: fn() -> Option<u64>,
    /// Whether the login has set this environment variable, which the slot's claude then
    /// inherits as it is (`SCROLLBACK`).
    pub preset: fn(&str) -> bool,
}

impl Deps {
    pub fn from_env() -> Deps {
        Deps {
            zmx: zmx::program(),
            claude: env_or("CLAUDE_SESSIONS_CLAUDE", "claude"),
            shell: env_or("SHELL", "/bin/sh"),
            memory: mem::read,
            oom_kills: mem::oom_kills,
            quick: QUICK_FAIL,
            preset: |key| std::env::var_os(key).is_some(),
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

// ── the menu's entry points ─────────────────────────────────────────────────

/// `Enter` on a row: attach to it if it is running, resume it if it is offloaded.
///
/// `workspace` is not used: a resume runs in the conversation's own directory, because that
/// is where Claude Code looks for its transcript. It is in the signature so the menu calls
/// every entry point the same way.
pub fn open(rows: &[Row], index: usize, _workspace: &str, term: &mut dyn Terminal) -> Outcome {
    open_with(rows, index, term, &Deps::from_env(), true)
}

/// `n`: a new slot running claude in the workspace.
pub fn new_session(rows: &[Row], workspace: &str, term: &mut dyn Terminal) -> Outcome {
    new_session_with(rows, workspace, term, &Deps::from_env(), true)
}

/// Mockup 4's `y`: offload `victim`, then open `then`, or start a new slot when it is `None`.
pub fn offload_then_open(
    rows: &[Row],
    victim: usize,
    then: Option<usize>,
    workspace: &str,
    term: &mut dyn Terminal,
) -> Outcome {
    offload_then_open_with(rows, victim, then, workspace, term, &Deps::from_env())
}

/// `s`: a login shell in the workspace.
pub fn shell(workspace: &str, term: &mut dyn Terminal) -> Outcome {
    shell_with(workspace, term, &Deps::from_env())
}

/// `c`, once confirmed: stop a running slot and mark it closed, or just mark an offloaded one.
pub fn close(rows: &[Row], index: usize) -> Outcome {
    let Some(row) = rows.get(index) else {
        return no_row(index);
    };
    let n = label(row);
    let slot = match &row.key {
        RowKey::Slot(slot) => slot,
        // A zmx session with no record has no pid and start time on file, and a process
        // found by walking /proc for it now would be a guess. Signalling a guess is exactly
        // what the pid-plus-start-time rule exists to prevent.
        RowKey::Socket(_) => {
            return Outcome::Refused(format!(
                "{n} was not started here, so there is no record of its process to stop it by \
                 safely — open it and /exit"
            ));
        }
        // A conversation from the store is listed because nothing is running it.
        RowKey::Conversation { .. } => {
            return Outcome::Back(Some(format!("{n} was already closed")));
        }
        RowKey::ArchiveFold => return no_row(index),
    };
    let _lock = match lock_slot(slot, &n) {
        Ok(l) => l,
        Err(why) => return Outcome::Refused(why),
    };
    let mut rec = match registry::load(slot) {
        Ok(Some(rec)) => rec,
        Ok(None) => return Outcome::Refused(format!("{n} has no record any more")),
        Err(e) => return Outcome::Refused(format!("{n}: {e}")),
    };
    match rec.state {
        State::Closed => return Outcome::Back(Some(format!("{n} was already closed"))),
        State::Offloaded => {}
        State::Live | State::Offloading => match (rec.pid, rec.proc_start) {
            (Some(pid), Some(start)) if procinfo::is_alive(pid, start) => {
                // The daemon is looked for before the stop, while the claude is still under it
                // to be found by.
                let table = procinfo::table();
                let was_under_zmx = offload::under_zmx(table.as_deref(), pid);
                if let Err(e) = offload::stop(pid, start, offload::TERM_GRACE, offload::KILL_GRACE)
                {
                    // Not marked closed: a claude still running behind a row that says it
                    // has ended is the one outcome worse than this.
                    return Outcome::Refused(format!("could not stop {n}: {e}"));
                }
                offload::teardown_zmx(slot, was_under_zmx);
            }
            // Recorded, and already gone: nothing to stop.
            (Some(_), Some(_)) => {}
            // Not knowing whether zmx has a session for it is not knowing that it does not.
            _ if listed(slot) != Some(false) => {
                return Outcome::Refused(format!(
                    "{n} is running but no process is recorded for it yet, so there is \
                     nothing to stop it by safely — try again in a moment"
                ));
            }
            _ => {}
        },
    }
    rec.state = State::Closed;
    rec.busy = false;
    rec.updated_ms = clock::now();
    if let Err(e) = registry::store(&rec) {
        return Outcome::Refused(format!("{n}: {e}"));
    }
    Outcome::Back(Some(if rec.has_conversation() {
        "closed · resumable from disk".to_string()
    } else {
        "closed".to_string()
    }))
}

// ── open: attach or resume ──────────────────────────────────────────────────

pub(crate) fn open_with(
    rows: &[Row],
    index: usize,
    term: &mut dyn Terminal,
    deps: &Deps,
    check_room: bool,
) -> Outcome {
    let Some(row) = rows.get(index) else {
        return no_row(index);
    };
    let n = label(row);
    let slot = match &row.key {
        RowKey::Socket(name) => return attach_socket(name, &n, term, deps),
        RowKey::Conversation { id, cwd } => {
            return resume_conversation(rows, (id, cwd, &row.title), &n, term, deps, check_room);
        }
        // A heading, not a session; the menu folds it rather than asking for it to be opened.
        RowKey::ArchiveFold => return no_row(index),
        RowKey::Slot(slot) => slot,
    };
    let rec = match registry::load(slot) {
        Ok(Some(rec)) => rec,
        // The record went between the list and the keypress. What is left is the session.
        Ok(None) if listed(slot) == Some(true) => {
            return attach_socket(slot, &n, term, deps);
        }
        Ok(None) => return Outcome::Refused(format!("{n} is gone")),
        Err(e) => return Outcome::Refused(format!("{n}: {e}")),
    };
    // Routing only. Nothing irreversible follows from this unlocked reading: a resume decides
    // again under the lock.
    match classify(&rec, clock::now()) {
        Kind::Running => attach_slot(rows, slot, term, deps),
        // A closed slot is listed (owner, 2026-10-03) so that its conversation can be reached
        // again; opening it resumes it exactly as an offloaded one is resumed.
        Kind::Resumable | Kind::Closed => resume(rows, slot, &n, term, deps, check_room),
        Kind::Starting => Outcome::Refused(starting(&n)),
    }
}

/// What a record says about its slot, read with `/proc` and zmx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A process is there to attach to.
    Running,
    /// Nothing is running, and the record says what to bring back.
    Resumable,
    /// Another menu marked it live a moment ago and its zmx session has not appeared yet.
    Starting,
    Closed,
}

fn classify(rec: &SlotRecord, now: Millis) -> Kind {
    match rec.state {
        State::Closed => Kind::Closed,
        State::Offloaded => Kind::Resumable,
        State::Live | State::Offloading => match (rec.pid, rec.proc_start) {
            // A live record whose process has gone is what `reconcile` turns into `offloaded`
            // after a restart; opening it before reconcile has run means the same thing.
            (Some(pid), Some(start)) => {
                if procinfo::is_alive(pid, start) {
                    Kind::Running
                } else {
                    Kind::Resumable
                }
            }
            // No pid: just started or resumed and not yet bound by `SessionStart`, or running
            // without hooks. zmx's session is the evidence that something is there.
            _ if listed(&rec.slot) == Some(true) => Kind::Running,
            _ if now.saturating_sub(rec.updated_ms) < STARTING_GRACE => Kind::Starting,
            _ => Kind::Resumable,
        },
    }
}

fn starting(n: &str) -> String {
    format!("{n} is being started somewhere else — try again in a moment")
}

fn no_row(_index: usize) -> Outcome {
    Outcome::Refused("that session is no longer listed".to_string())
}

/// How a message names a session: its title as the list draws it, quoted, and cut short
/// enough to leave the message room on a phone. Nothing on the screen is numbered (owner,
/// 2026-10-03).
fn label(row: &Row) -> String {
    const MOST: usize = 24;
    let mut t: String = row.title.chars().take(MOST).collect();
    if row.title.chars().count() > MOST {
        t.push_str("...");
    }
    format!("\"{t}\"")
}

/// A row's label, or the slot's name when it is not on the list — a new slot, or the one a
/// resume found its conversation already running in.
fn label_of(rows: &[Row], slot: &str) -> String {
    rows.iter()
        .find(|r| matches!(&r.key, RowKey::Slot(s) if s == slot))
        .map(label)
        .unwrap_or_else(|| slot.to_string())
}

fn attach_slot(rows: &[Row], slot: &str, term: &mut dyn Terminal, deps: &Deps) -> Outcome {
    let n = label_of(rows, slot);
    let mut cmd = attach_command(slot, deps);
    // zmx's exit status says nothing here — 0 for a detach and for a session that ended while
    // attached alike — so what happened is read afterwards, from the record.
    if let Err(why) = run_child(&mut cmd, term) {
        return Outcome::Refused(why);
    }
    match settle(slot, None, false) {
        After::Running => detached(),
        After::Ended | After::DiedUnbound => ended(&n),
    }
}

/// A session with no record: all there is to go on afterwards is whether zmx still lists it,
/// which it stops doing when its command exits.
fn attach_socket(name: &str, n: &str, term: &mut dyn Terminal, deps: &Deps) -> Outcome {
    let mut cmd = attach_command(name, deps);
    if let Err(why) = run_child(&mut cmd, term) {
        return Outcome::Refused(why);
    }
    if listed(name) != Some(false) {
        detached()
    } else {
        ended(n)
    }
}

/// `zmx attach <name> false`: attach to `name` and never create it.
///
/// **`zmx attach` creates a session that is not there**, where `abduco -a` refused, so a
/// session that ended between the list and the keypress would come back as a stray shell
/// under the slot's name. The command is the guard: zmx ignores it for a session that exists
/// (seen on 0.8.1: same pid before and after), and for one that does not it starts `false`,
/// which ends at once and leaves nothing behind — the attach exits 1, `cannot connect`.
fn attach_command(name: &str, deps: &Deps) -> Command {
    let mut cmd = zmx::command(&deps.zmx);
    cmd.arg("attach").arg(name).arg("false");
    cmd
}

/// Whether zmx lists a session called `name`: `None` when zmx could not be asked, which every
/// caller weighs as "cannot rule it out" in whichever direction is safe.
fn listed(name: &str) -> Option<bool> {
    zmx::session_for(name).map(|s| s.is_some())
}

/// Mockup 6. The cursor is still on the session, so the line need not say which.
fn detached() -> Outcome {
    Outcome::Back(Some("detached · it is still running".to_string()))
}

fn ended(n: &str) -> Outcome {
    Outcome::Back(Some(format!("{n} ended")))
}

/// What a resume decided under the lock, to be carried out once it is released.
enum Plan {
    Start {
        session: String,
        cwd: String,
        /// Where the record goes back to if the start does not take: offloaded or closed,
        /// whichever it was.
        back_to: State,
    },
    /// The conversation is already running in this slot; attach rather than fork it.
    Attach(String),
    Refuse(String),
}

fn resume(
    rows: &[Row],
    slot: &str,
    n: &str,
    term: &mut dyn Terminal,
    deps: &Deps,
    check_room: bool,
) -> Outcome {
    if check_room {
        if let Some(no_room) = room_check(rows, deps) {
            return no_room;
        }
    }
    let title = rows
        .iter()
        .find(|r| matches!(&r.key, RowKey::Slot(s) if s == slot))
        .map_or(slot, |r| r.title.as_str());
    let (session, cwd, back_to) = match plan_resume(slot, n) {
        Plan::Start {
            session,
            cwd,
            back_to,
        } => (session, cwd, back_to),
        Plan::Attach(other) => return attach_slot(rows, &other, term, deps),
        Plan::Refuse(why) => return Outcome::Refused(why),
    };
    let args = ["--resume".to_string(), session.clone()];
    let (took, oom) = match start_slot(slot, &cwd, &args, term, deps) {
        Ok(ran) => ran,
        Err(why) => {
            put_back(slot, back_to);
            return Outcome::Refused(why);
        }
    };
    match settle(slot, Some(back_to), took < deps.quick) {
        After::Running => detached(),
        After::Ended => ended(n),
        // Gone with no SessionStart: claude never got as far as running the conversation.
        // A slow death still means it ran unbound — hooks missing — and ended; only a quick
        // one is the failure mockup 5 describes. Either way the record is offloaded again.
        After::DiedUnbound if took < deps.quick => Outcome::ResumeFailed(Dialog::ResumeFailed {
            title: title.to_string(),
            session: session.chars().take(8).collect(),
            killed_for_memory: oom,
            output: stderr_tail(&stderr_path(slot), STDERR_LINES),
            closed: back_to == State::Closed,
        }),
        After::DiedUnbound => ended(n),
    }
}

/// The resume's decision, under the slot's lock, and the record marked before it is let go.
fn plan_resume(slot: &str, n: &str) -> Plan {
    let _lock = match lock_slot(slot, n) {
        Ok(l) => l,
        Err(why) => return Plan::Refuse(why),
    };
    let now = clock::now();
    let mut rec = match registry::load(slot) {
        Ok(Some(rec)) => rec,
        Ok(None) => return Plan::Refuse(format!("{n} is gone")),
        Err(e) => return Plan::Refuse(format!("{n}: {e}")),
    };
    match classify(&rec, now) {
        Kind::Resumable | Kind::Closed => {}
        // Somebody resumed it between the list being drawn and the lock being taken.
        Kind::Running => return Plan::Attach(slot.to_string()),
        Kind::Starting => return Plan::Refuse(starting(n)),
    }
    let back_to = if rec.state == State::Closed {
        State::Closed
    } else {
        State::Offloaded
    };
    // A zmx session with nothing recorded running in it: a claude the offloader could not
    // stop, or one started by hand under a slot's name. `zmx attach` on a taken name would
    // attach to it and ignore the resume — so refuse rather than call that a resume.
    match listed(slot) {
        Some(false) => {}
        Some(true) => {
            return Plan::Refuse(format!(
                "{n} still has a zmx session with nothing recorded running in it — open it \
                 from its row, or stop it with zmx kill {slot}"
            ));
        }
        None => {
            return Plan::Refuse(format!(
                "{n}: zmx did not answer, so whether it is still running cannot be ruled out"
            ));
        }
    }
    let (Some(session), Some(cwd)) = (rec.session_id.clone(), rec.cwd.clone()) else {
        return Plan::Refuse(format!(
            "{n} has no conversation recorded to resume — close it instead"
        ));
    };
    // A session id is written at SessionStart, the transcript only at the first prompt: a slot
    // stopped before it was ever prompted has the one and not the other, and `--resume` on it
    // fails at once (issue #5). The menu does not list such a slot; this is for one that lost
    // its transcript, or a reading older than the list.
    if !rec.has_conversation() {
        return Plan::Refuse(format!(
            "{n} has no conversation on disk to resume — it was never prompted, or its \
             transcript is gone; close it instead"
        ));
    }
    if !Path::new(&cwd).is_dir() {
        return Plan::Refuse(format!(
            "{n} ran in {cwd}, which is gone — claude finds a conversation by the directory it \
             ran in"
        ));
    }
    // THE RULE: never resume a conversation that is already running. Two processes on one
    // conversation fork it. Checked here, under the lock, because checked before taking it a
    // second menu could pass the same check in the same instant.
    match running_elsewhere(slot, &session, now) {
        Ok(None) => {}
        Ok(Some(Elsewhere::Slot(other))) => return Plan::Attach(other),
        Ok(Some(Elsewhere::Unattachable(why))) => {
            return Plan::Refuse(format!("{n} is not resumed: {why}"));
        }
        // Not being able to look is not the same as nothing being there.
        Err(e) => {
            return Plan::Refuse(format!(
                "{n} is not resumed: could not check whether its conversation is running ({e})"
            ));
        }
    }
    // Live with no pid, written before the lock is released: a second menu now sees it live
    // and attaches or waits, and the offloader holds off (`NoPid`) until `SessionStart` binds
    // the new process. Resumed by the menu, it is one of ours from here on.
    rec.state = State::Live;
    rec.pid = None;
    rec.proc_start = None;
    rec.busy = false;
    rec.needs_you = false;
    rec.registered = true;
    rec.updated_ms = now;
    if let Err(e) = registry::store(&rec) {
        return Plan::Refuse(format!("{n}: {e}"));
    }
    Plan::Start {
        session,
        cwd,
        back_to,
    }
}

/// Where a conversation is already running, if it is.
enum Elsewhere {
    /// In another slot, which can be attached to instead.
    Slot(String),
    /// Somewhere that cannot be attached to, and why.
    Unattachable(String),
}

/// Is `session` running anywhere but `slot`?
///
/// Two sources, because each misses something. Claude Code's own sessions files see every
/// claude, including ones nothing here started — a `claude --resume` typed in a shell — but
/// they are undocumented and could stop being written. The registry sees our slots, including
/// one being started right now that has no process to show yet.
fn running_elsewhere(slot: &str, session: &str, now: Millis) -> io::Result<Option<Elsewhere>> {
    let records = registry::all()?;
    let in_slot = |pid: u32| {
        records.iter().find(|r| {
            r.pid == Some(pid)
                && r.proc_start
                    .is_some_and(|start| procinfo::is_alive(pid, start))
                && listed(&r.slot) == Some(true)
        })
    };
    for running in live::all() {
        if running.session_id.as_deref() != Some(session) {
            continue;
        }
        return Ok(Some(match in_slot(running.pid) {
            Some(r) => Elsewhere::Slot(r.slot.clone()),
            None => Elsewhere::Unattachable(format!(
                "its conversation is already running as pid {}, outside any slot",
                running.pid
            )),
        }));
    }
    for other in &records {
        if other.slot == slot || other.session_id.as_deref() != Some(session) {
            continue;
        }
        match classify(other, now) {
            Kind::Running if listed(&other.slot) == Some(true) => {
                return Ok(Some(Elsewhere::Slot(other.slot.clone())));
            }
            Kind::Running => {
                return Ok(Some(Elsewhere::Unattachable(format!(
                    "its conversation is already running in {}, which has no zmx session \
                     to attach to",
                    other.slot
                ))));
            }
            Kind::Starting => {
                return Ok(Some(Elsewhere::Unattachable(format!(
                    "its conversation is being started in {}",
                    other.slot
                ))));
            }
            Kind::Resumable | Kind::Closed => {}
        }
    }
    Ok(None)
}

// ── resume a conversation from the store ────────────────────────────────────

/// `Enter` on a conversation from Claude Code's own store (`store.rs`): resumed in a slot,
/// from the directory it started in, whoever started it.
///
/// A slot whose record already names the conversation is that slot's to resume, through
/// `resume`, so both ways in to one conversation take the same slot lock and two menus
/// cannot start it twice. Otherwise a new slot is allocated for it, and the never-resume-
/// running check is made under the allocation lock with the record — naming the
/// conversation — written before that lock is let go: a second menu then finds it starting.
fn resume_conversation(
    rows: &[Row],
    (id, cwd, title): (&str, &str, &str),
    n: &str,
    term: &mut dyn Terminal,
    deps: &Deps,
    check_room: bool,
) -> Outcome {
    let now = clock::now();
    let holders: Vec<SlotRecord> = match registry::all() {
        Ok(recs) => recs
            .into_iter()
            .filter(|r| r.session_id.as_deref() == Some(id))
            .collect(),
        Err(e) => return Outcome::Refused(format!("{n}: {e}")),
    };
    if let Some(r) = holders.iter().find(|r| classify(r, now) == Kind::Running) {
        return attach_slot(rows, &r.slot, term, deps);
    }
    if holders.iter().any(|r| classify(r, now) == Kind::Starting) {
        return Outcome::Refused(starting(n));
    }
    if let Some(r) = holders.first() {
        return resume(rows, &r.slot, n, term, deps, check_room);
    }
    if check_room {
        if let Some(no_room) = room_check(rows, deps) {
            return no_room;
        }
    }
    if !Path::new(cwd).is_dir() {
        return Outcome::Refused(format!(
            "{n} ran in {cwd}, which is gone — claude finds a conversation by the directory it \
             ran in"
        ));
    }
    let slot = match allocate_for(cwd, Some((id, title))) {
        Ok(Ok(slot)) => slot,
        Ok(Err(Elsewhere::Slot(other))) => return attach_slot(rows, &other, term, deps),
        Ok(Err(Elsewhere::Unattachable(why))) => {
            return Outcome::Refused(format!("{n} is not resumed: {why}"));
        }
        Err(why) => return Outcome::Refused(why),
    };
    let args = ["--resume".to_string(), id.to_string()];
    let (took, oom) = match start_slot(&slot, cwd, &args, term, deps) {
        Ok(ran) => ran,
        Err(why) => {
            put_back(&slot, State::Closed);
            return Outcome::Refused(why);
        }
    };
    match settle(&slot, Some(State::Closed), took < deps.quick) {
        After::Running => detached(),
        After::Ended => ended(n),
        After::DiedUnbound if took < deps.quick => Outcome::ResumeFailed(Dialog::ResumeFailed {
            title: title.to_string(),
            session: id.chars().take(8).collect(),
            killed_for_memory: oom,
            output: stderr_tail(&stderr_path(&slot), STDERR_LINES),
            closed: true,
        }),
        After::DiedUnbound => ended(n),
    }
}

// ── new ─────────────────────────────────────────────────────────────────────

pub(crate) fn new_session_with(
    rows: &[Row],
    workspace: &str,
    term: &mut dyn Terminal,
    deps: &Deps,
    check_room: bool,
) -> Outcome {
    if check_room {
        if let Some(no_room) = room_check(rows, deps) {
            return no_room;
        }
    }
    if !Path::new(workspace).is_dir() {
        return Outcome::Refused(format!("{workspace} is not a directory"));
    }
    let slot = match allocate(workspace) {
        Ok(slot) => slot,
        Err(why) => return Outcome::Refused(why),
    };
    let (took, oom) = match start_slot(&slot, workspace, &[], term, deps) {
        Ok(ran) => ran,
        Err(why) => {
            put_back(&slot, State::Closed);
            return Outcome::Refused(why);
        }
    };
    // A new slot that dies unbound is closed rather than offloaded: it never had a
    // conversation, so there is nothing for a row to bring back.
    match settle(&slot, Some(State::Closed), took < deps.quick) {
        After::Running => detached(),
        After::Ended => ended("the new session"),
        After::DiedUnbound if took < deps.quick => {
            let said = stderr_tail(&stderr_path(&slot), 1);
            let how = if oom {
                "was killed for memory"
            } else {
                "ended at once"
            };
            Outcome::Refused(format!(
                "{slot} did not start: claude {how}{}",
                said.first().map(|l| format!(": {l}")).unwrap_or_default()
            ))
        }
        After::DiedUnbound => ended(&slot),
    }
}

/// The lowest free `claude-<n>`, with its record written before anyone else can choose it.
pub(crate) fn allocate(workspace: &str) -> Result<String, String> {
    match allocate_for(workspace, None)? {
        Ok(slot) => Ok(slot),
        Err(_) => unreachable!("only a resume checks for a running conversation"),
    }
}

/// The lowest free `claude-<n>` for a slot starting in `cwd`, resuming `(id, title)` if
/// given; `Ok(Err(..))` when that conversation is running somewhere already.
///
/// Free means no record that is live, offloading or offloaded, and no zmx session of that
/// name — a session started by hand as `claude-3` is still `claude-3`. A closed record's name
/// is reused: closed slots are not listed, their conversations are, from Claude Code's own
/// store, under their own ids (owner, 2026-10-03).
///
/// **Under a registry-wide lock**, because two menus pressing `n` at once would otherwise
/// both see `claude-4` free and both start it. The lock is held from the first look to the
/// record being on disk, and the record is written as `live` with no pid — the same shape a
/// resume leaves — so the name is taken the moment the lock is let go. A resume's check that
/// its conversation is not running is made under the same lock, and its record names the
/// conversation, so a second menu resuming it finds it starting.
fn allocate_for(
    cwd: &str,
    resuming: Option<(&str, &str)>,
) -> Result<Result<String, Elsewhere>, String> {
    let _all = SlotLock::acquire(&registry::dir().join("allocate.lock"), INTERACTIVE_WAIT)
        .map_err(|e| match e.kind() {
            io::ErrorKind::TimedOut => {
                "another menu is starting a session — try again in a moment".to_string()
            }
            _ => format!("could not take the allocation lock: {e}"),
        })?;
    if let Some((id, _)) = resuming {
        match running_elsewhere("", id, clock::now()) {
            Ok(None) => {}
            Ok(Some(e)) => return Ok(Err(e)),
            // Not being able to look is not the same as nothing being there.
            Err(e) => {
                return Err(format!(
                    "could not check whether that conversation is running ({e})"
                ));
            }
        }
    }
    let sessions = zmx::sessions().ok_or_else(|| {
        "zmx did not answer, so which slot names are free cannot be told — try again".to_string()
    })?;
    for n in 1..=MAX_SLOTS {
        let slot = format!("claude-{n}");
        if sessions.iter().any(|s| s.name == slot) {
            continue;
        }
        // A late hook could still be writing a closed record, so the slot's own lock too.
        let _lock = lock_slot(&slot, &slot)?;
        match registry::load(&slot) {
            Ok(None) => {}
            Ok(Some(r)) if r.state == State::Closed => {}
            Ok(Some(_)) => continue,
            // A record that cannot be read is not ours to overwrite; doctor reports it.
            Err(_) => continue,
        }
        let now = clock::now();
        let mut rec = SlotRecord::new(&slot, now);
        rec.cwd = Some(cwd.to_string());
        if let Some((id, title)) = resuming {
            rec.session_id = Some(id.to_string());
            // Shown on the row until the resumed conversation's own hooks read its titles.
            rec.ai_title = Some(title.to_string());
        }
        registry::store(&rec).map_err(|e| format!("{slot}: {e}"))?;
        return Ok(Ok(slot));
    }
    Err(format!("no free slot name up to claude-{MAX_SLOTS}"))
}

// ── offload, then open ──────────────────────────────────────────────────────

pub(crate) fn offload_then_open_with(
    rows: &[Row],
    victim: usize,
    then: Option<usize>,
    workspace: &str,
    term: &mut dyn Terminal,
    deps: &Deps,
) -> Outcome {
    let Some(row) = rows.get(victim) else {
        return no_row(victim);
    };
    let n = label(row);
    let RowKey::Slot(slot) = &row.key else {
        return Outcome::Refused(format!("{n} was not started here and cannot be offloaded"));
    };
    {
        let _lock = match lock_slot(slot, &n) {
            Ok(l) => l,
            Err(why) => return Outcome::Refused(why),
        };
        let mut rec = match registry::load(slot) {
            Ok(Some(rec)) => rec,
            Ok(None) => return Outcome::Refused(format!("{n} is gone")),
            Err(e) => return Outcome::Refused(format!("{n}: {e}")),
        };
        // The offer was made from a reading taken before the dialog went up, and the owner
        // may have taken a while to answer. The offloader's own rule, asked again under the
        // lock, is the only thing that may stop a slot.
        let table = procinfo::table();
        let seen = offload::look(&rec, table.as_deref());
        let verdict = match offload::judge(&rec, clock::now(), &seen) {
            Ok(verdict) => verdict,
            Err(hold) => {
                return Outcome::Refused(format!("{n} can no longer be offloaded: {hold}"));
            }
        };
        // Offered only with a conversation on disk (`offer`); if that went in the meantime, a
        // stop is a close, as the offloader's would be.
        match offload::stop_quiet(&mut rec, verdict, table.as_deref()) {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => return Outcome::Refused(why),
            Err(e) => return Outcome::Refused(format!("{n}: {e}")),
        }
    }
    // No second room check. The owner has just accepted the trade, and the cgroup's figure
    // counts page cache that is not handed back the instant a process exits, so asking again
    // could answer no for the very room just made — and offer to stop another slot.
    match then {
        Some(index) => open_with(rows, index, term, deps, false),
        None => new_session_with(rows, workspace, term, deps, false),
    }
}

/// Mockup 4, when there is no room: the memory figures and the idlest slot the offloader
/// would stop, if there is one on the list.
fn room_check(rows: &[Row], deps: &Deps) -> Option<Outcome> {
    let m = (deps.memory)();
    // Unknown headroom answers yes (`mem.rs`): an unreadable cgroup file must not be worse
    // than a full container.
    if m.room_for(mem::SESSION_COST) {
        return None;
    }
    Some(Outcome::NoRoom(Dialog::NoRoom {
        used: m.current.unwrap_or(0),
        limit: m.limit.unwrap_or(0),
        want: mem::SESSION_COST,
        offer: offer(rows),
    }))
}

/// The slot the offloader would offload that has been idle longest, as its index in the rows,
/// idle age and title as drawn. A suggestion only: accepting it decides again under the lock.
/// One the offloader would close instead — never prompted, nothing on disk — is not offered:
/// the dialog promises the offered session is resumable from disk.
fn offer(rows: &[Row]) -> Option<(usize, String, String)> {
    let now = clock::now();
    let table = procinfo::table();
    registry::all()
        .ok()?
        .iter()
        .filter(|r| matches!(r.state, State::Live | State::Offloading))
        .filter_map(|r| {
            let verdict = offload::judge(r, now, &offload::look(r, table.as_deref())).ok()?;
            let offload::Verdict::Offload { idle } = verdict else {
                return None;
            };
            let index = rows
                .iter()
                .position(|row| matches!(&row.key, RowKey::Slot(s) if *s == r.slot))?;
            Some((idle, index))
        })
        .max_by_key(|(idle, _)| *idle)
        .map(|(idle, index)| (index, fmt::age(idle), rows[index].title.clone()))
}

// ── shell ───────────────────────────────────────────────────────────────────

pub(crate) fn shell_with(workspace: &str, term: &mut dyn Terminal, deps: &Deps) -> Outcome {
    let mut cmd = Command::new(&deps.shell);
    cmd.arg("-l").current_dir(workspace);
    match run_child(&mut cmd, term) {
        Ok(_) => Outcome::Back(None),
        Err(why) => Outcome::Refused(why),
    }
}

// ── starting a slot, and what came of it ────────────────────────────────────

/// `<registry>/<slot>.stderr`: what the slot's claude said on stderr, for mockup 5.
fn stderr_path(slot: &str) -> PathBuf {
    registry::dir().join(format!("{slot}.stderr"))
}

/// The command line a slot is started with:
///
/// `zmx attach <slot> env CLAUDE_SESSIONS_SLOT=<slot> [SCROLLBACK=1…] sh -c START_WRAP sh <stderr> <claude> <args…>`
///
/// `env` sets the name every hook inherits, and each of `SCROLLBACK` the login has not set,
/// and execs the shell; the shell points stderr at the capture file and execs claude: one pid
/// from the zmx daemon's fork to claude, so claude is that daemon's direct child. The slot
/// name is free when this runs (`allocate_for`, `plan_resume`): on a taken name zmx would
/// attach to what is there and ignore the command.
///
/// **It creates and attaches in one, on the terminal.** What claude writes in the first few
/// milliseconds, before the client has connected, is not shown on that first attach, but zmx
/// keeps it and replays it on the next one. Creating the session from a client with no terminal
/// and then attaching was tried and is worse: that early output was lost outright (zmx 0.8.1,
/// 2026-10-06). Claude draws nothing for far longer than the gap, so in practice nothing is
/// missed; `tests/zmx_real.rs` pins both halves.
fn start_command(slot: &str, stderr: &Path, args: &[String], deps: &Deps) -> Command {
    let mut cmd = zmx::command(&deps.zmx);
    cmd.arg("attach")
        .arg(slot)
        .arg("env")
        .arg(format!("CLAUDE_SESSIONS_SLOT={slot}"))
        .args(
            SCROLLBACK
                .iter()
                .filter(|key| !(deps.preset)(key))
                .map(|key| format!("{key}=1")),
        )
        .arg("sh")
        .arg("-c")
        .arg(START_WRAP)
        .arg("sh")
        .arg(stderr)
        .arg(&deps.claude)
        .args(args);
    cmd
}

/// Start `slot` in `cwd` and wait for the owner to come back from it. Returns how long it took,
/// which with whether a `SessionStart` bound it is how a failed start is told from a session
/// that was used and left, and whether the container's OOM-kill count rose meanwhile. zmx's exit status is no help: 0 for a detach and for a claude that
/// ended, whatever claude's own status, and 1 for a claude gone before the client connected.
fn start_slot(
    slot: &str,
    cwd: &str,
    args: &[String],
    term: &mut dyn Terminal,
    deps: &Deps,
) -> Result<(Duration, bool), String> {
    let stderr = stderr_path(slot);
    if let Some(dir) = stderr.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // Truncated: the dialog must show what this start said, not what the last one did.
    std::fs::File::create(&stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;
    let mut cmd = start_command(slot, &stderr, args, deps);
    cmd.current_dir(cwd);
    let oom_before = (deps.oom_kills)();
    let started = Instant::now();
    run_child(&mut cmd, term)?;
    let took = started.elapsed();
    // Only an answer both times counts: an unreadable file is "not known", never "killed".
    let killed_for_memory = matches!(
        (oom_before, (deps.oom_kills)()),
        (Some(before), Some(after)) if after > before
    );
    Ok((took, killed_for_memory))
}

/// Run a child with the terminal handed over, and take the terminal back whatever happened.
fn run_child(cmd: &mut Command, term: &mut dyn Terminal) -> Result<ExitStatus, String> {
    term.suspend()
        .map_err(|e| format!("could not hand the terminal over: {e}"))?;
    let status = cmd.status();
    // A failure to take the terminal back is not reported. The menu repaints the whole
    // screen on its next frame, which is the only repair on offer, and turning it into
    // `Refused` would claim nothing was done when the child did run.
    let _ = term.resume();
    status.map_err(|e| format!("could not run {}: {e}", cmd.get_program().to_string_lossy()))
}

/// What a slot looks like once the child that had it on screen has returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Running,
    /// Gone, having been bound — it ran, and ended.
    Ended,
    /// Gone, and no `SessionStart` ever bound a pid to it.
    DiedUnbound,
}

/// Read the slot again under its lock, and record that the owner has seen it.
///
/// `last_attach_ms` is written here and nowhere else: it is the one fact about the owner
/// rather than the session, and it is what clears `unread`. It is set when the owner comes
/// back, not when they leave, because everything that happened while they were attached they
/// saw.
///
/// `unbound_death` is the state a start that died without ever being bound goes back to, and
/// `just_started` says the child returned quickly enough for that to be the explanation, so
/// the socket is given a moment to go.
fn settle(slot: &str, unbound_death: Option<State>, just_started: bool) -> After {
    if just_started
        && registry::load(slot)
            .ok()
            .flatten()
            .is_some_and(|r| r.pid.is_none())
    {
        wait_session_gone(slot, SESSION_SETTLE);
    }
    let lock = SlotLock::acquire(&registry::lock_path(slot), INTERACTIVE_WAIT).ok();
    let Ok(Some(mut rec)) = registry::load(slot) else {
        return if listed(slot) != Some(false) {
            After::Running
        } else {
            After::Ended
        };
    };
    let running = is_running(&rec);
    let bound = rec.pid.is_some();
    // Without the lock nothing may be written, so a start that died unbound cannot be put
    // back; it reads as ended, and `classify` treats the record as a dead start once
    // `STARTING_GRACE` has passed.
    if lock.is_none() {
        return if running {
            After::Running
        } else {
            After::Ended
        };
    }
    if !running && !bound {
        if let Some(state) = unbound_death {
            rec.state = state;
            rec.busy = false;
            rec.updated_ms = clock::now();
            let _ = registry::store(&rec);
            return After::DiedUnbound;
        }
    }
    rec.last_attach_ms = clock::now();
    let _ = registry::store(&rec);
    if running {
        After::Running
    } else {
        After::Ended
    }
}

/// Is something running in this slot? The recorded process when there is one; otherwise zmx's
/// session, which is all a slot not yet bound, or running without hooks, has to show. A zmx
/// that does not answer reads as running: the safe mistake, since the other one puts a live
/// slot back to offloaded.
fn is_running(rec: &SlotRecord) -> bool {
    match (rec.pid, rec.proc_start) {
        (Some(pid), Some(start)) => procinfo::is_alive(pid, start),
        _ => listed(&rec.slot) != Some(false),
    }
}

fn wait_session_gone(slot: &str, within: Duration) {
    let deadline = Instant::now() + within;
    while listed(slot) != Some(false) && Instant::now() < deadline {
        sleep(Duration::from_millis(20));
    }
}

/// A start that never got going: back to `state`, if nothing has bound it meanwhile.
fn put_back(slot: &str, state: State) {
    let Ok(_lock) = SlotLock::acquire(&registry::lock_path(slot), INTERACTIVE_WAIT) else {
        return;
    };
    if let Ok(Some(mut rec)) = registry::load(slot) {
        if rec.pid.is_none() && rec.state == State::Live {
            rec.state = state;
            rec.updated_ms = clock::now();
            let _ = registry::store(&rec);
        }
    }
}

fn lock_slot(slot: &str, n: &str) -> Result<SlotLock, String> {
    SlotLock::acquire(&registry::lock_path(slot), INTERACTIVE_WAIT).map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => {
            format!("{n} is busy — something else is acting on it; try again")
        }
        _ => format!("{n}: {e}"),
    })
}

/// The last `max` non-blank lines of a capture file, made safe to draw.
///
/// Claude colours what it prints, and an escape sequence handed to the renderer would be
/// measured as text and then obeyed by the terminal — knocking the dialog's frame out of line
/// at best. Escape sequences and control characters are dropped; what is left is text.
fn stderr_tail(path: &Path, max: usize) -> Vec<String> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&raw);
    let lines: Vec<String> = text
        .lines()
        .map(printable)
        .filter(|l| !l.is_empty())
        .collect();
    lines[lines.len().saturating_sub(max)..].to_vec()
}

fn printable(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ parameters, ended by a byte in @..~. Anything else after ESC is a
            // two-character sequence. Both are dropped whole.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if c == '\t' {
            out.push(' ');
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    //! Against stand-in programs: a `zmx` shell script that keeps a file per session, lists
    //! them as `zmx list` does and runs its command, and `claude` scripts that either stay up or fail the way a
    //! resume of a missing transcript does. Real processes, real signals, the real registry.
    //!
    //! **Why a mutex rather than explicit paths.** The registry, the zmx program to list with and
    //! Claude Code's sessions directory are each found from an environment variable by the
    //! module that owns them, and launch calls straight through to those modules — threading
    //! a path through every one of them would change four modules to suit a test. So each
    //! test here takes `ENV` and points the three variables at its own temporary tree. No
    //! test outside this module reads those variables (checked when this was written; the
    //! integration tests run in their own processes), so the mutex covers every reader.
    //! The programs to start and attach with are passed in `Deps`; the one to list with is
    //! `CLAUDE_SESSIONS_ZMX`, pointed at the same stand-in.

    use super::*;
    use crate::ui::Row;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Stdio};
    use std::sync::{Barrier, Mutex, MutexGuard};

    static ENV: Mutex<()> = Mutex::new(());

    struct Fixture {
        root: PathBuf,
        kids: Vec<Child>,
        _env: MutexGuard<'static, ()>,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = std::env::temp_dir().join(format!("cs-launch-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for d in ["registry", "zmx", "config/sessions", "bin", "work"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            // SAFETY: every reader of these variables in this test binary holds ENV (see the
            // module note), and std serialises its own environment access.
            unsafe {
                std::env::set_var("CLAUDE_SESSIONS_DIR", root.join("registry"));
                std::env::set_var("CLAUDE_SESSIONS_ZMX", root.join("bin/zmx"));
                std::env::set_var("CLAUDE_CONFIG_DIR", root.join("config"));
            }
            let f = Fixture {
                root,
                kids: Vec::new(),
                _env: guard,
            };
            // Listing works from the start, before a test chooses how attaching behaves.
            f.zmx(":");
            f
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn work(&self) -> String {
            self.path("work").display().to_string()
        }

        fn script(&self, name: &str, body: &str) -> String {
            let p = self.path(&format!("bin/{name}"));
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.display().to_string()
        }

        /// The stand-in zmx, at `bin/zmx`, which is also what `CLAUDE_SESSIONS_ZMX` names.
        /// Every attach is appended to `calls` (a listing is not), which is also how the fake
        /// terminal tells whether a child ran between its suspend and its resume.
        ///
        /// A session is a file `zmx/<name>` holding `<pid> <clients>`, and `list` prints one
        /// line per file the way zmx 0.8.1 does. `attach <name> <command…>` on a name that is
        /// not there creates the session as zmx's daemon does — a forked copy of itself, still
        /// named `zmx`, runs the command and removes the file when it exits — and then is the
        /// attached client: it runs `on_attach` and returns 0. `attach <name> false` on a session that is there runs
        /// `on_attach` — the owner's time attached — and returns 0, as zmx does whether the
        /// owner detached or the session ended meanwhile. On a session that is not there it
        /// exits 1, starting nothing.
        fn zmx(&self, on_attach: &str) -> String {
            let calls = self.path("calls");
            let dir = self.path("zmx");
            self.script(
                "zmx",
                &format!(
                    r#"dir="{dir}"
case "$1" in
  list)
    for f in "$dir"/*; do
      [ -e "$f" ] || continue
      read pid clients < "$f"
      printf 'name=%s\tpid=%s\tclients=%s\tcreated=0\n' "${{f##*/}}" "$pid" "$clients"
    done
    exit 0 ;;
  attach)
    echo "zmx $*" >> "{calls}"
    name=$2; shift 2
    if [ "$1" = false ]; then
      [ -e "$dir/$name" ] || exit 1
      {on_attach}
      exit 0
    fi
    [ -e "$dir/$name" ] && exit 0
    echo "0 0" > "$dir/$name"
    ( "$@" </dev/null >/dev/null 2>&1 & p=$!; echo "$p 0" > "$dir/$name"; wait $p; rm -f "$dir/$name" ) &
    {on_attach}
    exit 0 ;;
esac
exit 2"#,
                    calls = calls.display(),
                    dir = dir.display(),
                ),
            )
        }

        /// A claude that stays up: it records its pid, directory and arguments, and the
        /// scrollback variables it was given, writes a line to stderr, and becomes a long sleep
        /// under the same pid.
        fn claude_ok(&self) -> String {
            self.script(
                "claude",
                &format!(
                    r#"echo "$$ $PWD $CLAUDE_SESSIONS_SLOT $*" >> "{}"
echo "${{CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN-unset}} ${{CLAUDE_CODE_DISABLE_MOUSE-unset}} ${{CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL-unset}}" >> "{}"
echo "a warning on stderr" >&2
exec sleep 600"#,
                    self.path("claude.log").display(),
                    self.path("claude.env").display()
                ),
            )
        }

        fn claude_env(&self) -> Vec<String> {
            std::fs::read_to_string(self.path("claude.env"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// A claude that fails as `--resume` of a missing transcript does.
        fn claude_fails(&self) -> String {
            self.script(
                "claude",
                &format!(
                    r#"echo "$$ $PWD $CLAUDE_SESSIONS_SLOT $*" >> "{}"
printf '\033[31mNo conversation found with that session id\033[0m\n' >&2
exit 1"#,
                    self.path("claude.log").display()
                ),
            )
        }

        fn deps(&self, zmx: String, claude: String) -> Deps {
            Deps {
                zmx,
                claude,
                shell: "/bin/false".into(),
                memory: room,
                oom_kills: || Some(0),
                quick: QUICK_FAIL,
                preset: |_| false,
            }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.path("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn claude_log(&self) -> Vec<String> {
            std::fs::read_to_string(self.path("claude.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// A process to stand for a slot's claude, with its real start time.
        fn process(&mut self) -> (u32, u64) {
            let child = Command::new("sleep")
                .arg("600")
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id();
            self.kids.push(child);
            (pid, procinfo::start_time(pid).unwrap())
        }

        /// A zmx session that nothing in the test started, attached or not.
        fn session(&self, slot: &str, attached: bool) {
            let p = self.path(&format!("zmx/{slot}"));
            std::fs::write(&p, format!("0 {}\n", u8::from(attached))).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for line in self.claude_log() {
                if let Some(pid) = line.split_whitespace().next() {
                    let _ = Command::new("kill")
                        .args(["-9", pid])
                        .stderr(Stdio::null())
                        .status();
                }
            }
            for kid in &mut self.kids {
                let _ = kid.kill();
                let _ = kid.wait();
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn room() -> mem::Memory {
        mem::Memory {
            limit: Some(4 << 30),
            current: Some(1 << 30),
        }
    }
    fn full() -> mem::Memory {
        mem::Memory {
            limit: Some(1 << 30),
            current: Some((1 << 30) - (100 << 20)),
        }
    }
    fn unknown() -> mem::Memory {
        mem::Memory {
            limit: None,
            current: Some(1 << 30),
        }
    }

    /// Counts suspends and resumes, and records how many children had run at each, read from
    /// the stand-ins' shared call log.
    struct Term {
        calls: PathBuf,
        events: Vec<(&'static str, usize)>,
    }

    impl Term {
        fn new(f: &Fixture) -> Term {
            Term {
                calls: f.path("calls"),
                events: Vec::new(),
            }
        }
        fn ran(&self) -> usize {
            std::fs::read_to_string(&self.calls)
                .unwrap_or_default()
                .lines()
                .count()
        }
        /// Balanced, in order, and a child ran inside every suspend — never before it.
        fn assert_handed_over(&self, times: usize) {
            assert_eq!(self.events.len(), times * 2, "events: {:?}", self.events);
            for pair in self.events.chunks(2) {
                let [(s, before), (r, after)] = pair else {
                    unreachable!()
                };
                assert_eq!((*s, *r), ("suspend", "resume"), "{:?}", self.events);
                assert!(
                    after > before,
                    "a child must run while suspended: {:?}",
                    self.events
                );
            }
        }
    }

    impl Terminal for Term {
        fn suspend(&mut self) -> io::Result<()> {
            let n = self.ran();
            self.events.push(("suspend", n));
            Ok(())
        }
        fn resume(&mut self) -> io::Result<()> {
            let n = self.ran();
            self.events.push(("resume", n));
            Ok(())
        }
    }

    /// A transcript with an exchange in it: a conversation on disk to resume.
    const PROMPTED: &str = r#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;

    fn row(slot: &str, title: &str) -> Row {
        Row {
            key: RowKey::Slot(slot.into()),
            wants_you: false,
            unread: false,
            busy: false,
            attached: false,
            offloaded: false,
            closed: false,
            archived: false,
            title: title.into(),
            age: "now".into(),
        }
    }

    fn live(slot: &str, pid: u32, start: u64) -> SlotRecord {
        let mut r = SlotRecord::new(slot, clock::now());
        r.pid = Some(pid);
        r.proc_start = Some(start);
        r.session_id = Some(format!("conv-of-{slot}"));
        r.cwd = Some("/".into());
        r
    }

    /// An offloaded slot with its conversation on disk.
    fn offloaded(slot: &str, session: &str, cwd: &str) -> SlotRecord {
        let mut r = SlotRecord::new(slot, clock::now() - 3_600_000);
        r.state = State::Offloaded;
        r.session_id = Some(session.into());
        r.cwd = Some(cwd.into());
        // Beside the test's other files, never in `cwd`, which may be `/`.
        let dir = std::env::temp_dir().join(format!("cs-launch-conv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join(format!("{slot}-{session}.jsonl"));
        std::fs::write(&transcript, PROMPTED).unwrap();
        r.transcript_path = Some(transcript.display().to_string());
        r
    }

    /// An idle, detached slot the offloader would stop: stopped eleven minutes ago.
    /// A live slot idle past the threshold, detached, with its conversation on disk.
    fn idle(f: &mut Fixture, slot: &str) -> (u32, u64) {
        let (pid, start) = f.process();
        let mut r = live(slot, pid, start);
        let transcript = Path::new(&f.work()).join(format!("conv-of-{slot}.jsonl"));
        std::fs::write(&transcript, PROMPTED).unwrap();
        r.transcript_path = Some(transcript.display().to_string());
        let stop = clock::now() - 11 * 60 * 1000;
        r.last_stop_ms = Some(stop);
        r.last_activity_ms = stop;
        registry::store(&r).unwrap();
        f.session(slot, false);
        (pid, start)
    }

    fn load(slot: &str) -> SlotRecord {
        registry::load(slot).unwrap().expect("a record")
    }

    fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            sleep(Duration::from_millis(10));
        }
    }

    // ── attach ──────────────────────────────────────────────────────────────

    #[test]
    fn attaching_records_the_look_and_clears_unread() {
        let mut f = Fixture::new("attach");
        let (pid, start) = f.process();
        let mut rec = live("claude-1", pid, start);
        rec.last_stop_ms = Some(clock::now() - 1_000);
        registry::store(&rec).unwrap();
        f.session("claude-1", false);
        assert!(load("claude-1").unread(), "calibration: it starts unread");

        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [row("claude-1", "a"), row("claude-2", "b")];
        let mut term = Term::new(&f);
        let before = clock::now();
        let out = open_with(&rows, 0, &mut term, &deps, true);

        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        assert_eq!(f.calls(), ["zmx attach claude-1 false"]);
        term.assert_handed_over(1);
        let after = load("claude-1");
        assert!(after.last_attach_ms >= before, "the look is recorded");
        assert!(!after.unread(), "and that is what clears unread");
        assert_eq!(after.state, State::Live, "attaching changes nothing else");
    }

    #[test]
    fn a_slot_that_ends_while_attached_says_so() {
        let mut f = Fixture::new("attach-ends");
        let (pid, start) = f.process();
        registry::store(&live("claude-1", pid, start)).unwrap();
        f.session("claude-1", false);
        // The owner /exits while attached: the process is gone by the time zmx returns.
        let deps = f.deps(f.zmx(&format!("kill {pid}; sleep 0.2")), f.claude_ok());
        let rows = [row("claude-1", "a")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(out, Outcome::Back(Some("\"a\" ended".into())));
        term.assert_handed_over(1);
    }

    #[test]
    fn an_unregistered_row_is_attached_by_name() {
        let f = Fixture::new("socket-row");
        f.session("claude", false);
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let mut rows = vec![row("x", "claude")];
        rows[0].key = RowKey::Socket("claude".into());
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        assert_eq!(f.calls(), ["zmx attach claude false"]);
        term.assert_handed_over(1);
    }

    // ── resume ──────────────────────────────────────────────────────────────

    #[test]
    fn an_offloaded_slot_is_resumed_in_its_own_directory_as_zmxs_direct_child() {
        let f = Fixture::new("resume");
        registry::store(&offloaded("claude-1", "conv-1", &f.work())).unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [row("claude-1", "a")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);

        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        term.assert_handed_over(1);
        let started = wait_for("the stand-in claude", || f.claude_log().pop());
        let fields: Vec<&str> = started.split_whitespace().collect();
        assert_eq!(
            &fields[1..],
            [&f.work(), "claude-1", "--resume", "conv-1"],
            "in the conversation's directory, with the slot named in its environment"
        );
        let pid: u32 = fields[0].parse().unwrap();
        let parent = procinfo::parent(pid).and_then(procinfo::comm);
        assert_eq!(
            parent.as_deref(),
            Some("zmx"),
            "no shell may stay between the server and claude, or no hook would ever bind it"
        );
        let rec = load("claude-1");
        assert_eq!(rec.state, State::Live);
        assert_eq!(rec.pid, None, "unbound until SessionStart");
        assert!(rec.last_attach_ms > 0);
        let captured = std::fs::read_to_string(stderr_path("claude-1")).unwrap();
        assert_eq!(captured, "a warning on stderr\n");
        let env = wait_for("its environment", || f.claude_env().pop());
        assert_eq!(
            env, "1 1 1",
            "a resume gets the terminal's scrollback as a new one does"
        );
    }

    #[test]
    fn a_conversation_already_running_is_never_resumed() {
        let mut f = Fixture::new("running");
        registry::store(&offloaded("claude-1", "conv-1", &f.work())).unwrap();
        // Claude Code's own record of a live session on that conversation, in no slot: a
        // `claude --resume` somebody typed in a shell.
        let (pid, start) = f.process();
        let file = f.path(&format!("config/sessions/{pid}.json"));
        std::fs::write(
            &file,
            format!(
                r#"{{"pid":{pid},"sessionId":"conv-1","procStart":{start},"kind":"interactive"}}"#
            ),
        )
        .unwrap();

        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [row("claude-1", "a")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        match &out {
            Outcome::Refused(why) => assert!(why.contains(&format!("pid {pid}")), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(f.calls().is_empty(), "nothing was started");
        assert!(term.events.is_empty(), "the terminal was never handed over");
        assert_eq!(load("claude-1").state, State::Offloaded);

        // Calibration: the same slot, with that session gone, resumes.
        std::fs::remove_file(&file).unwrap();
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        let calls = f.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("--resume conv-1"), "{calls:?}");
    }

    #[test]
    fn a_conversation_running_in_another_slot_is_attached_instead() {
        let mut f = Fixture::new("elsewhere");
        registry::store(&offloaded("claude-1", "conv-1", &f.work())).unwrap();
        let (pid, start) = f.process();
        let mut other = live("claude-2", pid, start);
        other.session_id = Some("conv-1".into());
        registry::store(&other).unwrap();
        f.session("claude-2", false);

        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [row("claude-1", "a"), row("claude-2", "b")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        assert_eq!(
            f.calls(),
            ["zmx attach claude-2 false"],
            "attached, not resumed"
        );
        assert_eq!(load("claude-1").state, State::Offloaded);
        assert!(load("claude-2").last_attach_ms > 0);
    }

    fn stored(id: &str, cwd: &str, title: &str) -> Row {
        Row {
            key: RowKey::Conversation {
                id: id.into(),
                cwd: cwd.into(),
            },
            closed: true,
            ..row("unused", title)
        }
    }

    #[test]
    fn a_stored_conversation_is_resumed_in_a_new_slot_from_where_it_started() {
        // Owner, 2026-10-03: any conversation on disk, whoever started it, is one Enter away.
        let f = Fixture::new("stored");
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [stored("conv-9", &f.work(), "fix the dns")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        let started = wait_for("the stand-in claude", || f.claude_log().pop());
        let fields: Vec<&str> = started.split_whitespace().collect();
        assert_eq!(
            &fields[1..],
            [&f.work(), "claude-1", "--resume", "conv-9"],
            "a new slot, in the directory the conversation started in"
        );
        let rec = load("claude-1");
        assert_eq!(
            rec.session_id.as_deref(),
            Some("conv-9"),
            "the record names it"
        );
        assert_eq!(rec.cwd.as_deref(), Some(f.work().as_str()));
        assert_eq!(
            rec.display_title(),
            "fix the dns",
            "titled before its hooks run"
        );
    }

    #[test]
    fn a_stored_conversation_already_running_is_never_resumed() {
        let mut f = Fixture::new("stored-running");
        // A `claude --resume` somebody typed in a shell, in no slot.
        let (pid, start) = f.process();
        let file = f.path(&format!("config/sessions/{pid}.json"));
        std::fs::write(
            &file,
            format!(r#"{{"pid":{pid},"sessionId":"conv-9","procStart":{start}}}"#),
        )
        .unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [stored("conv-9", &f.work(), "t")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert!(
            matches!(&out, Outcome::Refused(why) if why.contains(&format!("pid {pid}"))),
            "{out:?}"
        );
        assert!(f.calls().is_empty(), "nothing was started");
        assert!(
            registry::load("claude-1").unwrap().is_none(),
            "no slot taken"
        );
        // Calibration: with that session gone, it resumes.
        std::fs::remove_file(&file).unwrap();
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
    }

    #[test]
    fn a_stored_conversation_a_slot_already_holds_goes_through_that_slot() {
        // Both ways in to one conversation take the same slot lock.
        let f = Fixture::new("stored-held");
        registry::store(&offloaded("claude-4", "conv-9", &f.work())).unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [stored("conv-9", &f.work(), "t")];
        let mut term = Term::new(&f);
        open_with(&rows, 0, &mut term, &deps, true);
        let started = wait_for("the stand-in claude", || f.claude_log().pop());
        assert!(started.contains(" claude-4 --resume conv-9"), "{started}");
        assert!(registry::load("claude-1").unwrap().is_none(), "no new slot");
    }

    #[test]
    fn a_stored_conversation_that_fails_to_resume_leaves_its_slot_closed() {
        let f = Fixture::new("stored-fails");
        let deps = f.deps(f.zmx(":"), f.claude_fails());
        let rows = [stored("0f9c4a1e-7d", &f.work(), "fix the dns")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::ResumeFailed(Dialog::ResumeFailed {
                title: "fix the dns".into(),
                session: "0f9c4a1e".into(),
                killed_for_memory: false,
                output: vec!["No conversation found with that session id".into()],
                closed: true,
            })
        );
        assert_eq!(load("claude-1").state, State::Closed);
    }

    #[test]
    fn a_leftover_socket_blocks_a_resume_rather_than_reading_as_one() {
        // The calibrating case is the resume test above: the same record, no socket, resumes.
        let f = Fixture::new("leftover");
        registry::store(&offloaded("claude-1", "conv-1", &f.work())).unwrap();
        f.session("claude-1", true);
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let mut term = Term::new(&f);
        let out = open_with(&[row("claude-1", "a")], 0, &mut term, &deps, true);
        assert!(
            matches!(&out, Outcome::Refused(why) if why.contains("zmx kill")),
            "{out:?}"
        );
        assert!(f.calls().is_empty() && term.events.is_empty());
        assert_eq!(load("claude-1").state, State::Offloaded);
    }

    #[test]
    fn a_resume_killed_for_memory_says_so() {
        // The calibrating case is the test below: the same failure, with the count unchanged,
        // says only that it ended.
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3);
        fn rising() -> Option<u64> {
            Some(COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
        }
        let f = Fixture::new("resume-oom");
        registry::store(&offloaded("claude-5", "0f9c4a1e-7d", &f.work())).unwrap();
        let mut deps = f.deps(f.zmx(":"), f.claude_fails());
        deps.oom_kills = rising;
        let rows: Vec<Row> = (1..=5).map(|n| row(&format!("claude-{n}"), "t")).collect();
        let mut term = Term::new(&f);
        match open_with(&rows, 4, &mut term, &deps, true) {
            Outcome::ResumeFailed(Dialog::ResumeFailed {
                killed_for_memory, ..
            }) => assert!(killed_for_memory),
            other => panic!("{other:?}"),
        }
        // A count that cannot be read is not a kill.
        deps.oom_kills = || None;
        let mut term = Term::new(&f);
        match open_with(&rows, 4, &mut term, &deps, true) {
            Outcome::ResumeFailed(Dialog::ResumeFailed {
                killed_for_memory, ..
            }) => assert!(!killed_for_memory),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_resume_says_why_and_leaves_the_slot_offloaded() {
        let f = Fixture::new("resume-fails");
        registry::store(&offloaded("claude-5", "0f9c4a1e-7d", &f.work())).unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_fails());
        let rows: Vec<Row> = (1..=5).map(|n| row(&format!("claude-{n}"), "t")).collect();
        let mut term = Term::new(&f);
        let out = open_with(&rows, 4, &mut term, &deps, true);

        assert_eq!(
            out,
            Outcome::ResumeFailed(Dialog::ResumeFailed {
                title: "t".into(),
                session: "0f9c4a1e".into(),
                killed_for_memory: false,
                output: vec!["No conversation found with that session id".into()],
                closed: false,
            })
        );
        term.assert_handed_over(1);
        let rec = load("claude-5");
        assert_eq!(
            rec.state,
            State::Offloaded,
            "left offloaded, to retry or close"
        );
        assert_eq!(
            rec.session_id.as_deref(),
            Some("0f9c4a1e-7d"),
            "nothing lost"
        );
        assert_eq!(rec.pid, None);
        assert_eq!(rec.last_attach_ms, 0, "a failed start is not a look");
        assert!(listed("claude-5") == Some(false));
    }

    #[test]
    fn a_closed_slot_is_resumed_like_an_offloaded_one() {
        // Closed slots are listed so their conversations can be reached again (owner,
        // 2026-10-03). Calibration first: a closed slot resumes.
        let f = Fixture::new("resume-closed");
        let mut closed = offloaded("claude-1", "conv-1", &f.work());
        closed.state = State::Closed;
        registry::store(&closed).unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [row("claude-1", "a")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        let started = wait_for("the stand-in claude", || f.claude_log().pop());
        assert!(started.ends_with("--resume conv-1"), "got {started}");
        assert_eq!(load("claude-1").state, State::Live);
    }

    #[test]
    fn a_closed_slot_that_fails_to_resume_goes_back_to_closed() {
        // Back to where it came from — closed — not to offloaded. (A separate test: fixtures
        // hold the lock that serialises the launcher's environment-reading tests, so two in
        // one test would wait on each other forever.)
        let f = Fixture::new("resume-closed-fails");
        let mut closed = offloaded("claude-2", "0f9c4a1e-7d", &f.work());
        closed.state = State::Closed;
        registry::store(&closed).unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_fails());
        let rows = [row("claude-1", "a"), row("claude-2", "b")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 1, &mut term, &deps, true);
        assert!(matches!(out, Outcome::ResumeFailed(_)), "got {out:?}");
        assert_eq!(load("claude-2").state, State::Closed);
    }

    #[test]
    fn a_resume_with_no_conversation_on_disk_is_refused_before_anything_runs() {
        // Issue #5: a session id with no transcript — never prompted, or the transcript is
        // gone. `--resume` on it exits at once, so it is not tried.
        let f = Fixture::new("resume-no-conversation");
        let rec = offloaded("claude-1", "conv-1", &f.work());
        std::fs::remove_file(rec.transcript_path.as_ref().unwrap()).unwrap();
        registry::store(&rec).unwrap();
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let rows = [row("claude-1", "a")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert!(
            matches!(&out, Outcome::Refused(why) if why.contains("no conversation on disk")),
            "{out:?}"
        );
        term.assert_handed_over(0);
        assert_eq!(load("claude-1").state, State::Offloaded, "left as it was");
        // Calibration: with the transcript back, the same slot resumes.
        std::fs::write(rec.transcript_path.as_ref().unwrap(), PROMPTED).unwrap();
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert!(!matches!(out, Outcome::Refused(_)), "{out:?}");
    }

    #[test]
    fn a_resume_with_no_room_offers_the_idlest_offloadable_slot() {
        let mut f = Fixture::new("no-room");
        registry::store(&offloaded("claude-1", "conv-1", &f.work())).unwrap();
        idle(&mut f, "claude-2");
        let mut deps = f.deps(f.zmx(":"), f.claude_ok());
        deps.memory = full;
        let rows = [row("claude-1", "a"), row("claude-2", "mount guards on one")];
        let mut term = Term::new(&f);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::NoRoom(Dialog::NoRoom {
                used: (1 << 30) - (100 << 20),
                limit: 1 << 30,
                want: mem::SESSION_COST,
                offer: Some((1, "11m".into(), "mount guards on one".into())),
            })
        );
        assert!(f.calls().is_empty() && term.events.is_empty());
        assert_eq!(load("claude-1").state, State::Offloaded);

        // With nothing offloadable the dialog can only say so.
        f.session("claude-2", true);
        let out = open_with(&rows, 0, &mut term, &deps, true);
        assert!(
            matches!(out, Outcome::NoRoom(Dialog::NoRoom { offer: None, .. })),
            "an attached slot is not offered: {out:?}"
        );

        // Calibration: room, or no way of knowing, both let it through.
        for memory in [room as fn() -> mem::Memory, unknown] {
            deps.memory = memory;
            registry::store(&offloaded("claude-1", "conv-1", &f.work())).unwrap();
            let _ = std::fs::remove_file(f.path("zmx/claude-1"));
            let out = open_with(&rows, 0, &mut term, &deps, true);
            assert!(matches!(out, Outcome::Back(_)), "{out:?}");
        }
    }

    #[test]
    fn offload_then_open_stops_the_victim_and_starts_the_new_one() {
        let mut f = Fixture::new("make-room");
        let (pid, start) = idle(&mut f, "claude-1");
        let mut deps = f.deps(f.zmx(":"), f.claude_ok());
        deps.memory = full;
        let rows = [row("claude-1", "mount guards on one")];
        let mut term = Term::new(&f);

        // Refused while it no longer qualifies: attached since the offer was made.
        f.session("claude-1", true);
        let out = offload_then_open_with(&rows, 0, None, &f.work(), &mut term, &deps);
        assert!(
            matches!(&out, Outcome::Refused(why) if why.contains("attached")),
            "{out:?}"
        );
        assert!(procinfo::is_alive(pid, start), "calibration: still running");

        f.session("claude-1", false);
        let out = offload_then_open_with(&rows, 0, None, &f.work(), &mut term, &deps);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into())),
            "no second room check after making room"
        );
        assert!(!procinfo::is_alive(pid, start), "the victim was stopped");
        assert_eq!(load("claude-1").state, State::Offloaded);
        assert_eq!(load("claude-2").state, State::Live);
        term.assert_handed_over(1);
    }

    #[test]
    fn a_never_prompted_slot_is_not_offered_and_one_whose_transcript_went_is_closed() {
        // Issue #5: the dialog promises the offered session is resumable from disk.
        let mut f = Fixture::new("make-room-unprompted");
        let (pid, start) = idle(&mut f, "claude-1");
        let rows = [row("claude-1", "a")];
        assert!(
            offer(&rows).is_some(),
            "calibration: with a transcript it is offered"
        );
        let transcript = load("claude-1").transcript_path.unwrap();
        std::fs::remove_file(&transcript).unwrap();
        assert_eq!(offer(&rows), None, "with none it is not");

        // Accepted all the same — the transcript went after the offer was drawn — the stop is
        // a close, as the offloader's would be.
        let mut deps = f.deps(f.zmx(":"), f.claude_ok());
        deps.memory = full;
        let mut term = Term::new(&f);
        offload_then_open_with(&rows, 0, None, &f.work(), &mut term, &deps);
        assert!(!procinfo::is_alive(pid, start), "the victim was stopped");
        assert_eq!(load("claude-1").state, State::Closed);
    }

    // ── new ─────────────────────────────────────────────────────────────────

    #[test]
    fn allocation_takes_the_lowest_free_name_and_reuses_a_closed_one() {
        let mut f = Fixture::new("allocate");
        assert_eq!(allocate("/w").unwrap(), "claude-1", "calibration: empty");
        let (pid, start) = f.process();
        registry::store(&live("claude-1", pid, start)).unwrap();
        registry::store(&offloaded("claude-3", "c", "/")).unwrap();
        f.session("claude-4", false); // started by hand under one of our names
        assert_eq!(
            allocate("/w").unwrap(),
            "claude-2",
            "live, offloaded and socket-only names are taken"
        );
        let rec = load("claude-2");
        assert_eq!(rec.state, State::Live);
        assert_eq!((rec.pid, rec.session_id), (None, None), "a fresh record");
        assert_eq!(rec.cwd.as_deref(), Some("/w"));

        // A closed slot's name is free: its conversation is listed from Claude Code's own
        // store, under its own id, not from the record (owner, 2026-10-03).
        let mut closed = load("claude-2");
        closed.state = State::Closed;
        closed.session_id = Some("old".into());
        registry::store(&closed).unwrap();
        assert_eq!(allocate("/w").unwrap(), "claude-2");
        assert_eq!(load("claude-2").session_id, None, "a fresh record over it");
        assert_eq!(
            allocate("/w").unwrap(),
            "claude-5",
            "calibration: once live again, it is taken"
        );
    }

    #[test]
    fn two_menus_allocating_at_once_never_pick_the_same_name() {
        // Calibrated by breaking the locks, 2026-10-02: with the registry-wide lock and the
        // slot lock both made per-thread this failed five runs in five; with either one
        // intact it passes, because each alone serialises the look and the write. Separate
        // open files are separate flock holders, so threads here contend as processes would.
        let _f = Fixture::new("allocate-race");
        let threads = 8;
        let gate = std::sync::Arc::new(Barrier::new(threads));
        let picked: Vec<String> = (0..threads)
            .map(|_| {
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    allocate("/w").unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let mut sorted = picked.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), threads, "every name distinct: {picked:?}");
        let mut want: Vec<String> = (1..=threads).map(|n| format!("claude-{n}")).collect();
        want.sort();
        assert_eq!(sorted, want, "and no gaps");
    }

    #[test]
    fn a_new_session_starts_claude_in_the_workspace() {
        let f = Fixture::new("new");
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let mut term = Term::new(&f);
        let out = new_session_with(&[], &f.work(), &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Back(Some("detached · it is still running".into()))
        );
        let started = wait_for("the stand-in claude", || f.claude_log().pop());
        let fields: Vec<&str> = started.split_whitespace().collect();
        assert_eq!(&fields[1..], [&f.work(), "claude-1"], "no arguments");
        term.assert_handed_over(1);
        assert_eq!(load("claude-1").state, State::Live);
        // Said on the command line rather than inherited, so this cannot pass merely because
        // the tests themselves run under a slot that already has them.
        assert!(
            f.calls()[0].contains(" env CLAUDE_SESSIONS_SLOT=claude-1 CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 CLAUDE_CODE_DISABLE_MOUSE=1 CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL=1 sh -c "),
            "{:?}",
            f.calls()
        );
        let env = wait_for("its environment", || f.claude_env().pop());
        assert_eq!(env, "1 1 1", "the terminal's own scrollback, issue #8");
    }

    #[test]
    fn a_new_session_that_dies_at_once_is_closed_and_says_what_it_said() {
        let f = Fixture::new("new-fails");
        let deps = f.deps(f.zmx(":"), f.claude_fails());
        let mut term = Term::new(&f);
        let out = new_session_with(&[], &f.work(), &mut term, &deps, true);
        assert_eq!(
            out,
            Outcome::Refused(
                "claude-1 did not start: claude ended at once: No conversation found with \
                 that session id"
                    .into()
            )
        );
        assert_eq!(load("claude-1").state, State::Closed);
    }

    // ── close and shell ─────────────────────────────────────────────────────

    #[test]
    fn closing_a_running_slot_stops_it_and_marks_it_closed() {
        let f = Fixture::new("close");
        let deps = f.deps(f.zmx(":"), f.claude_ok());
        let mut term = Term::new(&f);
        new_session_with(&[], &f.work(), &mut term, &deps, true);
        // Play the hook's part: SessionStart binds the slot's own claude.
        let started = wait_for("the stand-in claude", || f.claude_log().pop());
        let pid: u32 = started.split_whitespace().next().unwrap().parse().unwrap();
        let start = procinfo::start_time(pid).unwrap();
        let mut rec = load("claude-1");
        rec.pid = Some(pid);
        rec.proc_start = Some(start);
        rec.session_id = Some("conv-1".into());
        let transcript = Path::new(&f.work()).join("conv-1.jsonl");
        std::fs::write(&transcript, PROMPTED).unwrap();
        rec.transcript_path = Some(transcript.display().to_string());
        registry::store(&rec).unwrap();
        assert!(procinfo::is_alive(pid, start), "calibration: it is running");
        assert!(listed("claude-1") == Some(true));

        let rows = [row("claude-1", "a")];
        let out = close(&rows, 0);
        assert_eq!(
            out,
            Outcome::Back(Some("closed · resumable from disk".into()))
        );
        assert!(!procinfo::is_alive(pid, start), "the process is stopped");
        assert_eq!(load("claude-1").state, State::Closed);
        assert!(
            listed("claude-1") == Some(false),
            "and zmx dropped its session"
        );
    }

    #[test]
    fn closing_an_offloaded_slot_only_marks_it_and_a_socket_row_is_refused() {
        let f = Fixture::new("close-offloaded");
        registry::store(&offloaded("claude-1", "conv-1", "/")).unwrap();
        let mut rows = vec![row("claude-1", "a"), row("x", "claude")];
        assert_eq!(
            close(&rows, 0),
            Outcome::Back(Some("closed · resumable from disk".into()))
        );
        assert_eq!(load("claude-1").state, State::Closed);

        f.session("claude", false);
        rows[1].key = RowKey::Socket("claude".into());
        assert!(matches!(close(&rows, 1), Outcome::Refused(_)));
        assert!(listed("claude") == Some(true), "left alone");
    }

    #[test]
    fn a_shell_is_a_login_shell_in_the_workspace() {
        let f = Fixture::new("shell");
        let mut deps = f.deps(String::new(), String::new());
        deps.shell = f.script(
            "shell",
            &format!(r#"echo "shell $PWD $*" >> "{}""#, f.path("calls").display()),
        );
        let mut term = Term::new(&f);
        assert_eq!(shell_with(&f.work(), &mut term, &deps), Outcome::Back(None));
        assert_eq!(f.calls(), [format!("shell {} -l", f.work())]);
        term.assert_handed_over(1);
    }

    #[test]
    fn a_scrollback_variable_the_login_set_is_left_to_it() {
        let deps = Deps {
            zmx: "zmx".into(),
            claude: "claude".into(),
            shell: String::new(),
            memory: room,
            oom_kills: || Some(0),
            quick: QUICK_FAIL,
            preset: |key| key == "CLAUDE_CODE_DISABLE_MOUSE",
        };
        let cmd = start_command("claude-3", Path::new("/r/e"), &[], &deps);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let assigned: Vec<&str> = args
            .iter()
            .map(String::as_str)
            .filter(|a| a.starts_with("CLAUDE_CODE_"))
            .collect();
        assert_eq!(
            assigned,
            [
                "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1",
                "CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL=1"
            ],
            "no assignment for the one the login set, so claude inherits the login's value"
        );
    }

    // ── small pieces ────────────────────────────────────────────────────────

    #[test]
    fn captured_stderr_is_made_safe_to_draw() {
        assert_eq!(
            printable("\u{1b}[1;31mError:\u{1b}[0m\tno such\u{7} thing\r"),
            "Error: no such thing"
        );
        assert_eq!(printable("plain"), "plain", "calibration");
    }

    #[test]
    fn the_start_command_is_the_one_tests_launch_rs_binds() {
        // tests/launch.rs runs this argv under the real hook, which is the only place the
        // binding can be shown end to end; it spells the line out because a binary crate
        // cannot be imported. If this changes, change it there too.
        let deps = Deps {
            zmx: "zmx".into(),
            claude: "claude".into(),
            shell: String::new(),
            memory: room,
            oom_kills: || Some(0),
            quick: QUICK_FAIL,
            preset: |_| false,
        };
        let cmd = start_command(
            "claude-3",
            Path::new("/r/claude-3.stderr"),
            &["--resume".into(), "c".into()],
            &deps,
        );
        let argv: Vec<String> = std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            argv,
            [
                "zmx",
                "attach",
                "claude-3",
                "env",
                "CLAUDE_SESSIONS_SLOT=claude-3",
                "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1",
                "CLAUDE_CODE_DISABLE_MOUSE=1",
                "CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL=1",
                "sh",
                "-c",
                r#"e=$1; shift; exec "$@" 2>>"$e""#,
                "sh",
                "/r/claude-3.stderr",
                "claude",
                "--resume",
                "c",
            ]
        );
    }
}
