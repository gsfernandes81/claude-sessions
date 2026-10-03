//! The menu, run on a real pseudo-terminal: **an idle menu emits zero bytes** (`CLAUDE.md`;
//! the link it is driven over is metered).
//!
//! Calibrated in the same test, in both directions, because a silent pty would pass the idle
//! check for the wrong reason: the menu is first seen to draw, then seen to say nothing while
//! idle, then seen to redraw when the registry changes under it — the one thing besides a key
//! that is allowed to make it talk — and finally to give the terminal back on `q`.

#[path = "support/pty.rs"]
mod pty;

use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn record(slot: &str, title: &str, needs_you: bool, transcript: &std::path::Path) -> String {
    // Two days and an hour old, so its age reads `2d` whichever minute the floored age clock
    // is in — exactly two days would read `1d` until the next minute turned, and redraw then,
    // legitimately. Offloaded, so no process is needed to make it read as anything — with a
    // transcript on disk, because an offloaded slot without one is not listed (issue #5).
    let at = now_ms() - 2 * 86_400_000 - 3_600_000;
    let t = transcript.display();
    format!(
        r#"{{"slot":"{slot}","state":"offloaded","title":"{title}","session_id":"s-{slot}",
            "cwd":"/workspace","transcript_path":"{t}","needs_you":{needs_you},
            "last_activity_ms":{at},"timers":[]}}"#
    )
}

/// Wait, if need be, until the wall clock is well clear of a minute boundary: the minute is
/// the one tick the menu may redraw on by itself (the header's memory figure is refreshed
/// then), and a test window straddling it would see bytes the rule allows. Anything the menu
/// wrote while we waited — a minute's redraw, which lands on the first poll after the
/// boundary — is drained here, or the harness would hand it to the next window.
fn clear_of_the_minute(term: &mut pty::Pty, window: Duration) {
    let into = now_ms() % 60_000;
    let need = window.as_millis() as u64 + 3_000;
    if into + need >= 60_000 {
        std::thread::sleep(Duration::from_millis(60_000 - into + 1_000));
        // One registry poll (2 s) and a margin, so the minute's redraw has happened.
        term.read_for(Duration::from_millis(2_500));
    }
}

#[test]
fn an_idle_menu_emits_zero_bytes_and_a_registry_change_redraws_it() {
    let root = std::env::temp_dir().join(format!("cs-menu-pty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (reg, abd) = (root.join("registry"), root.join("abduco"));
    std::fs::create_dir_all(&reg).unwrap();
    std::fs::create_dir_all(&abd).unwrap();
    let transcript = root.join("s-claude-1.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    std::fs::write(
        reg.join("claude-1.json"),
        record("claude-1", "retire the old tunnel", false, &transcript),
    )
    .unwrap();

    let mut cmd = Command::new(BIN);
    cmd.env("CLAUDE_SESSIONS_DIR", &reg)
        .env("ABDUCO_SOCKET_DIR", &abd)
        .env("CLAUDE_SESSIONS_WORKSPACE", "/workspace")
        .env("CLAUDE_CONFIG_DIR", root.join("cc"));
    let mut term = pty::Pty::spawn(cmd, 80, 24).expect("spawn the menu on a pty");

    // Calibration 1: it draws.
    let first = term.read_until(b"retire the old tunnel", Duration::from_secs(5));
    assert!(
        String::from_utf8_lossy(&first).contains("retire the old tunnel"),
        "the menu never drew its row: {:?}",
        String::from_utf8_lossy(&first)
    );
    // Let the first frame finish arriving.
    term.read_for(Duration::from_millis(500));

    // The property: two registry polls' worth of nothing happening, and nothing written.
    let window = Duration::from_secs(5);
    clear_of_the_minute(&mut term, window);
    let idle = term.read_for(window);
    assert!(
        idle.is_empty(),
        "an idle menu wrote {} bytes: {:?}",
        idle.len(),
        String::from_utf8_lossy(&idle)
    );

    // Calibration 2: a registry change IS seen — the instrument is not deaf, and the poll
    // that read nothing above was really looking.
    std::fs::write(
        reg.join("claude-1.json"),
        record("claude-1", "retire the old tunnel", true, &transcript),
    )
    .unwrap();
    let redraw = term.read_until(b"!", Duration::from_secs(6));
    assert!(
        String::from_utf8_lossy(&redraw).contains('!'),
        "a slot that started wanting you was never redrawn: {:?}",
        String::from_utf8_lossy(&redraw)
    );

    // And after that one redraw, quiet again.
    term.read_for(Duration::from_millis(500));
    clear_of_the_minute(&mut term, Duration::from_secs(3));
    let after = term.read_for(Duration::from_secs(3));
    assert!(after.is_empty(), "{} bytes after the redraw", after.len());

    // `q` gives the terminal back: the alternate screen is left and the menu exits 0.
    term.write(b"q");
    let tail = term.read_for(Duration::from_millis(800));
    assert!(
        tail.windows(8).any(|w| w == b"\x1b[?1049l"),
        "the alternate screen was not left on quit"
    );
    let status = term
        .wait(Duration::from_secs(3))
        .expect("the menu exits on q");
    assert!(status.success(), "q should exit 0, got {status:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn too_narrow_a_terminal_is_refused_before_it_is_touched() {
    // The door falls through to a login shell on a non-zero exit, so a terminal too narrow
    // to draw in must get an exit status and a reason, and must not be left in raw mode or
    // on the alternate screen.
    let root = std::env::temp_dir().join(format!("cs-menu-narrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut cmd = Command::new(BIN);
    cmd.env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .env("ABDUCO_SOCKET_DIR", root.join("abduco"));
    let mut term = pty::Pty::spawn(cmd, 6, 24).expect("spawn");
    let out = term.read_for(Duration::from_millis(800));
    let status = term.wait(Duration::from_secs(3)).expect("it exits");
    assert!(!status.success(), "too narrow must exit non-zero");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("columns"), "it should say why: {text:?}");
    assert!(
        !out.windows(8).any(|w| w == b"\x1b[?1049h"),
        "it switched to the alternate screen before refusing"
    );
    let _ = std::fs::remove_dir_all(&root);
}
