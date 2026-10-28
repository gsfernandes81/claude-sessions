//! The hub, on a real pseudo-terminal, over a stand-in ssh: several containers on one menu.
//!
//! Each "container" is a directory of its own — registry, zmx, Claude Code's store — and the
//! stand-in ssh runs `claude-sessions` with that directory's environment, exactly as the real
//! one would run it in that container. So everything between the hub and a container goes
//! through the real code on both sides: `agent` serving snapshots, actions sent down, a child
//! sent back up to run at the terminal through `ssh -t … agent exec`. A third container does
//! not answer: its ssh fails as ssh does, exit 255 and a line on stderr.
//!
//! Calibrated as `menu_pty.rs` is: the hub is seen to draw, then to write nothing while idle but
//! its load line —
//! through a window in which the dead container is asked again, twice — then to redraw when
//! one container's registry changes.

#[path = "support/pty.rs"]
mod pty;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

const PROMPTED: &str = r#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Field 22 of `/proc/<pid>/stat`, as text.
fn start_time(pid: &str) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 2..];
    tail.split_whitespace().nth(19).map(str::to_string)
}

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        // Each stand-in claude by pid and start time: a pid alone may be anybody's by now.
        let pids = std::fs::read_to_string(self.0.join("pids")).unwrap_or_default();
        for line in pids.lines() {
            let mut f = line.split_whitespace();
            if let (Some(pid), Some(start)) = (f.next(), f.next()) {
                if start_time(pid).as_deref() == Some(start) {
                    let _ = Command::new("kill")
                        .args(["-9", pid])
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Root {
    fn new(tag: &str) -> Root {
        let root = std::env::temp_dir().join(format!("cs-hub-pty-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // ssh, as the hub runs it: `-t|-T -o BatchMode=yes -- <host> <command…>`. A host with
        // a `down` file is refused the way ssh refuses; any other runs the command with the
        // host's environment, or a shell with none.
        script(
            &root.join("bin/ssh"),
            &format!(
                r#"while [ $# -gt 0 ]; do
  case $1 in -t|-T) shift ;; -o) shift 2 ;; --) shift; break ;; *) break ;; esac
done
host=$1; shift
d="{root}/$host"
if [ -f "$d/down" ] || [ ! -d "$d" ]; then
  echo "ssh: connect to host $host port 2222: Connection refused" >&2
  exit 255
fi
. "$d/env"
[ $# -eq 0 ] && exec sh
exec "$@""#,
                root = root.display()
            ),
        );
        Root(root)
    }

    /// A container: its registry, zmx, store and workspace, and the environment its ssh
    /// gives. Its zmx keeps a file per session, lists them as zmx does, attaches to one that
    /// is there by returning at once — a detach — and creates one that is not by running its
    /// command in a forked copy of itself, which is named `zmx` as a real daemon is.
    fn container(&self, host: &str) -> PathBuf {
        let d = self.0.join(host);
        for sub in ["registry", "zmx", "cc", "work", "bin", "cgroup"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        // A cgroup with room, so a resume here never depends on how full the machine running
        // the test is: 1 GiB used of 8.
        std::fs::write(d.join("cgroup/memory.max"), "8589934592\n").unwrap();
        std::fs::write(d.join("cgroup/memory.current"), "1073741824\n").unwrap();
        let zmx_dir = d.join("zmx");
        script(
            &d.join("bin/zmx"),
            &format!(
                r#"dir="{dir}"
case $1 in
  list)
    for f in "$dir"/*; do
      [ -f "$f" ] || continue
      printf 'name=%s\tpid=%s\tclients=0\tcreated=1\n' "${{f##*/}}" "$(cat "$f")"
    done
    exit 0 ;;
  attach)
    name=$2; shift 2
    [ -f "$dir/$name" ] && exit 0
    ( "$@" </dev/null >/dev/null 2>&1; rm -f "$dir/$name" ) &
    echo $! > "$dir/$name"
    exit 0 ;;
  kill) rm -f "$dir/$2"; exit 0 ;;
esac
exit 2"#,
                dir = zmx_dir.display()
            ),
        );
        // A claude that starts as the slot's own, says so through the real hook, and stays up.
        script(
            &d.join("bin/claude"),
            &format!(
                r#"echo "$$ $(cut -d' ' -f22 /proc/$$/stat)" >> "{pids}"
sid=$2
printf '{{"hook_event_name":"SessionStart","source":"resume","session_id":"%s","cwd":"%s"}}' "$sid" "$PWD" | "{bin}" hook
exec sleep 600"#,
                pids = self.0.join("pids").display(),
                bin = BIN,
            ),
        );
        std::fs::write(
            d.join("env"),
            format!(
                "export CLAUDE_SESSIONS_DIR='{reg}' CLAUDE_SESSIONS_ZMX='{zmx}' ZMX_DIR='{zdir}' \
                 CLAUDE_SESSIONS_CLAUDE='{claude}' CLAUDE_SESSIONS_WORKSPACE='{work}' \
                 CLAUDE_CONFIG_DIR='{cc}' CLAUDE_SESSIONS_CGROUP='{cgroup}'\n",
                reg = d.join("registry").display(),
                zmx = d.join("bin/zmx").display(),
                zdir = zmx_dir.display(),
                claude = d.join("bin/claude").display(),
                work = d.join("work").display(),
                cc = d.join("cc").display(),
                cgroup = d.join("cgroup").display(),
            ),
        )
        .unwrap();
        d
    }

    /// An offloaded slot with a conversation on disk to resume, in container `d`. Two days
    /// and an hour old, so its age reads `2d` whatever minute it is.
    fn offloaded(&self, d: &Path, slot: &str, title: &str, at: u64) {
        let transcript = d.join(format!("{slot}.jsonl"));
        std::fs::write(&transcript, PROMPTED).unwrap();
        std::fs::write(
            d.join(format!("registry/{slot}.json")),
            format!(
                r#"{{"slot":"{slot}","state":"offloaded","title":"{title}","session_id":"s-{slot}",
                    "cwd":"{work}","transcript_path":"{t}","needs_you":false,
                    "last_activity_ms":{at}}}"#,
                work = d.join("work").display(),
                t = transcript.display(),
            ),
        )
        .unwrap();
    }

    fn hub(&self, hosts: &str) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("CLAUDE_SESSIONS_HOSTS", hosts)
            // The load line read once an hour: the idle check watches everything else
            // (`menu_pty.rs` has the load line's own test).
            .env("CLAUDE_SESSIONS_LOAD_MS", "3600000")
            .env("CLAUDE_SESSIONS_SSH", self.0.join("bin/ssh"))
            .env("CLAUDE_SESSIONS_REMOTE", BIN)
            .env("CLAUDE_SESSIONS_RETRY_MS", "1500")
            // Nothing of the hub's own may stand in for a container's.
            .env("CLAUDE_SESSIONS_DIR", self.0.join("hub-registry-unused"))
            .env_remove("CLAUDE_SESSIONS_POOL");
        cmd
    }
}

/// Whether every cursor move in `out` is to the 24th line: the load line's, at 80×24. Empty
/// output passes.
fn only_the_bottom_line(out: &[u8]) -> bool {
    text(out)
        .split("\x1b[")
        .skip(1)
        .filter_map(|seq| {
            let end = seq.find(|c: char| c.is_ascii_alphabetic())?;
            (&seq[end..=end] == "H").then(|| seq[..end].to_string())
        })
        .all(|at| at.split(';').next() == Some("24"))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Well clear of a minute boundary: the minute is the one tick a container may send rows by
/// itself (the ages), and a window straddling it would see bytes the rule allows.
fn clear_of_the_minute(term: &mut pty::Pty, window: Duration) {
    let into = now_ms() % 60_000;
    let need = window.as_millis() as u64 + 4_000;
    if into + need >= 60_000 {
        std::thread::sleep(Duration::from_millis(60_000 - into + 1_000));
        term.read_for(Duration::from_millis(3_000));
    }
}

#[test]
fn the_home_lists_every_container_and_a_dead_one_costs_the_idle_screen_nothing() {
    let root = Root::new("home");
    let old = now_ms() - 2 * 86_400_000 - 3_600_000;
    let infra = root.container("infra-dev");
    root.offloaded(&infra, "claude-1", "retire the old tunnel", old);
    let or3 = root.container("or3-dev");
    root.offloaded(&or3, "claude-1", "loop on the base build", old - 1000);

    let mut term = pty::Pty::spawn(root.hub("infra-dev or3-dev down-dev"), 80, 24).unwrap();
    let first = term.read_until(b"not answering", Duration::from_secs(10));
    let first = text(&[first, term.read_for(Duration::from_millis(500))].concat());
    for want in [
        "3 containers",
        "Containers · 3",
        "infra-dev",
        "or3-dev",
        "1 offloaded",
        "down-dev",
        "not answering",
    ] {
        assert!(first.contains(want), "{want:?} not on the home: {first:?}");
    }
    assert!(
        !first.contains("retire the old tunnel"),
        "the home lists only loaded sessions under its containers, not offloaded ones: {first:?}"
    );

    // The property, through two retries of the container that is not answering.
    let window = Duration::from_secs(4);
    clear_of_the_minute(&mut term, window);
    // Nothing but the load line: a container may send a fresh reading meanwhile, whose
    // figures the line then shows — the one line an idle hub may rewrite (2026-10-27).
    let idle = term.read_for(window);
    assert!(
        only_the_bottom_line(&idle),
        "an idle hub wrote more than its load line, {} bytes: {:?}",
        idle.len(),
        text(&idle)
    );

    // Calibration: one container's change is seen, through its own agent.
    root.offloaded(&or3, "claude-2", "loop on the new build", old - 2000);
    let seen = term.read_until(b"2 offloaded", Duration::from_secs(10));
    assert!(text(&seen).contains("2 offloaded"), "{:?}", text(&seen));
    assert!(
        !only_the_bottom_line(&seen),
        "calibration: a row's change is drawn above the load line, which the idle check rejects"
    );

    // Into the dead container, and back. End is its row, the last.
    term.write(b"\x1b[F\r");
    let down = term.read_until(b"is not answering:", Duration::from_secs(5));
    let down = text(&[down, term.read_for(Duration::from_millis(300))].concat());
    assert!(
        down.contains("Connection refused"),
        "ssh's reason: {down:?}"
    );
    assert!(down.contains("Esc"), "and the way back: {down:?}");
    term.write(b"\x1b");
    let home = term.read_until(b"Containers", Duration::from_secs(5));
    assert!(text(&home).contains("Containers"), "{:?}", text(&home));

    // Into or3-dev, one row up: its sessions, one level down, and none of infra-dev's.
    term.write(b"\x1b[A\r");
    let focused = term.read_until(b"back", Duration::from_secs(5));
    let focused = text(&[focused, term.read_for(Duration::from_millis(300))].concat());
    assert!(focused.contains("loop on the new build"), "{focused:?}");
    assert!(focused.contains("Offloaded · 2"), "{focused:?}");
    assert!(
        !focused.contains("retire the old tunnel"),
        "another container's session on or3-dev's screen: {focused:?}"
    );

    term.write(b"q");
    assert!(
        term.wait(Duration::from_secs(5)).is_some(),
        "the hub did not quit on q"
    );
}

#[test]
fn a_resume_runs_in_its_container_with_the_child_at_the_hubs_terminal() {
    let root = Root::new("resume");
    let old = now_ms() - 2 * 86_400_000 - 3_600_000;
    let infra = root.container("infra-dev");
    root.offloaded(&infra, "claude-1", "retire the old tunnel", old);
    let or3 = root.container("or3-dev");
    root.offloaded(&or3, "claude-1", "loop on the base build", old - 1000);

    let mut term = pty::Pty::spawn(root.hub("infra-dev or3-dev"), 80, 24).unwrap();
    let first = term.read_until(b"or3-dev", Duration::from_secs(10));
    assert!(text(&first).contains("infra-dev"), "{:?}", text(&first));
    term.read_for(Duration::from_millis(500));

    // Into infra-dev, the first container; then Enter on its one session resumes it: the
    // container decides, the hub runs `zmx attach … claude --resume s-claude-1` back in it,
    // and the container reads what came of it.
    term.write(b"\r");
    let inside = term.read_until(b"retire the old tunnel", Duration::from_secs(5));
    assert!(
        text(&inside).contains("retire the old tunnel"),
        "{:?}",
        text(&inside)
    );
    term.read_for(Duration::from_millis(300));
    term.write(b"\r");
    let back = term.read_until(b"detached", Duration::from_secs(15));
    assert!(
        text(&back).contains("detached · it is still running"),
        "{:?}",
        text(&back)
    );
    let rec = std::fs::read_to_string(infra.join("registry/claude-1.json")).unwrap();
    assert!(
        rec.contains(r#""state": "live""#),
        "resumed in infra-dev: {rec}"
    );
    assert!(rec.contains(r#""pid":"#), "bound by its own hook: {rec}");
    let other = std::fs::read_to_string(or3.join("registry/claude-1.json")).unwrap();
    assert!(
        other.contains(r#""state":"offloaded""#) || other.contains(r#""state": "offloaded""#),
        "or3-dev's slot of the same name untouched: {other}"
    );
    term.write(b"q");
    assert!(term.wait(Duration::from_secs(5)).is_some());
}
