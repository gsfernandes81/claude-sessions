//! claude-sessions — several Claude Code sessions per box, cheap when idle, findable again.
//!
//! One binary, subcommands. With no arguments at a terminal it is the menu (`run.rs`, drawing
//! `render.rs` from `menu.rs`'s state, acting through `launch.rs`); everything else here is
//! the registry, the hook that feeds it, and the passes that keep it agreeing with reality.
//!
//! Arguments are parsed by hand. Eight subcommands and three flags is not worth a parser, and
//! this binary is on the ssh path in a container pulled by checksum — every dependency is one
//! more thing to cross-compile for musl and one more thing to read before trusting.

// Every print goes through `say!`, `say_nl!` and `warn!` below, never `print!`/`println!`/
// `eprintln!`: those panic when the write fails, and a write fails whenever the terminal or the
// pipe on the other end has gone (issue #6: an ssh link dropped under the menu, its report
// of that went to the same dead terminal, and the panic dumped core in a git checkout). The
// lints hold the line; tests may print.
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

/// `println!` to stdout that cannot panic: a reader that has gone is no reason to crash.
macro_rules! say {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}

/// `print!` to stdout that cannot panic.
macro_rules! say_nl {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = write!(std::io::stdout(), $($t)*);
    }};
}

/// `eprintln!` that cannot panic.
macro_rules! warn {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

mod activity;
mod archive;
mod bind;
mod clock;
mod events;
mod fmt;
mod hooks_config;
mod json;
mod launch;
mod live;
mod lockfile;
mod mem;
mod menu;
mod offload;
mod procinfo;
mod registry;
mod render;
mod run;
mod signal;
mod sockdiag;
mod statusline;
mod store;
mod term;
mod transcript;
mod ui;
mod work;
mod zmx;

use events::{Binding, Outcome};
use fmt::{age, human};
use registry::{SlotRecord, State};
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The menu's exit status when its terminal went away: 128 + SIGHUP, what a shell reports
/// for a hangup, so the door can tell it from a failure worth explaining.
const TERMINAL_GONE: u8 = 129;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "--version" | "-V" => {
            say!("claude-sessions {VERSION}");
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" => {
            say_nl!("{}", usage());
            ExitCode::SUCCESS
        }
        // THE ONE SUBCOMMAND THAT MUST NOT FAIL. UserPromptSubmit, Stop and SubagentStop are
        // blocking hooks: a non-zero exit on the first blocks the prompt, and on the others
        // tells Claude (or its subagent) it has more to do. A registry bug must never wedge a
        // session, so every failure in here is logged and swallowed.
        // A panic too: it is logged here rather than printed, and caught, so even a bug that
        // panics exits 0.
        "hook" => {
            std::panic::set_hook(Box::new(|info| log(&format!("hook: panicked: {info}"))));
            match std::panic::catch_unwind(cmd_hook) {
                Ok(Ok(())) | Err(_) => {}
                Ok(Err(e)) => log(&format!("hook: {e}")),
            }
            ExitCode::SUCCESS
        }
        "reconcile" => report(cmd_reconcile()),
        "doctor" => report(cmd_doctor()),
        // The menu, when there is a person at a terminal to drive it; the plain list when the
        // output is a pipe or a file, so `claude-sessions | grep` still answers.
        "" if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            match run::run() {
                Ok(()) => ExitCode::SUCCESS,
                // Nobody to tell: end quietly, with the status a shell gives a hangup.
                Err(run::Stop::TerminalGone) => ExitCode::from(TERMINAL_GONE),
                Err(run::Stop::Failed(e)) => fail(&e),
            }
        }
        "list" | "" => report(cmd_list()),
        "close" => match args.get(1) {
            Some(slot) => report(cmd_close(slot)),
            None => fail("close needs a slot name: claude-sessions close claude-1"),
        },
        "hooks-config" => {
            // The path the hook command will name. Given explicitly by an image build that
            // installs the binary somewhere other than where it runs it from; otherwise this
            // binary's own path, which is right when it is run from where it is installed.
            let exe = match args.get(1) {
                Some(p) => p.clone(),
                None => match std::env::current_exe() {
                    Ok(p) => p.display().to_string(),
                    Err(e) => {
                        return fail(&format!(
                            "cannot tell where this binary is ({e}); pass its path"
                        ));
                    }
                },
            };
            say!("{}", json::to_string_pretty(&hooks_config::settings(&exe)));
            ExitCode::SUCCESS
        }
        "statusline" => {
            statusline::run();
            ExitCode::SUCCESS
        }
        "offload" => match args.get(1).map(String::as_str) {
            None => report(offload::run(false)),
            Some("--dry-run") => report(offload::run(true)),
            Some(other) => fail(&format!("offload takes only --dry-run, not {other:?}")),
        },
        other => fail(&format!("unknown subcommand {other:?}\n\n{}", usage())),
    }
}

fn usage() -> String {
    format!(
        "claude-sessions {VERSION} — Claude Code sessions, per box

  claude-sessions              the menu; the list below when not at a terminal
  claude-sessions list         every slot, one line each
  claude-sessions hook         fed by Claude Code's hooks on stdin; always exits 0
  claude-sessions reconcile    make the registry agree with reality after a restart
  claude-sessions doctor       what is visible, per slot, and what is not
  claude-sessions close SLOT   mark a slot closed (refuses one that is still running)
  claude-sessions offload      stop every slot idle 10 minutes past its Stop; run from a timer
                  [--dry-run]  say what it would stop, and stop nothing; either way, say
                               what the activity rule (measured, not acted on) would do
  claude-sessions statusline   RAM, load and host, for Claude Code's status line
  claude-sessions hooks-config [PATH]
                               the Claude Code settings that install the hooks and the status
                               line, naming PATH
                               (default: this binary) — for /etc/claude-code/managed-settings.d/

The registry is {} — override with CLAUDE_SESSIONS_DIR.
",
        registry::dir().display()
    )
}

fn report(r: std::io::Result<()>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(msg: &str) -> ExitCode {
    warn!("claude-sessions: {msg}");
    ExitCode::FAILURE
}

/// Best-effort log beside the registry. A hook that cannot say why it failed is a hook whose
/// failure is invisible, and this is the only place that would ever record it.
fn log(line: &str) {
    let dir = registry::dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hook.log"))
    {
        let _ = writeln!(f, "{} {}", clock::now(), line);
    }
}

// ── hook ────────────────────────────────────────────────────────────────────

fn cmd_hook() -> std::io::Result<()> {
    let mut body = String::new();
    std::io::stdin().read_to_string(&mut body)?;
    let ev = match events::Event::parse(&body) {
        Ok(ev) => ev,
        Err(e) => {
            // A payload shape this version does not know degrades to no evidence, which is
            // safe: the offloader's rule is that it needs evidence to act, not to hold off.
            log(&format!(
                "unparseable payload ({e}): {}",
                body.chars().take(200).collect::<String>()
            ));
            return Ok(());
        }
    };

    let Some((slot, registered)) = slot_for_hook() else {
        log(&format!("no slot for a {} event; not ours", ev.name()));
        return Ok(());
    };

    let binding = bind::binding_for(std::process::id(), ev.fired_in_subagent());

    // The claude whose pid belongs in the record is the slot's own, which is the one directly
    // under its zmx daemon — not this hook, and not a nested claude.
    let own_pid = procinfo::ancestor_named(std::process::id(), "claude", 8)
        .filter(|_| binding == Binding::Own);
    let own_start = own_pid.and_then(procinfo::start_time);

    // A SessionEnd for /clear or /resume changes nothing — the same process goes on, and its
    // SessionStart follows at once — so it takes no lock and writes nothing. It used to take
    // the lock anyway and race that SessionStart for it, and the loser logged a lock failure
    // indistinguishable from a real end being dropped (infra's Stage B review, 2026-10-03).
    if ev.name() == "SessionEnd" && matches!(ev.reason(), Some("clear") | Some("resume")) {
        return Ok(());
    }

    // SessionEnd hooks share a 1.5 s budget, so that one event may only risk SESSION_END_WAIT.
    // Every other event has a 5 s hook timeout (hooks_config.rs) and waits longer: dropping a
    // UserPromptSubmit because the lock was busy leaves a working claude reading as idle,
    // which is the one mistake the offloader cannot survive (issue #1).
    let wait = if ev.name() == "SessionEnd" {
        lockfile::SESSION_END_WAIT
    } else {
        lockfile::INTERACTIVE_WAIT
    };
    // The conversation's titles, read before the lock — a transcript's tail is the slow part
    // of this hook, and the lock is what other hooks wait on (issue #1). Only the slot's own
    // claude names the slot, and only when a title can have changed: a conversation opening,
    // and the end of each turn.
    let titles = (binding == Binding::Own && matches!(ev.name(), "SessionStart" | "Stop"))
        .then(|| ev.transcript_path())
        .flatten()
        .map(|p| transcript::titles(std::path::Path::new(p)));

    let _lock = lockfile::SlotLock::acquire(&registry::lock_path(&slot), wait).map_err(|e| {
        // Name the event, and a SessionEnd's reason: which event lost is the whole question
        // when reading this log, and the wait in the message only hints at it.
        let reason = ev.reason().map(|r| format!(" ({r})")).unwrap_or_default();
        log(&format!("{slot}: {}{reason} dropped, lock: {e}", ev.name()));
        e
    })?;

    let now = clock::now();
    let mut rec = registry::load(&slot)?.unwrap_or_else(|| {
        let mut r = SlotRecord::new(&slot, now);
        r.registered = registered;
        r
    });
    let outcome = events::apply(&mut rec, &ev, now, binding, own_pid, own_start);
    if let Some(titles) = &titles {
        events::apply_titles(&mut rec, titles);
    }
    match outcome {
        Outcome::Changed => registry::store(&rec),
        Outcome::Ignored(_why) => Ok(()),
    }
}

/// The slot this hook belongs to, and whether we started it.
///
/// The environment is the fast answer: the menu sets `CLAUDE_SESSIONS_SLOT` when it starts a
/// slot. Failing that — a `zmx attach work claude` somebody typed, which nothing here started —
/// zmx names its own session in `ZMX_SESSION`, so the session is listed as unregistered rather
/// than ignored. Only with a zmx daemon above us: the variable is inherited, and a process that
/// merely carries it out of a session is not in one.
fn slot_for_hook() -> Option<(String, bool)> {
    if let Some(slot) = bind::slot_from_env() {
        return Some((slot, true));
    }
    let name = std::env::var("ZMX_SESSION")
        .ok()
        .filter(|s| !s.is_empty())?;
    procinfo::ancestor_named(std::process::id(), "zmx", 10)?;
    Some((name, false))
}

// ── reconcile ───────────────────────────────────────────────────────────────

/// Make the registry agree with reality.
///
/// Run at container start, because a `stop`/`start` keeps the registry on disk and hands out
/// the same pids again: without this, every slot still reads `live` and points at a pid that
/// now belongs to something else.
fn cmd_reconcile() -> std::io::Result<()> {
    let now = clock::now();
    let mut moved = 0usize;
    for mut rec in registry::all()? {
        if matches!(rec.state, State::Closed | State::Offloaded) {
            continue;
        }
        let gone = match (rec.pid, rec.proc_start) {
            (Some(pid), Some(start)) => !procinfo::is_alive(pid, start),
            // A record with no pid cannot be checked. Leave it: `doctor` reports it, and
            // guessing would mean marking a live session offloaded.
            _ => false,
        };
        if gone {
            let _lock = lockfile::SlotLock::acquire(
                &registry::lock_path(&rec.slot),
                lockfile::INTERACTIVE_WAIT,
            )?;
            rec.state = State::Offloaded;
            rec.busy = false;
            rec.updated_ms = now;
            registry::store(&rec)?;
            say!("{}: process gone -> offloaded", rec.slot);
            moved += 1;
        }
    }

    // Dead sessions: `zmx list` removes a dead daemon's socket itself when its connection is
    // refused, so one listing is the whole sweep. A daemon that does not answer is left alone:
    // it may be busy, and its session may be somebody's work.
    match zmx::sessions() {
        Some(all) => {
            let quiet = all.iter().filter(|s| !s.answered).count();
            say!(
                "reconcile: {moved} slot(s) offloaded, {} zmx session(s), {quiet} not answering",
                all.len()
            );
        }
        None => {
            warn!("claude-sessions: zmx could not be asked; its sessions were not checked");
            say!("reconcile: {moved} slot(s) offloaded");
        }
    }
    Ok(())
}

// ── list and doctor ─────────────────────────────────────────────────────────

fn cmd_list() -> std::io::Result<()> {
    let now = clock::now();
    let recs = registry::all()?;
    let sessions = zmx::sessions().unwrap_or_else(|| {
        warn!(
            "claude-sessions: zmx did not answer; attached and unregistered sessions are not shown"
        );
        Vec::new()
    });
    let mut rows: Vec<&SlotRecord> = recs.iter().filter(|r| r.state != State::Closed).collect();
    // The order the menu will use: wants you, then unread, then most recent activity.
    rows.sort_by_key(|r| {
        (
            !r.needs_you,
            !r.unread(),
            std::cmp::Reverse(r.last_activity_ms),
        )
    });
    // The menu lists zmx's sessions UNION the registry, not just the registry: a `zmx attach`
    // somebody typed is real work that nothing here started, and showing only our own would
    // mean a list that disagrees with the box.
    let unregistered: Vec<&zmx::Session> = sessions
        .iter()
        .filter(|s| !recs.iter().any(|r| r.slot == s.name))
        .collect();

    if rows.is_empty() && unregistered.is_empty() {
        say!("no slots. run claude-sessions at a terminal and press n to start one");
        return Ok(());
    }
    for r in rows {
        let sess = sessions.iter().find(|s| s.name == r.slot);
        let alive = matches!((r.pid, r.proc_start), (Some(p), Some(s)) if procinfo::is_alive(p, s));
        let marks = format!(
            "{}{}{}{}{}",
            if r.needs_you { "!" } else { "" },
            if r.unread() { "*" } else { "" },
            if r.has_pending_timer(now) { "t" } else { "" },
            // Attached is only meaningful for a session we know to be alive.
            if alive && sess.is_some_and(|s| s.attached) {
                "@"
            } else {
                ""
            },
            if r.state == State::Offloaded { "z" } else { "" },
        );
        let title = r.display_title();
        say!(
            "{:<10} {:<4} {:<40} {}",
            r.slot,
            marks,
            title,
            age(now.saturating_sub(r.last_activity_ms))
        );
    }
    for sock in unregistered {
        // `u` is the mark for "not started by claude-sessions". A name that is not of the form
        // claude-<n> is certainly not ours; one that IS could be a slot whose record went
        // missing, which `reconcile` is what repairs.
        let why = if zmx::is_slot_name(&sock.name) {
            "no record — run reconcile"
        } else {
            "not started by claude-sessions"
        };
        say!(
            "{:<10} {:<4} {:<40} --",
            sock.name,
            if sock.attached { "u@" } else { "u" },
            why
        );
    }
    Ok(())
}

/// What is visible and what is not — per slot, per event.
///
/// The event ages are the point. Claude Code updates itself in place in these containers, so
/// a renamed payload field degrades to "no evidence" rather than to an error: a slot whose
/// `Stop` was last seen in August while `UserPromptSubmit` arrived this morning means a hook
/// stopped being delivered, and nothing else in this tool would ever say so.
fn cmd_doctor() -> std::io::Result<()> {
    let now = clock::now();
    let m = mem::read();
    say!("version   : {VERSION}");
    say!("registry  : {}", registry::dir().display());
    match (m.limit, m.used()) {
        (Some(limit), Some(used)) => say!(
            "memory    : {} of {} used in this container{}; room for another session: {}",
            human(used),
            human(limit),
            m.reclaimable
                .map(|r| format!(
                    " (page cache the kernel reclaims first, {}, not counted)",
                    human(r)
                ))
                .unwrap_or_else(|| " (memory.stat unreadable: page cache counted as used)".into()),
            if m.room_for(mem::SESSION_COST) {
                "yes"
            } else {
                "no"
            }
        ),
        _ => say!("memory    : no cgroup limit readable; /proc/meminfo describes the host"),
    }
    match zmx::sessions() {
        Some(all) => say!(
            "zmx       : {} session(s): {}",
            all.len(),
            all.iter()
                .map(|s| format!(
                    "{}{}",
                    s.name,
                    if !s.answered {
                        "(not answering)"
                    } else if s.attached {
                        "(attached)"
                    } else {
                        ""
                    }
                ))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        None => say!(
            "zmx       : could not be asked ({} list failed or timed out)",
            zmx::program()
        ),
    }
    // What Claude Code says about itself, printed beside what we think — because when the two
    // disagree, that disagreement IS the finding, and nothing else in this tool would show it.
    let ours = live::all();
    say!(
        "claude    : {} live session(s) by Claude Code's own account, {} interactive",
        ours.len(),
        ours.iter().filter(|s| s.is_interactive()).count()
    );
    for sess in &ours {
        say!(
            "            pid {} {} {} {} {}",
            sess.pid,
            sess.kind.as_deref().unwrap_or("?"),
            sess.status.as_deref().unwrap_or("?"),
            sess.session_id.as_deref().unwrap_or("-"),
            sess.cwd.as_deref().unwrap_or("-"),
        );
    }
    for rec in registry::all()? {
        say!();
        say!(
            "{} [{:?}]{}",
            rec.slot,
            rec.state,
            if rec.registered { "" } else { " (not ours)" }
        );
        // Plain values, not Rust's debug spelling: this is read by a person (issue #4).
        let or_none = |v: Option<String>| v.unwrap_or_else(|| "none".into());
        say!(
            "  pid       : {} start {}",
            or_none(rec.pid.map(|p| p.to_string())),
            or_none(rec.proc_start.map(|s| s.to_string()))
        );
        say!("  session   : {}", or_none(rec.session_id.clone()));
        say!(
            "  flags     : busy={} needs_you={} unread={} timers={}",
            rec.busy,
            rec.needs_you,
            rec.unread(),
            rec.timers.len()
        );
        if rec.last_event_ms.is_empty() {
            say!("  events    : none seen — the hooks are not installed, or not firing");
        } else {
            for (name, at) in &rec.last_event_ms {
                // "last seen now", not "last seen now ago".
                let when = match age(now.saturating_sub(*at)) {
                    a if a == "now" => a,
                    a => format!("{a} ago"),
                };
                say!("  {name:<10}: last seen {when}");
            }
        }
    }
    Ok(())
}

// ── close ───────────────────────────────────────────────────────────────────

/// Mark a slot closed without opening it — for something offloaded last week.
///
/// **It refuses a slot that is still running**, rather than marking it closed and leaving a
/// live claude that nothing lists. Stopping a process is the offloader's job and the menu's
/// confirm dialog; this subcommand only ever writes a record.
fn cmd_close(slot: &str) -> std::io::Result<()> {
    let _lock =
        lockfile::SlotLock::acquire(&registry::lock_path(slot), lockfile::INTERACTIVE_WAIT)?;
    let Some(mut rec) = registry::load(slot)? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no such slot {slot}"),
        ));
    };
    if let (Some(pid), Some(start)) = (rec.pid, rec.proc_start) {
        if procinfo::is_alive(pid, start) {
            return Err(std::io::Error::other(format!(
                "{slot} is still running as pid {pid}. Closing it would leave a claude nobody \
                 lists — attach to it and /exit, or let the offloader stop it first"
            )));
        }
    }
    rec.state = State::Closed;
    rec.updated_ms = clock::now();
    registry::store(&rec)?;
    say!("{slot}: closed. the conversation is still on disk for claude --resume");
    Ok(())
}
