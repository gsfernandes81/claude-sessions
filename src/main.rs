//! claude-sessions — several Claude Code sessions per box, cheap when idle, findable again.
//!
//! One binary, subcommands. With no arguments at a terminal it is the menu (`run.rs`, drawing
//! `render.rs` from `menu.rs`'s state, acting through `launch.rs`); everything else here is
//! the registry, the hook that feeds it, and the passes that keep it agreeing with reality.
//!
//! Arguments are parsed by hand. A handful of subcommands and flags is not worth a parser, and
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
mod agent;
mod archive;
mod bind;
mod clock;
mod events;
mod fmt;
mod hooks_config;
mod hub;
mod install;
mod json;
mod keepalive;
mod launch;
mod live;
mod lockfile;
mod mail;
mod mem;
mod menu;
mod offload;
mod procinfo;
mod proto;
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
/// The keep-alive skill, printed by `claude-sessions skill` so an image needs only the binary.
const SKILL: &str = include_str!("../skills/keepalive/SKILL.md");

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
        // THE ONE SUBCOMMAND THAT MUST NOT FAIL OR SPEAK: its stdout is fed to Claude and a
        // failing synchronous hook is shown to the person. Every failure is logged and
        // swallowed, a panic included.
        "hook" => {
            std::panic::set_hook(Box::new(|info| log(&format!("hook: panicked: {info}"))));
            contained(cmd_hook, log);
            ExitCode::SUCCESS
        }
        // The mail hook has the same contract: fed by Claude Code, always 0, and the only thing
        // it may say is its one line per new message.
        "mail-hook" => {
            std::panic::set_hook(Box::new(|info| {
                log(&format!("mail-hook: panicked: {info}"))
            }));
            contained(
                || mail::hook(&mut std::io::stdout().lock(), &log),
                |e| log(&format!("mail-{e}")),
            );
            ExitCode::SUCCESS
        }
        "mail-agent" => report(mail::agent()),
        "mail-loop" => match args.get(1).map(String::as_str) {
            None => report(mail::run(false)),
            Some("--once") => report(mail::run(true)),
            Some(other) => fail(&format!("mail-loop takes only --once, not {other:?}")),
        },
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
        "keepalive" => {
            let Some(asked) = args.get(1) else {
                return fail("keepalive needs a duration: claude-sessions keepalive 25m");
            };
            match keepalive::run(asked) {
                Ok(line) => {
                    say!("{line}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        "skill" => {
            say_nl!("{SKILL}");
            ExitCode::SUCCESS
        }
        "offload" => {
            let dry = match args.get(1).map(String::as_str) {
                None => false,
                Some("--dry-run") => true,
                Some(other) => {
                    return fail(&format!("offload takes only --dry-run, not {other:?}"));
                }
            };
            report(offload::run(dry))
        }
        "offload-all" => match args.get(1).map(String::as_str) {
            None => report(hub::offload_all(false)),
            Some("--dry-run") => report(hub::offload_all(true)),
            Some(other) => fail(&format!("offload-all takes only --dry-run, not {other:?}")),
        },
        // The container's half of the hub (`agent.rs`): never typed, always run over ssh.
        "agent" => match args.get(1).map(String::as_str) {
            None => report(agent::serve()),
            Some("exec") => match args.get(2) {
                Some(hex) => fail(&format!("could not run that: {}", agent::exec(hex))),
                None => fail("agent exec needs the command the hub was sent"),
            },
            Some(other) => fail(&format!("agent takes only exec, not {other:?}")),
        },
        "install" => match args.get(1) {
            Some(dir) => match install::run(std::path::Path::new(dir)) {
                Ok(done) => {
                    for path in done {
                        say!("{path}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&format!("install: {e}")),
            },
            None => fail("install needs a directory: claude-sessions install /opt/claude-sessions"),
        },
        other => fail(&format!("unknown subcommand {other:?}\n\n{}", usage())),
    }
}

fn usage() -> String {
    format!(
        "claude-sessions {VERSION} — Claude Code sessions, per box

  claude-sessions              the menu over every container (CLAUDE_SESSIONS_HOSTS, else
                               this one); the list below when not at a terminal
  claude-sessions offload-all  one offload pass in every container, over ssh; run from the
                  [--dry-run]  hub's timer every three minutes
  claude-sessions mail-loop    carry mail between the containers: each one's outbox into the
                  [--once]     hub's volume (HUB_MAIL), each queue into its receiver's inbox;
                               run from the hub's start, for ever, or one pass
  claude-sessions install DIR  put this binary, its hooks drop-in and its skill in DIR, for
                               every container to mount
  claude-sessions agent        a container's half of the hub, run over ssh by the hub
  claude-sessions mail-agent   a container's half of the mail loop, run over ssh by the hub
  claude-sessions mail-hook    fed by Claude Code's hooks on stdin; one line per message new
                               in /workspace/.mail/in, nothing otherwise; always exits 0
  claude-sessions list         every slot here, one line each
  claude-sessions hook         fed by Claude Code's hooks on stdin; always exits 0
  claude-sessions reconcile    make the registry agree with reality after a restart
  claude-sessions doctor       what is visible, per slot, and what is not
  claude-sessions close SLOT   mark a slot closed (refuses one that is still running)
  claude-sessions offload      measure every slot here, and stop each one quiet for 10
                               minutes, detached and not kept alive; run by offload-all
                  [--dry-run]  say what it would stop, and stop nothing
  claude-sessions keepalive DURATION
                               keep the slot this runs in from being offloaded for DURATION
                               (90s, 25m, 2h; at most 12h; 0 ends it), for work that waits
                               quietly
  claude-sessions skill        print the keep-alive skill that tells claude when to run that,
                               to be saved as skills/keepalive/SKILL.md in Claude Code's
                               config directory
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

/// Run the hook's work and let nothing out: an error is logged, and a panic stops here, having
/// been logged by the panic hook the caller installed.
fn contained(
    work: impl FnOnce() -> std::io::Result<()> + std::panic::UnwindSafe,
    log: impl Fn(&str),
) {
    if let Ok(Err(e)) = std::panic::catch_unwind(work) {
        log(&format!("hook: {e}"));
    }
}

fn cmd_hook() -> std::io::Result<()> {
    let mut body = String::new();
    std::io::stdin().read_to_string(&mut body)?;
    let ev = match events::Event::parse(&body) {
        Ok(ev) => ev,
        Err(e) => {
            // A payload shape this version does not know changes nothing: the menu shows what
            // it last knew, and the offloader reads no hooks.
            log(&format!(
                "unparseable payload ({e}): {}",
                body.chars().take(200).collect::<String>()
            ));
            return Ok(());
        }
    };

    let Some((slot, registered)) = bind::slot() else {
        log(&format!("no slot for a {} event; not ours", ev.name()));
        return Ok(());
    };

    let origin = bind::origin(std::process::id(), ev.fired_in_subagent());
    let (binding, own_pid, fired) = (origin.binding, origin.own_pid, origin.fired());
    let own_start = own_pid.and_then(procinfo::start_time);

    // A SessionEnd for /clear or /resume changes nothing — the same process goes on, and its
    // SessionStart follows at once — so it takes no lock and writes nothing. It used to take
    // the lock anyway and race that SessionStart for it, and the loser logged a lock failure
    // indistinguishable from a real end being dropped (infra's Stage B review, 2026-10-03).
    if ev.name() == "SessionEnd" && matches!(ev.reason(), Some("clear") | Some("resume")) {
        return Ok(());
    }

    let wait = events::lock_wait(&ev, binding);
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

    let mut rec = registry::load(&slot)?.unwrap_or_else(|| {
        let mut r = SlotRecord::new(&slot, fired);
        r.registered = registered;
        r
    });
    rec.written.forget_other_boot();
    match events::apply(&mut rec, &ev, fired, binding, own_pid, own_start) {
        Outcome::Changed => {
            if let Some(titles) = &titles {
                events::apply_titles(&mut rec, titles);
            }
            rec.updated_ms = clock::now();
            registry::store(&rec)
        }
        Outcome::Ignored(_why) => Ok(()),
    }
}

// ── reconcile ───────────────────────────────────────────────────────────────

/// Make the registry agree with reality.
///
/// Run at container start, because a `stop`/`start` keeps the registry on disk and hands out
/// the same pids again: without this, every slot still reads `live` and points at a pid that
/// now belongs to something else.
fn cmd_reconcile() -> std::io::Result<()> {
    let mut moved = 0usize;
    for listed in registry::all()? {
        if !process_gone(&listed) {
            continue;
        }
        let slot = listed.slot;
        let _lock = match lockfile::SlotLock::acquire(
            &registry::lock_path(&slot),
            lockfile::INTERACTIVE_WAIT,
        ) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                say!("{slot}: kept — its lock is busy");
                continue;
            }
            Err(e) => return Err(e),
        };
        // Decided again on the record as it is under the lock, not as it was listed.
        let Some(mut rec) = registry::load(&slot)? else {
            continue;
        };
        if !process_gone(&rec) {
            continue;
        }
        rec.state = State::Offloaded;
        rec.busy = false;
        rec.updated_ms = clock::now();
        registry::store(&rec)?;
        say!("{slot}: process gone -> offloaded");
        moved += 1;
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

/// A slot still open whose recorded process has gone. A record with no pid cannot be checked
/// and is left: `doctor` reports it, and guessing would mark a live session offloaded.
fn process_gone(rec: &SlotRecord) -> bool {
    if matches!(rec.state, State::Closed | State::Offloaded) {
        return false;
    }
    match (rec.pid, rec.proc_start) {
        (Some(pid), Some(start)) => !procinfo::is_alive(pid, start),
        _ => false,
    }
}

// ── list and doctor ─────────────────────────────────────────────────────────

fn cmd_list() -> std::io::Result<()> {
    let recs = registry::all()?;
    let sessions = zmx::sessions().unwrap_or_else(|| {
        warn!(
            "claude-sessions: zmx did not answer; attached and unregistered sessions are not shown"
        );
        Vec::new()
    });
    let alive = |r: &SlotRecord| matches!((r.pid, r.proc_start), (Some(p), Some(s)) if procinfo::is_alive(p, s));
    for line in list_lines(&recs, &sessions, alive, clock::now()) {
        say!("{line}");
    }
    Ok(())
}

/// `list`'s lines: every slot not closed, in the menu's order, then every zmx session no
/// record names. `alive` says whether a record's process is still running.
fn list_lines(
    recs: &[SlotRecord],
    sessions: &[zmx::Session],
    alive: impl Fn(&SlotRecord) -> bool,
    now: clock::Millis,
) -> Vec<String> {
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
        return vec!["no slots. run claude-sessions at a terminal and press n to start one".into()];
    }
    let mut out = Vec::new();
    for r in rows {
        let sess = sessions.iter().find(|s| s.name == r.slot);
        let marks = format!(
            "{}{}{}{}{}",
            if r.needs_you { "!" } else { "" },
            if r.unread() { "*" } else { "" },
            if r.kept_for(now).is_some() { "k" } else { "" },
            // Attached is only meaningful for a session we know to be alive.
            if alive(r) && sess.is_some_and(|s| s.attached) {
                "@"
            } else {
                ""
            },
            if r.state == State::Offloaded { "z" } else { "" },
        );
        out.push(format!(
            "{:<10} {:<4} {:<40} {}",
            r.slot,
            marks,
            r.display_title(),
            age(now.saturating_sub(r.last_activity_ms))
        ));
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
        out.push(format!(
            "{:<10} {:<4} {:<40} --",
            sock.name,
            if sock.attached { "u@" } else { "u" },
            why
        ));
    }
    out
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
    say!("{}", memory_line(&m));
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
            "  flags     : busy={} needs_you={} unread={} kept alive={}",
            rec.busy,
            rec.needs_you,
            rec.unread(),
            or_none(
                rec.kept_for(now)
                    .map(|left| format!("{}m more", left.div_ceil(60_000)))
            )
        );
        if rec.last_event_ms.is_empty() {
            say!("  events    : none seen — the hooks are not installed, or not firing");
        } else {
            for (name, at) in &rec.last_event_ms {
                say!("  {name:<10}: last seen {}", seen(now, *at));
            }
        }
    }
    Ok(())
}

/// `doctor`'s memory line.
fn memory_line(m: &mem::Memory) -> String {
    let (Some(limit), Some(used)) = (m.limit, m.used()) else {
        return "memory    : no cgroup limit readable; /proc/meminfo describes the host".into();
    };
    format!(
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
    )
}

/// How long ago `at` was: "now", not "now ago".
fn seen(now: clock::Millis, at: clock::Millis) -> String {
    match age(now.saturating_sub(at)) {
        a if a == "now" => a,
        a => format!("{a} ago"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const NOW: clock::Millis = 10 * 86_400_000;

    fn rec(slot: &str, f: impl FnOnce(&mut SlotRecord)) -> SlotRecord {
        let mut r = SlotRecord::new(slot, clock::Moment::ms(NOW - 5 * 60_000));
        r.title = Some(format!("{slot}'s title"));
        f(&mut r);
        r
    }

    fn sess(name: &str, attached: bool) -> zmx::Session {
        zmx::Session {
            name: name.into(),
            pid: Some(1),
            attached,
            answered: true,
            created: None,
        }
    }

    /// Each line's first two columns: the slot and its marks.
    fn heads(lines: &[String]) -> Vec<(String, String)> {
        lines
            .iter()
            .map(|l| {
                let mut w = l.split_whitespace();
                let slot = w.next().unwrap().to_string();
                let marks = l[10..15].trim().to_string();
                (slot, marks)
            })
            .collect()
    }

    #[test]
    fn list_orders_and_marks_as_the_menu_does() {
        let recs = [
            rec("claude-1", |r| r.last_activity_ms = NOW - 60_000),
            rec("claude-2", |r| r.needs_you = true),
            rec("claude-3", |r| r.last_stop_ms = Some(NOW - 1_000)),
            rec("claude-4", |r| r.state = State::Offloaded),
            rec("claude-5", |r| r.keep_until_ms = Some(NOW + 60_000)),
            rec("claude-6", |r| r.state = State::Closed),
        ];
        let sessions = [sess("claude-1", true), sess("claude-5", true)];
        // claude-5 is attached by zmx's word, but its process is not alive: no `@`.
        let alive = |r: &SlotRecord| r.slot != "claude-5";
        let lines = list_lines(&recs, &sessions, alive, NOW);
        let want = [
            ("claude-2", "!"),
            ("claude-3", "*"),
            ("claude-1", "@"),
            ("claude-4", "z"),
            ("claude-5", "k"),
        ];
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(heads(&lines), want, "{lines:#?}");
        assert!(
            lines[2].contains("claude-1's title") && lines[2].ends_with(" 1m"),
            "{lines:#?}"
        );
    }

    #[test]
    fn list_shows_zmx_sessions_no_record_names() {
        let recs = [rec("claude-1", |_| {})];
        let sessions = [
            sess("claude-1", false),
            sess("claude-9", false),
            sess("work", true),
        ];
        let lines = list_lines(&recs, &sessions, |_| true, NOW);
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(
            lines[1].starts_with("claude-9   u ") && lines[1].contains("no record — run reconcile")
        );
        assert!(lines[2].starts_with("work       u@") && lines[2].contains("not started by"));
        // Nothing at all says how to start; a closed slot alone is nothing.
        let closed = [rec("claude-1", |r| r.state = State::Closed)];
        let none = list_lines(&closed, &[], |_| true, NOW);
        assert_eq!(none.len(), 1);
        assert!(none[0].starts_with("no slots."), "{none:?}");
        // Calibration: an unregistered session alone is something.
        assert_eq!(list_lines(&closed, &sessions[2..], |_| true, NOW).len(), 1);
        assert!(list_lines(&closed, &sessions[2..], |_| true, NOW)[0].starts_with("work"));
    }

    #[test]
    fn doctor_says_memory_against_a_limit_or_that_there_is_none() {
        let g = 1024 * 1024 * 1024;
        let m = mem::Memory {
            limit: Some(8 * g),
            current: Some(3 * g),
            reclaimable: Some(g),
        };
        assert_eq!(
            memory_line(&m),
            "memory    : 2.0G of 8.0G used in this container (page cache the kernel reclaims \
             first, 1.0G, not counted); room for another session: yes"
        );
        let full = mem::Memory {
            current: Some(8 * g),
            reclaimable: None,
            ..m
        };
        assert!(memory_line(&full).ends_with("counted as used); room for another session: no"));
        let unlimited = mem::Memory { limit: None, ..m };
        assert!(memory_line(&unlimited).contains("no cgroup limit"));
    }

    #[test]
    fn doctor_says_now_or_how_long_ago() {
        assert_eq!(seen(NOW, NOW - 1_000), "now");
        assert_eq!(seen(NOW, NOW - 3 * 60_000), "3m ago");
    }

    #[test]
    fn the_hook_lets_no_error_or_panic_out() {
        let said = RefCell::new(Vec::new());
        let log = |l: &str| said.borrow_mut().push(l.to_string());
        contained(|| Ok(()), log);
        assert!(said.borrow().is_empty(), "calibration: nothing to say");
        contained(|| Err(std::io::Error::other("no registry")), log);
        assert_eq!(*said.borrow(), ["hook: no registry"]);
        // Unwound without the panic hook, which would put another test's terminal back.
        contained(
            || std::panic::resume_unwind(Box::new("a registry bug")),
            log,
        );
    }
}
