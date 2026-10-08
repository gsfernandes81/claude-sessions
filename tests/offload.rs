//! The offloader end to end, against the real binary and real processes.
//!
//! A slot here is the shape the menu will make: a `zmx` daemon with a `claude` as its direct
//! child. Both are stand-ins — a shell named `zmx` running a `sleep` named `claude`, by
//! symlink, because comm is taken from the name a program was started by — and `zmx list` is
//! a script that lists the session while its claude lives, as zmx does. So the test exercises
//! the real `/proc` walk, the real signals and the real teardown without either program
//! installed.
//!
//! **Calibrated both ways.** The same slot, attached, must survive; asked with `--dry-run`, it
//! must survive too. A test that only ever saw the process die could be passing because
//! something kills everything.
//!
//! Every process these tests start has `ZMX_SESSION` stripped: run from inside a zmx session
//! named like the fixture's slot, the activity measurement would count them as its members.

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
        // Only the claude this slot recorded: once it has gone, its pid, and any child of that
        // pid, may be another test's process or anything on the machine running the tests.
        // From the leaf up, so nothing under claude is orphaned to outlive the test.
        if alive(self.claude, self.claude_start) {
            let mut tree = chain_under(self.claude);
            tree.insert(0, self.claude);
            for pid in tree.iter().rev() {
                let _ = Command::new("kill")
                    .env_remove("ZMX_SESSION")
                    .args(["-9", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
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

/// The chain of processes under `pid`, nearest first.
fn chain_under(pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut at = pid;
    while let Some(child) = child_of(at) {
        chain.push(child);
        at = child;
    }
    chain
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

/// What the stand-in claude does.
#[derive(Clone, Copy, PartialEq)]
enum Load {
    /// Waits, doing nothing: a `sleep`.
    Sleep,
    /// Waits with a `sleep` running under it.
    Child,
    /// Reads a file in a loop: work the measurement sees.
    Busy,
}

/// A slot `claude-1` doing `load`, measured quiet for eleven minutes 3 minutes ago. With
/// `transcript`, its conversation is on disk; a slot opened and never prompted has none:
/// Claude Code writes it at the first prompt (issue #5).
fn slot(tag: &str, load: Load, attached: bool, transcript: bool) -> Slot {
    let root = std::env::temp_dir().join(format!("cs-offload-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["bin", "registry", "zmx"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    symlink("/bin/sh", root.join("bin/zmx")).unwrap();
    let list = root.join("bin/zmx-list");
    std::fs::write(
        &list,
        format!(
            r#"#!/bin/sh
[ "$1" = list ] || exit 2
for f in "{}"/*; do
  [ -e "$f" ] || continue
  read pid clients < "$f"
  kill -0 "$pid" 2>/dev/null || continue
  printf 'name=%s\tpid=%s\tclients=%s\tcreated=0\n' "${{f##*/}}" "$pid" "$clients"
done
"#,
            root.join("zmx").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&list, std::fs::Permissions::from_mode(0o755)).unwrap();
    let claude_bin = if load == Load::Sleep {
        "/bin/sleep"
    } else {
        "/bin/sh"
    };
    symlink(claude_bin, root.join("bin/claude")).unwrap();
    // `; :` keeps each shell as its command's parent rather than letting it exec the command
    // away — which is what zmx does too: its daemon outlives its command by a couple of
    // seconds.
    let claude_args = match load {
        Load::Sleep => "600",
        Load::Child => "-c 'sleep 600; :'",
        Load::Busy => "-c 'while :; do read l < /etc/services; done'",
    };

    let mut server = Command::new(root.join("bin/zmx"))
        .env_remove("ZMX_SESSION")
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
        if Instant::now() >= deadline {
            let _ = server.kill();
            let _ = server.wait();
            panic!("the stand-in claude never started");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let claude_start: u64 = stat_fields(claude).unwrap()[19].parse().unwrap();
    // From here a panic drops the slot, which kills everything it started.
    let s = Slot {
        root,
        server,
        claude,
        claude_start,
    };
    let root = &s.root;
    // The sleep under claude forks after claude does; wait for it, so a pass measures it.
    while load == Load::Child && child_of(claude).is_none() {
        assert!(Instant::now() < deadline, "the child never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Busy is a megabyte read before the pass looks, far past any slot's budget, however
    // slowly a loaded runner schedules the loop.
    let read = || {
        std::fs::read_to_string(format!("/proc/{claude}/io"))
            .ok()
            .and_then(|io| {
                io.lines()
                    .find_map(|l| l.strip_prefix("rchar: ")?.parse::<u64>().ok())
            })
            .unwrap_or(0)
    };
    while load == Load::Busy && read() < 1_000_000 {
        assert!(Instant::now() < deadline, "the busy loop never got going");
        std::thread::sleep(Duration::from_millis(10));
    }

    let attached = u8::from(attached);
    std::fs::write(root.join("zmx/claude-1"), format!("{claude} {attached}\n")).unwrap();

    // The path is recorded either way, as the hooks record it from SessionStart on; only
    // whether the file is there differs.
    let transcript_path = root.join("conv-1.jsonl");
    if transcript {
        std::fs::write(&transcript_path, PROMPTED).unwrap();
    }
    let record = format!(
        r#"{{"slot":"claude-1","state":"live","pid":{claude},"proc_start":{claude_start},
            "session_id":"conv-1","cwd":"/workspace","transcript_path":"{}"}}"#,
        transcript_path.display()
    );
    std::fs::write(root.join("registry/claude-1.json"), record).unwrap();
    measured_before(&s, -180_000);
    s
}

/// A quiet slot with its conversation on disk: what the offloader stops when detached.
fn idle_slot(tag: &str, attached: bool) -> Slot {
    slot(tag, Load::Sleep, attached, true)
}

fn offload(root: &Path, extra: &[&str]) -> (bool, String) {
    let out = Command::new(BIN)
        .env_remove("ZMX_SESSION")
        .arg("offload")
        .args(extra)
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .env("CLAUDE_SESSIONS_ZMX", root.join("bin/zmx-list"))
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
    let s = idle_slot("idle", false);
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
        !out.contains("still lists"),
        "zmx dropped the session with its claude: {out}"
    );
    let log = std::fs::read_to_string(s.root.join("registry/offload.log")).unwrap();
    assert!(log.contains("claude-1: offloaded"), "logged: {log}");
}

#[test]
fn an_idle_slot_with_no_transcript_is_stopped_and_marked_closed() {
    // Issue #5. Calibrated by the test above: the same fixture with its transcript present is
    // offloaded, so a pass here that closed would be reading the transcript, not closing all.
    let s = slot("notranscript", Load::Sleep, false, false);
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

    measured_before(&s, -180_000);
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: closed"), "got: {out}");
    assert!(out.contains("0 slot(s) offloaded, 1 closed"), "got: {out}");
    assert!(!alive(s.claude, s.claude_start), "the claude is gone");
    assert_eq!(state_of(&s.root), "closed");
    assert!(
        !out.contains("still lists"),
        "torn down exactly as an offload is: {out}"
    );
    let log = std::fs::read_to_string(s.root.join("registry/offload.log")).unwrap();
    assert!(log.contains("claude-1: closed"), "logged: {log}");
}

/// Every pass measures before it decides. A slot never measured has no window yet and is
/// kept; a pass straight after is too short to count and leaves the state for the next. The
/// state lives beside the registry under a name the registry does not read as a slot.
#[test]
fn every_pass_measures_and_a_short_window_waits_for_the_next() {
    let s = idle_slot("activity", false);
    std::fs::remove_file(s.root.join("registry/activity.state")).unwrap();
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("claude-1: measured — ") && out.contains("a first reading"),
        "got: {out}"
    );
    assert!(
        out.contains("claude-1: kept — not quiet long enough"),
        "a first reading counts as active: {out}"
    );
    let state =
        std::fs::read_to_string(s.root.join("registry/activity.state")).expect("state written");
    assert!(state.contains("\"claude-1\""), "{state}");
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("too short; the next pass counts it"),
        "got: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(s.root.join("registry/activity.state")).unwrap(),
        state,
        "the short window left the first reading untouched for the next pass"
    );
    assert!(
        !out.lines()
            .any(|l| l.starts_with("activity: kept") || l.starts_with("activity: would")),
        "no phantom slot named after the state file: {out}"
    );
}

/// A slot measured before: its claude read 3 minutes ago at nothing, quiet for 11 minutes.
/// `at_offset_ms` moves the last reading (negative is the past).
fn measured_before(s: &Slot, at_offset_ms: i64) {
    let now = now_ms() as i64;
    std::fs::write(
        s.root.join("registry/activity.state"),
        format!(
            r#"{{"claude-1":{{"at":{},"last_active":{},"procs":[[{},{},0,0]],"minima":[[{},87.5]]}}}}"#,
            now + at_offset_ms,
            now - 11 * 60_000,
            s.claude,
            s.claude_start,
            now / 3_600_000
        ),
    )
    .unwrap();
}

/// The floor the state file holds for claude-1, as written.
fn floor_kept(s: &Slot) -> bool {
    std::fs::read_to_string(s.root.join("registry/activity.state"))
        .unwrap()
        .contains("87.5")
}

/// A pass past the first reading, through the real `zmx list` stand-in: quiet and detached is
/// offloadable, attached is kept, and a reading from the future is no reading.
#[test]
fn a_measured_pass_reads_attachment_and_ignores_a_future_reading() {
    let s = idle_slot("activity-quiet", false);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("claude-1: measured — ") && out.contains("claude-1: would offload, idle 11m"),
        "got: {out}"
    );

    let s = idle_slot("activity-attached", true);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("claude-1: kept — attached"), "got: {out}");

    let s = idle_slot("activity-future", false);
    measured_before(&s, 60 * 60_000);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("a first reading"), "got: {out}");
    assert!(
        floor_kept(&s),
        "a future reading starts over but keeps the slot's floor"
    );
}

/// A live record whose claude died without a word — an OOM kill — has no processes to read;
/// its floor is carried until the slot is resumed, not dropped.
#[test]
fn a_live_slot_with_nothing_to_read_keeps_its_floor() {
    let s = idle_slot("activity-dead", false);
    measured_before(&s, -180_000);
    let path = s.root.join("registry/claude-1.json");
    let body = std::fs::read_to_string(&path).unwrap();
    let dead = body.replacen(&format!("\"pid\":{}", s.claude), "\"pid\":4000000", 1);
    assert_ne!(body, dead, "calibration: the fixture took the pid");
    std::fs::write(&path, dead).unwrap();
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(floor_kept(&s), "{out}");
    assert!(
        !out.contains("claude-1: measured"),
        "nothing to read and nothing held, so nothing to say: {out}"
    );
}

/// The floor belongs to the slot's name while it has a record: a pass that sees the slot
/// offloaded keeps it, so a resume does not relearn it from busy windows. Calibration: the
/// same pass with the record gone drops it.
#[test]
fn an_offloaded_slot_keeps_its_floor() {
    let s = idle_slot("activity-offloaded", false);
    measured_before(&s, -180_000);
    let path = s.root.join("registry/claude-1.json");
    let body = std::fs::read_to_string(&path).unwrap();
    let offloaded = body.replacen("\"state\":\"live\"", "\"state\":\"offloaded\"", 1);
    assert_ne!(body, offloaded, "calibration: the fixture took the state");
    std::fs::write(&path, offloaded).unwrap();
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(floor_kept(&s), "{out}");
    std::fs::remove_file(&path).unwrap();
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(!floor_kept(&s), "calibration: no record, no floor");
}

#[test]
fn the_same_slot_attached_is_left_alone() {
    let s = idle_slot("attached", true);
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: kept — attached"), "got: {out}");
    assert!(alive(s.claude, s.claude_start));
    assert_eq!(state_of(&s.root), "live");
}

/// A pass decides only on what it read itself: one straight after another, too soon to read,
/// keeps every slot rather than act on the other's reading. Calibration: the dry run before it
/// would have offloaded the same slot.
#[test]
fn a_pass_too_soon_after_the_last_decides_nothing() {
    let s = idle_slot("too-soon", false);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok && out.contains("claude-1: would offload"), "{out}");
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "{out}");
    assert!(out.contains("too short; the next pass counts it"), "{out}");
    assert!(out.contains("claude-1: kept — not measured yet"), "{out}");
    assert!(alive(s.claude, s.claude_start));
}

/// A stop that failed leaves the slot `offloading` with its claude alive; it is measured like a
/// live one, so a later pass can finish the job. Calibration: the same slot, live.
#[test]
fn a_slot_left_offloading_is_measured_and_decided_again() {
    let s = idle_slot("left-offloading", false);
    let path = s.root.join("registry/claude-1.json");
    let body = std::fs::read_to_string(&path).unwrap();
    let left = body.replacen("\"state\":\"live\"", "\"state\":\"offloading\"", 1);
    assert_ne!(body, left, "calibration: the fixture took the state");
    std::fs::write(&path, left).unwrap();
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("claude-1: measured — "), "{out}");
    assert!(out.contains("claude-1: would offload, idle 11m"), "{out}");
}

#[test]
fn a_dry_run_stops_nothing() {
    let s = idle_slot("dry", false);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: would offload"), "got: {out}");
    assert!(alive(s.claude, s.claude_start));
    assert_eq!(state_of(&s.root), "live");
}

/// A slot doing work is kept by what it measurably does, not by what runs under it.
/// Calibration both ways: an idle `sleep` under claude holds nothing.
#[test]
fn a_slot_doing_work_is_kept_and_an_idle_child_holds_nothing() {
    let s = slot("busy", Load::Busy, false, true);
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "offload failed: {out}");
    assert!(
        out.contains("claude-1: kept — not quiet long enough"),
        "its reading loop should hold it: {out}"
    );
    assert!(alive(s.claude, s.claude_start));
    assert_eq!(state_of(&s.root), "live");

    let s = slot("idle-child", Load::Child, false, true);
    let child: Vec<(u32, u64)> = chain_under(s.claude)
        .into_iter()
        .map(|pid| (pid, stat_fields(pid).unwrap()[19].parse().unwrap()))
        .collect();
    let (ok, out) = offload(&s.root, &[]);
    // The stand-in has no terminal to hang up, so its child outlives it here; zmx's teardown
    // ends it on a real slot.
    for &(pid, start) in &child {
        if alive(pid, start) {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
    assert!(ok, "offload failed: {out}");
    assert!(out.contains("claude-1: offloaded"), "got: {out}");
}

fn keepalive(root: &Path, slot: Option<&str>, asked: &str) -> (bool, String) {
    let mut cmd = Command::new(BIN);
    cmd.env_remove("ZMX_SESSION")
        .env_remove("CLAUDE_SESSIONS_SLOT")
        .args(["keepalive", asked])
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"));
    if let Some(slot) = slot {
        cmd.env("CLAUDE_SESSIONS_SLOT", slot);
    }
    let out = cmd.output().expect("keepalive runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
    )
}

/// A quiet slot that asked to be kept alive is kept until it asks no more. Calibration: the
/// same slot is offloaded before the keep-alive and after it ends.
#[test]
fn a_keepalive_holds_a_quiet_slot_until_it_ends() {
    let s = idle_slot("keepalive", false);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(
        ok && out.contains("claude-1: would offload"),
        "calibration: {out}"
    );

    let (ok, out) = keepalive(&s.root, Some("claude-1"), "25m");
    assert!(ok, "{out}");
    assert!(out.contains("claude-1: kept alive for 25m"), "{out}");
    measured_before(&s, -180_000);
    let (ok, out) = offload(&s.root, &[]);
    assert!(ok, "{out}");
    assert!(
        out.contains("claude-1: kept — kept alive for 25m more"),
        "{out}"
    );
    assert!(alive(s.claude, s.claude_start));

    let (ok, out) = keepalive(&s.root, Some("claude-1"), "0");
    assert!(ok && out.contains("keep-alive ended"), "{out}");
    measured_before(&s, -180_000);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok && out.contains("claude-1: would offload"), "{out}");

    let (ok, out) = keepalive(&s.root, None, "25m");
    assert!(
        !ok && out.contains("not inside a claude-sessions slot"),
        "{out}"
    );
    let (ok, out) = keepalive(&s.root, Some("claude-1"), "13h");
    assert!(!ok && out.contains("longer than the 12h"), "{out}");
}

/// A session somebody attached by hand has no `CLAUDE_SESSIONS_SLOT`: a keep-alive asked from
/// inside it finds the slot as the hook does, by `ZMX_SESSION` under a zmx daemon.
/// Calibration: the same variable with no zmx above it is no slot.
#[test]
fn a_keepalive_from_a_hand_started_session_finds_its_slot() {
    let s = idle_slot("keepalive-by-hand", false);
    let ask = |session: Option<&str>, via_zmx: bool| {
        let line = format!("{BIN} keepalive 25m; :");
        let mut cmd = if via_zmx {
            let mut c = Command::new(s.root.join("bin/zmx"));
            c.args(["-c", &line]);
            c
        } else {
            let mut c = Command::new(BIN);
            c.args(["keepalive", "25m"]);
            c
        };
        cmd.env_remove("CLAUDE_SESSIONS_SLOT")
            .env_remove("ZMX_SESSION")
            .env("CLAUDE_SESSIONS_DIR", s.root.join("registry"));
        if let Some(name) = session {
            cmd.env("ZMX_SESSION", name);
        }
        let out = cmd.output().expect("keepalive runs");
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
    };
    let out = ask(None, true);
    assert!(out.contains("not inside a claude-sessions slot"), "{out}");
    // The variable alone is not a session. Not checkable from inside a real one, whose daemon
    // is above this test too.
    if std::env::var_os("ZMX_SESSION").is_none() {
        let out = ask(Some("claude-1"), false);
        assert!(out.contains("not inside a claude-sessions slot"), "{out}");
    }
    let out = ask(Some("claude-1"), true);
    assert!(out.contains("claude-1: kept alive for 25m"), "{out}");
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(
        ok && out.contains("claude-1: kept — kept alive for 25m more"),
        "{out}"
    );
}

/// Hold `path` locked (flock(1), the same advisory lock the crate takes) for `secs`.
fn hold_lock(path: &Path, secs: f64) -> Child {
    let child = Command::new("flock")
        .env_remove("ZMX_SESSION")
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
    let s = idle_slot("drylock", false);
    let lock = s.root.join("registry/claude-1.lock");
    let mut holder = hold_lock(&lock, 3.0);
    let (ok, out) = offload(&s.root, &["--dry-run"]);
    assert!(ok, "offload failed: {out}");
    assert!(
        out.contains("claude-1: would offload"),
        "a dry run must decide without the lock: {out}"
    );
    // Calibration: the lock really is held — a live pass, which must take it, is kept off.
    measured_before(&s, -180_000);
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
    let s = idle_slot("keptlock", true);
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
