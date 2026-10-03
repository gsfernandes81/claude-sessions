//! The offloader end to end, against the real binary and real processes.
//!
//! A slot here is the shape the menu will make: an `abduco` server with a `claude` as its
//! direct child. Both are stand-ins — a shell named `abduco` running a `sleep` named `claude`,
//! by symlink, because comm is taken from the name a program was started by — so the test
//! exercises the real `/proc` walk, the real signals and the real teardown without either
//! program installed.
//!
//! **Calibrated both ways.** The same slot, attached, must survive; asked with `--dry-run`, it
//! must survive too. A test that only ever saw the process die could be passing because
//! something kills everything.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

struct Slot {
    root: PathBuf,
    server: Child,
    claude: u32,
    claude_start: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(grandchild) = child_of(self.claude) {
            let _ = Command::new("kill")
                .args(["-9", &grandchild.to_string()])
                .stderr(Stdio::null())
                .status();
        }
        let _ = Command::new("kill")
            .args(["-9", &self.claude.to_string()])
            .stderr(Stdio::null())
            .status();
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A transcript with an exchange in it: a conversation on disk to resume.
const PROMPTED: &str = r#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        stat[stat.rfind(')')? + 1..]
            .split_whitespace()
            .map(str::to_string)
            .collect(),
    )
}

/// Alive and not a zombie, with this start time.
fn alive(pid: u32, start: u64) -> bool {
    stat_fields(pid).is_some_and(|f| f[0] != "Z" && f[19].parse::<u64>().ok() == Some(start))
}

fn child_of(ppid: u32) -> Option<u32> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: u32 = e.file_name().to_string_lossy().parse().ok()?;
        let f = stat_fields(pid)?;
        (f[1].parse::<u32>().ok()? == ppid).then_some(pid)
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// An idle slot `claude-1`: stopped eleven minutes ago, nothing pending, socket `mode`, and a
/// transcript on disk for its conversation.
/// With `child`, the stand-in claude has a `sleep` running under it — work a stop would kill.
fn idle_slot(tag: &str, socket_mode: u32, child: bool) -> Slot {
    idle_slot_with(tag, socket_mode, child, true)
}

/// As [`idle_slot`], with the transcript there or not. A slot opened and never prompted has
/// none: Claude Code writes it at the first prompt (issue #5).
fn idle_slot_with(tag: &str, socket_mode: u32, child: bool, transcript: bool) -> Slot {
    let root = std::env::temp_dir().join(format!("cs-offload-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["bin", "registry", "abduco"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    symlink("/bin/sh", root.join("bin/abduco")).unwrap();
    let claude_bin = if child { "/bin/sh" } else { "/bin/sleep" };
    symlink(claude_bin, root.join("bin/claude")).unwrap();
    let claude_args = if child { "-c 'sleep 600; :'" } else { "600" };

    // `; :` keeps the shell as the parent rather than letting it exec the command, which is
    // what abduco does too: it outlives its command only long enough to notice it has gone.
    let server = Command::new(root.join("bin/abduco"))
        .arg("-c")
        .arg(format!(
            "{} {claude_args}; :",
            root.join("bin/claude").display()
        ))
        .stdin(Stdio::null())
        // The shell reports its child's signal ("Terminated", "Killed"), which is the test
        // working, not failing; it would only be noise in the CI log.
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the stand-in server");
    let deadline = Instant::now() + Duration::from_secs(5);
    let claude = loop {
        if let Some(pid) = child_of(server.id()) {
            if std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default()
                == "claude\n"
            {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in claude never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let claude_start: u64 = stat_fields(claude).unwrap()[19].parse().unwrap();

    let sock = root.join("abduco/claude-1@test");
    std::fs::write(&sock, "").unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(socket_mode)).unwrap();

    // The path is recorded either way, as the hooks record it from SessionStart on; only
    // whether the file is there differs.
    let transcript_path = root.join("conv-1.jsonl");
    if transcript {
        std::fs::write(&transcript_path, PROMPTED).unwrap();
    }
    let stop = now_ms() - 11 * 60 * 1000;
    let record = format!(
        r#"{{"slot":"claude-1","state":"live","pid":{claude},"proc_start":{claude_start},
            "session_id":"conv-1","cwd":"/workspace","transcript_path":"{}",
            "busy":false,"needs_you":false,
            "last_activity_ms":{stop},"last_stop_ms":{stop},"timers":[]}}"#,
        transcript_path.display()
    );
    std::fs::write(root.join("registry/claude-1.json"), record).unwrap();
    Slot {
        root,
        server,
        claude,
        claude_start,
    }
}

fn offload(root: &Path, extra: &[&str]) -> (bool, String) {
    let out = Command::new(BIN)
        .arg("offload")
        .args(extra)
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .env("ABDUCO_SOCKET_DIR", root.join("abduco"))
        // Never the real config directory, should a record ever fall back to the derived path.
        .env("CLAUDE_CONFIG_DIR", root.join("claude-config"))
        .output()
        .expect("offload runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn state_of(root: &Path) -> String {
    let body = std::fs::read_to_string(root.join("registry/claude-1.json")).unwrap();
    let at = body.find("\"state\"").expect("a state field");
    body[at..].split('"').nth(3).unwrap().to_string()
}

#[test]
fn an_idle_detached_slot_is_stopped_and_marked_offloaded() {
    let s = idle_slot("idle", 0o600, false);
    assert!(
        alive(s.claude, s.claude_start),
        "calibration: it starts alive"
    );
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: offloaded"), "got: {out}");
    assert!(out.contains("1 slot(s) offloaded, 0 closed"), "got: {out}");
    assert!(!alive(s.claude, s.claude_start), "the claude is gone");
    assert_eq!(state_of(&s.root), "offloaded");
    assert!(
        !s.root.join("abduco/claude-1@test").exists(),
        "the dead server's socket is cleared, or the menu would read it as attached"
    );
    let log = std::fs::read_to_string(s.root.join("registry/offload.log")).unwrap();
    assert!(log.contains("claude-1: offloaded"), "logged: {log}");
}

#[test]
fn an_idle_slot_with_no_transcript_is_stopped_and_marked_closed() {
    // Issue #5. Calibrated by the test above: the same fixture with its transcript present is
    // offloaded, so a pass here that closed would be reading the transcript, not closing all.
    let s = idle_slot_with("notranscript", 0o600, false, false);
    assert!(
        alive(s.claude, s.claude_start),
        "calibration: it starts alive"
    );
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "offload failed: {out}");
    assert!(
        out.contains("claude-1: would close, idle 11m"),
        "got: {out}"
    );
    assert!(alive(s.claude, s.claude_start), "a dry run stops nothing");
    assert_eq!(state_of(&s.root), "live");

    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: closed"), "got: {out}");
    assert!(out.contains("0 slot(s) offloaded, 1 closed"), "got: {out}");
    assert!(!alive(s.claude, s.claude_start), "the claude is gone");
    assert_eq!(state_of(&s.root), "closed");
    assert!(
        !s.root.join("abduco/claude-1@test").exists(),
        "torn down exactly as an offload is"
    );
    let log = std::fs::read_to_string(s.root.join("registry/offload.log")).unwrap();
    assert!(log.contains("claude-1: closed"), "logged: {log}");
}

#[test]
fn the_same_slot_attached_is_left_alone() {
    let s = idle_slot("attached", 0o700, false);
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: kept — attached"), "got: {out}");
    assert!(alive(s.claude, s.claude_start));
    assert_eq!(state_of(&s.root), "live");
}

#[test]
fn a_dry_run_stops_nothing() {
    let s = idle_slot("dry", 0o600, false);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: would offload"), "got: {out}");
    assert!(alive(s.claude, s.claude_start));
    assert_eq!(state_of(&s.root), "live");
}

#[test]
fn work_running_under_the_slot_keeps_it() {
    let s = idle_slot("busy", 0o600, true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child_of(s.claude).is_none() {
        assert!(
            Instant::now() < deadline,
            "the build stand-in never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(
        out.contains("claude-1: kept — sleep (pid"),
        "the sleep under it should hold it: {out}"
    );
    assert!(alive(s.claude, s.claude_start));
    assert_eq!(state_of(&s.root), "live");
}

/// Hold `path` locked (flock(1), the same advisory lock the crate takes) for `secs`.
fn hold_lock(path: &Path, secs: f64) -> Child {
    let child = Command::new("flock")
        .arg(path)
        .args(["sleep", &secs.to_string()])
        .spawn()
        .expect("flock(1) runs");
    // Let it take the lock before the test goes on.
    std::thread::sleep(Duration::from_millis(150));
    child
}

#[test]
fn a_dry_run_takes_no_lock_but_a_live_pass_does() {
    // Issue #1: a pass held each slot's lock while it read /proc, and a hook arriving then
    // was dropped. A dry run writes nothing, so it must not take the lock at all.
    let s = idle_slot("drylock", 0o600, false);
    let lock = s.root.join("registry/claude-1.lock");
    let mut holder = hold_lock(&lock, 3.0);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "offload failed: {out}");
    assert!(
        out.contains("claude-1: would offload"),
        "a dry run must decide without the lock: {out}"
    );
    // Calibration: the lock really is held — a live pass, which must take it, is kept off.
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("its lock is busy"), "got: {out}");
    assert!(alive(s.claude, s.claude_start), "nothing was stopped");
    holder.kill().ok();
    holder.wait().ok();
}

#[test]
fn a_kept_slot_never_waits_for_its_lock() {
    // The common case — a slot that is not idle — is decided from the snapshot and the
    // record alone, so a held lock does not even slow it down.
    let s = idle_slot("keptlock", 0o700, false);
    let mut holder = hold_lock(&s.root.join("registry/claude-1.lock"), 3.0);
    let started = Instant::now();
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: kept — attached"), "got: {out}");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "it waited for a lock it had no need of: {:?}",
        started.elapsed()
    );
    holder.kill().ok();
    holder.wait().ok();
}
