//! claude-sessions — several Claude Code sessions per box, cheap when idle, findable again.
//!
//! One binary, subcommands. The TUI (the default with no arguments) is not written yet; what
//! is here is the half that has to be right before a screen is worth drawing: the registry,
//! the hook that feeds it, and the repair pass that makes it agree with reality.
//!
//! Arguments are parsed by hand. Seven subcommands and three flags is not worth a parser, and
//! this binary is on the ssh path in a container pulled by checksum — every dependency is one
//! more thing to cross-compile for musl and one more thing to read before trusting.

mod abduco;
mod bind;
mod clock;
mod events;
mod json;
mod live;
mod lockfile;
mod mem;
mod offload;
mod procinfo;
mod registry;
mod signal;

use events::{Binding, Outcome};
use registry::{SlotRecord, State};
use std::io::Read;
use std::io::Write;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "--version" | "-V" => {
            println!("claude-sessions {VERSION}");
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        // THE ONE SUBCOMMAND THAT MUST NOT FAIL. UserPromptSubmit and Stop are blocking
        // hooks: a non-zero exit on the first blocks the prompt, and on the second tells
        // Claude it has more to do. A registry bug must never wedge a session, so every
        // failure in here is logged and swallowed.
        "hook" => {
            if let Err(e) = cmd_hook() {
                log(&format!("hook: {e}"));
            }
            ExitCode::SUCCESS
        }
        "reconcile" => report(cmd_reconcile()),
        "doctor" => report(cmd_doctor()),
        "list" | "" => report(cmd_list()),
        "close" => match args.get(1) {
            Some(slot) => report(cmd_close(slot)),
            None => fail("close needs a slot name: claude-sessions close claude-1"),
        },
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

  claude-sessions              the menu (not built yet; prints the list below)
  claude-sessions list         every slot, one line each
  claude-sessions hook         fed by Claude Code's hooks on stdin; always exits 0
  claude-sessions reconcile    make the registry agree with reality after a restart
  claude-sessions doctor       what is visible, per slot, and what is not
  claude-sessions close SLOT   mark a slot closed (refuses one that is still running)
  claude-sessions offload      stop every slot idle 10 minutes past its Stop; run from a timer
                  [--dry-run]  say what it would stop, and stop nothing

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
    eprintln!("claude-sessions: {msg}");
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

    let binding = bind::binding_for(std::process::id(), ev.has_agent_id());

    // The claude whose pid belongs in the record is the slot's own, which is the one directly
    // under abduco — not this hook, and not a nested claude.
    let own_pid = procinfo::ancestor_named(std::process::id(), "claude", 8)
        .filter(|_| binding == Binding::Own);
    let own_start = own_pid.and_then(procinfo::start_time);

    let _lock =
        lockfile::SlotLock::acquire(&registry::lock_path(&slot), lockfile::SESSION_END_WAIT)
            .map_err(|e| {
                log(&format!("{slot}: lock: {e}"));
                e
            })?;

    let now = clock::now();
    let mut rec = registry::load(&slot)?.unwrap_or_else(|| {
        let mut r = SlotRecord::new(&slot, now);
        r.registered = registered;
        r
    });
    match events::apply(&mut rec, &ev, now, binding, own_pid, own_start) {
        Outcome::Changed => registry::store(&rec),
        Outcome::Ignored(_why) => Ok(()),
    }
}

/// The slot this hook belongs to, and whether we started it.
///
/// The environment is the fast answer: the menu sets `CLAUDE_SESSIONS_SLOT` when it starts a
/// slot. Failing that — one of today's `abduco -A claude claude` logins, which nothing here
/// started — the abduco server above us names the session on its own command line, so the
/// slot can be recovered from `/proc` and listed as unregistered rather than ignored.
fn slot_for_hook() -> Option<(String, bool)> {
    if let Some(slot) = bind::slot_from_env() {
        return Some((slot, true));
    }
    let abduco = procinfo::ancestor_named(std::process::id(), "abduco", 10)?;
    session_name_of_abduco(abduco).map(|name| (name, false))
}

/// The session name on an abduco server's command line: the argument after `-c`, `-A` or `-n`.
fn session_name_of_abduco(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "-c" | "-A" | "-n") {
            return it.next().cloned();
        }
    }
    None
}

// ── reconcile ───────────────────────────────────────────────────────────────

/// Make the registry agree with reality.
///
/// Run at container start, because a `stop`/`start` keeps `~/.abduco` on disk and hands out
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
            println!("{}: process gone -> offloaded", rec.slot);
            moved += 1;
        }
    }

    // Stale sockets: a killed abduco server leaves its socket behind WITH THE ATTACHED BIT
    // STILL SET, so a menu built on the mode alone would show a corpse as busy and refuse to
    // offer the session. Only a socket whose name no live abduco process claims is removed,
    // and only if we could read /proc at all — deleting on a failed enumeration would take
    // every live session's socket with it.
    let live_names = abduco_session_names();
    let mut swept = 0usize;
    if let Some(live_names) = live_names {
        for sock in abduco::sockets() {
            if !live_names.contains(&sock.name) {
                std::fs::remove_file(&sock.path)?;
                println!("{}: socket with no server -> removed", sock.name);
                swept += 1;
            }
        }
    } else {
        eprintln!("claude-sessions: could not enumerate /proc; no sockets swept");
    }

    println!("reconcile: {moved} slot(s) offloaded, {swept} socket(s) swept");
    Ok(())
}

/// The session names of every live abduco server, or `None` if `/proc` could not be read.
fn abduco_session_names() -> Option<Vec<String>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if procinfo::comm(pid).as_deref() != Some("abduco") {
            continue;
        }
        if let Some(session) = session_name_of_abduco(pid) {
            out.push(session);
        }
    }
    Some(out)
}

// ── list and doctor ─────────────────────────────────────────────────────────

fn cmd_list() -> std::io::Result<()> {
    let now = clock::now();
    let recs = registry::all()?;
    let sockets = abduco::sockets();
    let mut rows: Vec<&SlotRecord> = recs.iter().filter(|r| r.state != State::Closed).collect();
    // The order the menu will use: wants you, then unread, then most recent activity.
    rows.sort_by_key(|r| {
        (
            !r.needs_you,
            !r.unread(),
            std::cmp::Reverse(r.last_activity_ms),
        )
    });
    // The menu lists abduco's sessions UNION the registry, not just the registry: until every
    // client has been reconfigured, `ssh <container>` still runs `abduco -A claude claude` and
    // those sessions are real work that nothing here started. Showing only our own would mean
    // a list that disagrees with the box.
    let unregistered: Vec<&abduco::Socket> = sockets
        .iter()
        .filter(|s| !recs.iter().any(|r| r.slot == s.name))
        .collect();

    if rows.is_empty() && unregistered.is_empty() {
        println!("no slots. the menu is not built yet; start a session by hand for now");
        return Ok(());
    }
    for r in rows {
        let sock = sockets.iter().find(|s| s.name == r.slot);
        let alive = matches!((r.pid, r.proc_start), (Some(p), Some(s)) if procinfo::is_alive(p, s));
        let marks = format!(
            "{}{}{}{}{}",
            if r.needs_you { "!" } else { "" },
            if r.unread() { "*" } else { "" },
            if r.has_pending_timer(now) { "t" } else { "" },
            // Attached is only meaningful for a session we know to be alive.
            if alive && sock.is_some_and(|s| s.attached_bit) {
                "@"
            } else {
                ""
            },
            if r.state == State::Offloaded { "z" } else { "" },
        );
        let title = r
            .pid
            .and_then(live::for_pid)
            .and_then(|l| l.real_title().map(str::to_string))
            .or_else(|| r.title.clone())
            .unwrap_or_else(|| "(no title yet)".into());
        println!(
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
        let why = if abduco::is_slot_name(&sock.name) {
            "no record — run reconcile"
        } else {
            "not started by claude-sessions"
        };
        println!(
            "{:<10} {:<4} {:<40} --",
            sock.name,
            if sock.attached_bit { "u@" } else { "u" },
            why
        );
    }
    Ok(())
}

fn age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
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
    println!("version   : {VERSION}");
    println!("registry  : {}", registry::dir().display());
    match (m.limit, m.current) {
        (Some(limit), Some(cur)) => println!(
            "memory    : {} of {} used in this container, room for another session: {}",
            human(cur),
            human(limit),
            if m.room_for(mem::SESSION_COST) {
                "yes"
            } else {
                "no"
            }
        ),
        _ => println!("memory    : no cgroup limit readable; /proc/meminfo describes the host"),
    }
    let sockets = abduco::sockets();
    println!(
        "abduco    : {} socket(s): {}",
        sockets.len(),
        sockets
            .iter()
            .map(|s| format!(
                "{}{}",
                s.name,
                if s.attached_bit { "(attached)" } else { "" }
            ))
            .collect::<Vec<_>>()
            .join(" ")
    );
    // What Claude Code says about itself, printed beside what we think — because when the two
    // disagree, that disagreement IS the finding, and nothing else in this tool would show it.
    let ours = live::all();
    println!(
        "claude    : {} live session(s) by Claude Code's own account, {} interactive",
        ours.len(),
        ours.iter().filter(|s| s.is_interactive()).count()
    );
    for sess in &ours {
        println!(
            "            pid {} {} {} {} {}",
            sess.pid,
            sess.kind.as_deref().unwrap_or("?"),
            sess.status.as_deref().unwrap_or("?"),
            sess.session_id.as_deref().unwrap_or("-"),
            sess.cwd.as_deref().unwrap_or("-"),
        );
    }
    if let Some(sock) = abduco::socket_for("claude") {
        println!(
            "            an abduco session literally named \"claude\" exists{} — one of \
             today's `ssh <container>` logins, not one of ours",
            if sock.attached_bit { ", attached" } else { "" }
        );
    }
    for rec in registry::all()? {
        println!();
        println!(
            "{} [{:?}]{}",
            rec.slot,
            rec.state,
            if rec.registered { "" } else { " (not ours)" }
        );
        println!("  pid       : {:?} start {:?}", rec.pid, rec.proc_start);
        println!("  session   : {:?}", rec.session_id);
        println!(
            "  flags     : busy={} needs_you={} unread={} timers={}",
            rec.busy,
            rec.needs_you,
            rec.unread(),
            rec.timers.len()
        );
        if rec.last_event_ms.is_empty() {
            println!("  events    : none seen — the hooks are not installed, or not firing");
        } else {
            for (name, at) in &rec.last_event_ms {
                println!(
                    "  {name:<10}: last seen {} ago",
                    age(now.saturating_sub(*at))
                );
            }
        }
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    let mb = bytes / (1024 * 1024);
    if mb >= 1024 {
        format!("{:.1}G", mb as f64 / 1024.0)
    } else {
        format!("{mb}M")
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
    println!("{slot}: closed. the conversation is still on disk for claude --resume");
    Ok(())
}
