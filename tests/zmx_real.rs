//! The menu driving the **real zmx**, on a real pseudo-terminal: start, bind, detach,
//! re-attach, close — and the check v0.3.8 lacked, that nothing switches the terminal to the
//! alternate screen while a session has it.
//!
//! **Why this exists.** v0.3.8 turned claude's mouse reporting off so that swipes would scroll
//! the terminal's own buffer. Every test passed, because every test ran a stand-in for the
//! session holder; the real abduco switched the terminal to the alternate screen on attach,
//! where Termux and Windows Terminal turn a swipe into arrow keys, and the owner found scrolling
//! had become prompt history. Stand-ins prove the logic. Only the real holder proves the
//! terminal.
//!
//! **It needs zmx**: `CLAUDE_SESSIONS_TEST_ZMX` names the binary, and CI downloads the pinned
//! release with its checksum. Without it the test says so and passes — except under CI, where
//! a missing binary is a failure, so a broken download cannot make it pass for the wrong reason.

#[path = "support/pty.rs"]
mod pty;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

/// abduco's client switches to the alternate screen with this on every attach.
const ALT_SCREEN_ON: &[u8] = b"\x1b[?1049h";
/// The menu leaves its own alternate screen with this before handing the terminal over.
const ALT_SCREEN_OFF: &[u8] = b"\x1b[?1049l";

fn real_zmx() -> Option<PathBuf> {
    match std::env::var("CLAUDE_SESSIONS_TEST_ZMX") {
        Ok(p) if !p.is_empty() => {
            let p = PathBuf::from(p);
            // A process's comm is its executable's file name, and binding looks for `zmx`.
            assert_eq!(
                p.file_name().and_then(|n| n.to_str()),
                Some("zmx"),
                "CLAUDE_SESSIONS_TEST_ZMX must name a file called zmx: slots bind by that name"
            );
            Some(p)
        }
        _ => {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI must provide CLAUDE_SESSIONS_TEST_ZMX; without it this test proves nothing"
            );
            eprintln!("zmx_real: CLAUDE_SESSIONS_TEST_ZMX is not set; not run");
            None
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Whatever followed the menu's last hand-over: what the session holder and the session wrote.
fn after_hand_over(bytes: &[u8]) -> &[u8] {
    let at = bytes
        .windows(ALT_SCREEN_OFF.len())
        .rposition(|w| w == ALT_SCREEN_OFF)
        .map_or(0, |i| i + ALT_SCREEN_OFF.len());
    &bytes[at..]
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn zmx_list(zmx: &Path, dir: &Path) -> String {
    let out = Command::new(zmx)
        .arg("list")
        .env("ZMX_DIR", dir)
        .env_remove("ZMX_SESSION")
        .output()
        .expect("zmx list runs");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn wait_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Alive and not a zombie.
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat[stat.rfind(')')? + 2..].split(' ').next()?.to_string();
            Some(state != "Z")
        })
        .unwrap_or(false)
}

fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 2..].split(' ').nth(1)?.parse().ok()
}

#[test]
fn the_check_for_the_alternate_screen_can_fail() {
    // Calibration for the check below: what abduco wrote on attach is caught, and a hand-over
    // followed only by a session's own output is not.
    let abduco_attach = b"\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l\x1b[?1049h\x1b[Hclaude";
    assert!(contains(after_hand_over(abduco_attach), ALT_SCREEN_ON));
    let zmx_attach = b"\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l\x1b[2J\x1b[Hclaude";
    assert!(!contains(after_hand_over(zmx_attach), ALT_SCREEN_ON));
}

#[test]
fn the_menu_starts_attaches_and_closes_a_session_in_the_real_zmx() {
    let Some(zmx) = real_zmx() else { return };
    let root = std::env::temp_dir().join(format!("cs-zmx-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["bin", "registry", "zmx", "work", "cc"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let dir = root.join("zmx");
    let transcript = root.join("conv-z.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"a real zmx session","sessionId":"conv-z"}"#,
            "\n"
        ),
    )
    .unwrap();
    // A claude that says one line at once and another half a second later, binds itself through
    // the real hook, and stays up. The first line lands before zmx's client has connected,
    // which a first attach does not show and a re-attach replays; the second lands after, the
    // way real claude's first frame does — it takes far longer than half a second to draw.
    script(
        &root.join("bin/claude"),
        &format!(
            r#"echo "BEFORE-THE-CLIENT"
echo $$ >> "{pids}"
printf '{{"hook_event_name":"SessionStart","source":"startup","session_id":"conv-z","cwd":"%s","transcript_path":"{transcript}"}}' "$PWD" | "{bin}" hook
sleep 0.5
echo "THE-SESSION-SPOKE"
exec sleep 600"#,
            pids = root.join("pids").display(),
            transcript = transcript.display(),
            bin = BIN,
        ),
    );

    let mut cmd = Command::new(BIN);
    cmd.env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .env("CLAUDE_SESSIONS_ZMX", &zmx)
        .env("CLAUDE_SESSIONS_CLAUDE", root.join("bin/claude"))
        .env("CLAUDE_SESSIONS_WORKSPACE", root.join("work"))
        .env("CLAUDE_CONFIG_DIR", root.join("cc"))
        .env("ZMX_DIR", &dir)
        .env_remove("ZMX_SESSION");
    let mut term = pty::Pty::spawn(cmd, 80, 24).expect("spawn the menu on a pty");
    term.read_for(Duration::from_millis(800));

    // n: a slot in the real zmx, its claude the daemon's direct child, bound by the real hook.
    term.write(b"n");
    let attach = term.read_until(b"THE-SESSION-SPOKE", Duration::from_secs(10));
    assert!(
        contains(&attach, b"THE-SESSION-SPOKE"),
        "the session never drew: {:?}",
        String::from_utf8_lossy(&attach)
    );
    assert!(
        contains(&attach, ALT_SCREEN_OFF),
        "calibration: the menu was seen handing the terminal over"
    );
    assert!(
        !contains(after_hand_over(&attach), ALT_SCREEN_ON),
        "something switched to the alternate screen while the session had the terminal: {:?}",
        String::from_utf8_lossy(after_hand_over(&attach))
    );
    let claude: u32 = wait_for("the stand-in claude's pid", Duration::from_secs(5), || {
        std::fs::read_to_string(root.join("pids"))
            .ok()?
            .trim()
            .parse()
            .ok()
    });
    let daemon = parent(claude).expect("its parent");
    assert_eq!(
        comm(daemon),
        "zmx",
        "claude is the zmx daemon's direct child"
    );
    let record = wait_for("the hook's binding", Duration::from_secs(5), || {
        let body = std::fs::read_to_string(root.join("registry/claude-1.json")).ok()?;
        body.contains(&format!("\"pid\": {claude}")).then_some(body)
    });
    assert!(record.contains("conv-z"), "{record}");

    // Ctrl-\ detaches; the menu is back, and zmx holds the session with nobody attached.
    term.write(b"\x1c");
    let back = term.read_until(b"still running", Duration::from_secs(5));
    assert!(
        contains(&back, b"still running"),
        "{:?}",
        String::from_utf8_lossy(&back)
    );
    let listed = zmx_list(&zmx, &dir);
    assert!(
        listed.contains("name=claude-1") && listed.contains(&format!("pid={claude}")),
        "{listed}"
    );
    assert!(listed.contains("clients=0"), "{listed}");

    // Enter re-attaches the same claude, and zmx replays all it said — the line from before
    // the first client too.
    term.write(b"\r");
    let again = term.read_until(b"THE-SESSION-SPOKE", Duration::from_secs(5));
    assert!(
        contains(&again, b"BEFORE-THE-CLIENT") && contains(&again, b"THE-SESSION-SPOKE"),
        "the replay: {:?}",
        String::from_utf8_lossy(&again)
    );
    assert!(!contains(after_hand_over(&again), ALT_SCREEN_ON));
    assert!(
        zmx_list(&zmx, &dir).contains("clients=1"),
        "attached while it has the terminal"
    );
    term.write(b"\x1c");
    term.read_until(b"still running", Duration::from_secs(5));
    let pids = std::fs::read_to_string(root.join("pids")).unwrap();
    assert_eq!(pids.lines().count(), 1, "attached, never started twice");

    // c, y: the claude is stopped and zmx drops the session.
    term.write(b"c");
    term.read_until(b"y close", Duration::from_secs(3));
    term.write(b"y");
    let closed = term.read_until(b"closed", Duration::from_secs(15));
    assert!(
        contains(&closed, b"closed"),
        "{:?}",
        String::from_utf8_lossy(&closed)
    );
    // Dead, or a zombie its daemon has not reaped yet — zmx's daemon lingers a couple of
    // seconds after its program, and the menu counts a zombie as dead too (procinfo.rs).
    assert!(!running(claude), "the claude is stopped");
    assert!(
        !zmx_list(&zmx, &dir).contains("name=claude-1"),
        "zmx dropped the session"
    );

    // A session somebody starts by hand appears without a keypress, well inside the menu's
    // one-minute refresh: its socket changes the directory the menu watches.
    let mut hand = Command::new(&zmx);
    hand.args(["attach", "by-hand", "sleep", "600"])
        .env("ZMX_DIR", &dir)
        .env_remove("ZMX_SESSION");
    let (mut hand_master, mut hand_child) = pty::spawn_bare(hand, 80, 24).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    std::io::Write::write_all(&mut hand_master, b"\x1c").unwrap();
    let _ = hand_child.wait();
    let seen = term.read_until(b"by-hand", Duration::from_secs(8));
    assert!(
        contains(&seen, b"by-hand"),
        "the hand-made session was not listed: {:?}",
        String::from_utf8_lossy(&seen)
    );

    term.write(b"q");
    term.wait(Duration::from_secs(3));
    let _ = Command::new(&zmx)
        .args(["kill", "by-hand"])
        .env("ZMX_DIR", &dir)
        .output();
    let _ = std::fs::remove_dir_all(&root);
}
