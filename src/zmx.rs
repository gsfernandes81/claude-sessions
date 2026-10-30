//! zmx, the session holder: one daemon per slot, holding its claude across a dropped link.
//!
//! **Why zmx and not abduco** (owner, 2026-10-06). abduco's client switches the terminal to the
//! alternate screen on every attach, whatever runs inside, and on the alternate screen with no
//! mouse reporting both Windows Terminal and Termux turn the wheel or a swipe into Up/Down
//! keys — so with claude's mouse reporting off (issue #8) every scroll became prompt history,
//! and the terminal's own scrollback was never reachable at all. zmx's client sends no
//! alternate-screen switch: output lands in the terminal's own buffer. Its daemon also keeps a
//! terminal emulator fed with what the session printed, and replays it into the terminal on
//! each attach, so history survives a dropped link — measured at about 1.08× the original
//! bytes, capped near 10,000 lines (≈480 KB, ≈120 KB after ssh's compression), on every attach.
//!
//! **Everything is read through `zmx list`, never from zmx's files.** Its line per session is
//! `name=… pid=… clients=… …`, tab-separated (`pid` is the session's program, so a slot's
//! claude), or `name=… err=… status=…` for a daemon that did not answer. It removes a dead
//! daemon's socket itself when the connection is refused, so unlike abduco's socket bit it
//! never reports a corpse as attached. It answers each daemon within a second (a stopped one
//! reads `err=Timeout`), and the whole call is bounded here too: the menu is the door.
//!
//! **Every call strips `ZMX_SESSION` and `ZMX_SESSION_PREFIX`.** zmx sets the first inside
//! every session, and `zmx attach` from inside one *switches the calling terminal* to the
//! other session rather than nesting. A claude-sessions run from a slot's own shell — the
//! menu's `s`, or a tool call — must not move the terminal somebody is using.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// The most `zmx list` may take before the reading is given up as unknown. zmx bounds each
/// daemon at a second; this bounds the whole call.
const LIST_LIMIT: Duration = Duration::from_secs(3);

/// How stale the menu lets its reading get with nothing created or removed: how late a
/// session attached from another terminal shows as attached here.
pub const MENU_REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    /// The session's program — for a slot, its claude. `None` for a daemon that did not answer.
    pub pid: Option<u32>,
    /// A client is attached. False for a daemon that did not answer, which is then judged by
    /// `answered`, never by this.
    pub attached: bool,
    /// The daemon answered. One that did not (`err=Timeout`) may be alive and attached.
    pub answered: bool,
    /// When the session was created, in Unix seconds, if zmx said.
    pub created: Option<u64>,
}

/// The program to run: `CLAUDE_SESSIONS_ZMX`, else `zmx` on `PATH`.
pub fn program() -> String {
    std::env::var("CLAUDE_SESSIONS_ZMX")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "zmx".to_string())
}

/// A `zmx` command with the two variables stripped (module note) — which also keeps a daemon
/// started from inside a slot from being measured as part of it (`activity.rs`).
pub fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove("ZMX_SESSION")
        .env_remove("ZMX_SESSION_PREFIX");
    cmd
}

/// Every session zmx knows of, or `None` when zmx could not be asked or did not answer in
/// time. **`None` is not "no sessions"**: a caller deciding whether something is safe treats
/// it as "cannot rule it out".
pub fn sessions() -> Option<Vec<Session>> {
    list_from(&program(), LIST_LIMIT)
}

/// [`sessions`], asked of `program`, given `limit` to answer.
fn list_from(program: &str, limit: Duration) -> Option<Vec<Session>> {
    let mut child = command(program)
        .arg("list")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut s);
        s
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return None;
    };
    let text = reader.join().ok()?;
    status.success().then(|| parse(&text))
}

pub fn session_for(name: &str) -> Option<Option<Session>> {
    sessions().map(|all| all.into_iter().find(|s| s.name == name))
}

/// `zmx list`'s lines. A line is `[→ ]key=value\tkey=value…`; the arrow marks the calling
/// session, which never appears here because `ZMX_SESSION` is stripped, but is allowed for.
/// "no sessions found" goes to stderr, so an empty listing is empty text.
fn parse(text: &str) -> Vec<Session> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches("→").trim_start();
        let mut name = None;
        let mut pid = None;
        let mut clients = None;
        let mut created = None;
        let mut err = false;
        for field in line.split('\t') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "name" => name = Some(value.to_string()),
                "pid" => pid = value.parse::<u32>().ok(),
                "clients" => clients = value.parse::<usize>().ok(),
                "created" => created = value.parse::<u64>().ok(),
                "err" => err = true,
                _ => {}
            }
        }
        let Some(name) = name else { continue };
        let answered = !err && clients.is_some();
        out.push(Session {
            name,
            pid: pid.filter(|_| answered),
            attached: answered && clients.unwrap_or(0) > 0,
            answered,
            created,
        });
    }
    out
}

/// Where zmx keeps its sockets, resolved as zmx resolves it: `ZMX_DIR`, else
/// `$XDG_RUNTIME_DIR/zmx`, else `$TMPDIR/zmx-<uid>`, else `/tmp/zmx-<uid>`. Only ever used as a
/// change signal for the menu (`Watch`); a wrong answer costs a minute's staleness, never a
/// wrong verdict. `tests/zmx_real.rs` sees a session made by hand listed well inside that
/// minute, with `ZMX_DIR` set; the fallbacks are zmx's README, as `zmx version` prints them.
pub fn socket_dir() -> PathBuf {
    socket_dir_from(|k| std::env::var(k).ok(), uid())
}

/// [`socket_dir`] from `env` and this process's uid.
fn socket_dir_from(env: impl Fn(&str) -> Option<String>, uid: Option<u32>) -> PathBuf {
    let var = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(d) = var("ZMX_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = var("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("zmx");
    }
    let uid = uid.unwrap_or(0);
    let base = var("TMPDIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(base).join(format!("zmx-{uid}"))
}

fn uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The menu's reading of zmx, refreshed when it can have changed rather than on every poll.
///
/// Every `zmx list` connects to every daemon, and each daemon logs the connection: at the
/// menu's two-second poll that is a log wiped every few hours and a disk written every two
/// seconds, for a reading that rarely changes. A session appearing or going changes the socket
/// directory, which is a `stat`; whether one is attached elsewhere does not, and is allowed to
/// be up to [`MENU_REFRESH`] late. Anything the menu *does* re-reads at once (`force`).
#[derive(Debug, Default)]
pub struct Watch {
    seen: Option<(Instant, Option<SystemTime>)>,
    last: Vec<Session>,
}

impl Watch {
    pub fn sessions(&mut self, force: bool) -> &[Session] {
        let dir = std::fs::metadata(socket_dir())
            .and_then(|m| m.modified())
            .ok();
        self.refresh(Instant::now(), force, dir, sessions)
    }

    /// [`Watch::sessions`] at `now`, with the socket directory last modified at `dir`, asking
    /// `list` only when the reading can have changed.
    fn refresh(
        &mut self,
        now: Instant,
        force: bool,
        dir: Option<SystemTime>,
        list: impl FnOnce() -> Option<Vec<Session>>,
    ) -> &[Session] {
        let stale = match self.seen {
            None => true,
            Some((at, was)) => force || was != dir || now.duration_since(at) >= MENU_REFRESH,
        };
        if stale {
            // An unanswered listing keeps the last good one: the menu shows what it last
            // knew rather than emptying the list on a slow daemon.
            if let Some(listed) = list() {
                self.last = listed;
            }
            self.seen = Some((now, dir));
        }
        &self.last
    }
}

/// Is this one of ours? The menu names its slots `claude-<n>` and `pi-<n>` (`harness.rs`); any
/// other zmx session is somebody's own and gets marked rather than adopted.
pub fn is_slot_name(name: &str) -> bool {
    crate::harness::Harness::of_slot(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `zmx list` as zmx 0.8.1 printed it on 2026-10-06: an attached session, a detached one,
    /// a stopped daemon and a killed one.
    const LISTING: &str = "name=claude-1\tpid=738\tclients=1\tcreated=1791272098\tcwd=file://vm/workspace\tcmd=env CLAUDE_SESSIONS_SLOT=claude-1 sh -c x\n\
name=claude-2\tpid=711\tclients=0\tcreated=1791272090\tcwd=file://vm/workspace\tcmd=/bin/sh -c 'a\tb'\n\
name=stuck\terr=Timeout\tstatus=unreachable\n\
name=gone\terr=ConnectionRefused\tstatus=cleaning up\n";

    #[test]
    fn a_listing_says_who_is_attached_and_which_daemons_did_not_answer() {
        let s = parse(LISTING);
        assert_eq!(s.len(), 4);
        assert_eq!(
            s[0],
            Session {
                name: "claude-1".into(),
                pid: Some(738),
                attached: true,
                answered: true,
                created: Some(1_791_272_098),
            }
        );
        assert!(!s[1].attached, "clients=0");
        assert_eq!(
            s[1].pid,
            Some(711),
            "a tab inside cmd= does not disturb the fields before it"
        );
        assert!(!s[2].answered && s[2].pid.is_none() && !s[2].attached);
        assert!(
            !s[3].answered,
            "a refused one is reported, and zmx has removed its socket"
        );
    }

    #[test]
    fn the_calling_sessions_arrow_and_an_empty_listing_are_allowed_for() {
        let s =
            parse("→ name=a\tpid=1\tclients=0\tcreated=1\n  name=b\tpid=2\tclients=2\tcreated=1\n");
        assert_eq!(
            s.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(s[1].attached);
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_daemon_answers_only_with_a_client_count_and_no_error() {
        // An error beside a count, and a line with neither: no answer either way.
        let s = parse("name=a\tpid=1\tclients=1\terr=Timeout\nname=b\tpid=2\n");
        assert!(!s[0].answered && !s[0].attached && s[0].pid.is_none());
        assert!(!s[1].answered && s[1].pid.is_none());
    }

    #[test]
    fn the_socket_directory_is_resolved_as_zmx_resolves_it() {
        fn env<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
            move |k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
        }
        let all = [
            ("ZMX_DIR", "/z"),
            ("XDG_RUNTIME_DIR", "/run/user/7"),
            ("TMPDIR", "/t"),
        ];
        assert_eq!(socket_dir_from(env(&all), Some(7)), PathBuf::from("/z"));
        assert_eq!(
            socket_dir_from(env(&all[1..]), Some(7)),
            PathBuf::from("/run/user/7/zmx")
        );
        assert_eq!(
            socket_dir_from(env(&all[2..]), Some(7)),
            PathBuf::from("/t/zmx-7")
        );
        assert_eq!(
            socket_dir_from(env(&[]), Some(7)),
            PathBuf::from("/tmp/zmx-7")
        );
        assert_eq!(socket_dir_from(env(&[]), None), PathBuf::from("/tmp/zmx-0"));
        // A variable set but empty is unset, as zmx reads it.
        let blank = [("ZMX_DIR", ""), ("XDG_RUNTIME_DIR", ""), ("TMPDIR", "")];
        assert_eq!(
            socket_dir_from(env(&blank), Some(7)),
            PathBuf::from("/tmp/zmx-7")
        );
    }

    #[test]
    fn the_uid_is_this_processs_own() {
        use std::os::unix::fs::MetadataExt;
        let own = std::fs::metadata("/proc/self").unwrap().uid();
        assert_eq!(uid(), Some(own));
    }

    fn session(name: &str) -> Session {
        parse(&format!("name={name}\tpid=1\tclients=0\n")).remove(0)
    }

    #[test]
    fn a_watch_lists_again_only_when_the_reading_can_have_changed() {
        let t0 = Instant::now();
        let mtime = Some(SystemTime::UNIX_EPOCH);
        let mut w = Watch::default();
        let list = |names: &[&str]| {
            let out: Vec<Session> = names.iter().map(|n| session(n)).collect();
            move || Some(out)
        };
        // The first reading always lists.
        assert_eq!(w.refresh(t0, false, mtime, list(&["a"])).len(), 1);
        // Nothing changed and not yet a minute: the last listing, unasked.
        let unasked = || -> Option<Vec<Session>> { panic!("listed") };
        let just_short = t0 + MENU_REFRESH - Duration::from_millis(1);
        assert_eq!(w.refresh(just_short, false, mtime, unasked)[0].name, "a");
        // A minute on, forced, or the directory changed: listed again.
        let names = |w: &mut Watch, at, force, dir, l| {
            w.refresh(at, force, dir, l)
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&mut w, t0 + MENU_REFRESH, false, mtime, list(&["b"])),
            ["b"]
        );
        let at = t0 + MENU_REFRESH;
        assert_eq!(names(&mut w, at, true, mtime, list(&["c"])), ["c"]);
        assert_eq!(names(&mut w, at, false, None, list(&["d"])), ["d"]);
        // A listing that went unanswered keeps the last good one.
        assert_eq!(w.refresh(at, true, None, || None)[0].name, "d");
    }

    #[test]
    fn a_listing_is_only_what_zmx_answered_in_time() {
        let dir = std::env::temp_dir().join(format!("cs-zmx-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stand_in = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.to_str().unwrap().to_string()
        };
        let ok = stand_in("ok", "printf 'name=claude-1\\tpid=5\\tclients=0\\n'");
        let failed = stand_in("failed", "exit 1");
        let hung = stand_in("hung", "exec sleep 30");
        let limit = Duration::from_millis(500);
        let listed = list_from(&ok, limit).expect("answered");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "claude-1");
        assert_eq!(list_from(&failed, limit), None);
        let began = Instant::now();
        assert_eq!(list_from(&hung, limit), None);
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_harness_dash_n_is_a_slot() {
        assert!(is_slot_name("claude-1"));
        assert!(is_slot_name("claude-42"));
        assert!(is_slot_name("pi-1"));
        assert!(!is_slot_name("pi"));
        assert!(!is_slot_name("claude"));
        assert!(!is_slot_name("claude-"));
        assert!(!is_slot_name("claude-1a"));
        assert!(!is_slot_name("work"));
    }
}
