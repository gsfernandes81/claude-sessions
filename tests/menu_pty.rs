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

/// A transcript with an exchange in it: a conversation on disk to resume.
const PROMPTED: &str = r#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn record(slot: &str, title: &str, transcript: &std::path::Path) -> String {
    // Two days and an hour old, so its age reads `2d` whichever minute the floored age clock
    // is in — exactly two days would read `1d` until the next minute turned, and redraw then,
    // legitimately. Offloaded, so no process is needed to make it read as anything — with a
    // transcript on disk, because an offloaded slot without one is not listed (issue #5).
    let at = now_ms() - 2 * 86_400_000 - 3_600_000;
    let t = transcript.display();
    format!(
        r#"{{"slot":"{slot}","state":"offloaded","title":"{title}","session_id":"s-{slot}",
            "cwd":"/workspace","transcript_path":"{t}","needs_you":false,
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
    std::fs::write(&transcript, PROMPTED).unwrap();
    std::fs::write(
        reg.join("claude-1.json"),
        record("claude-1", "retire the old tunnel", &transcript),
    )
    .unwrap();

    // Claude Code's own store: one conversation started in the workspace, listed under
    // Closed whoever started it (owner, 2026-10-03), and one started in `/workspace-old`,
    // which is not — its directory name begins like the workspace's, so only its own `cwd`
    // can rule it out (calibrated 2026-10-03: from `/elsewhere` it was skipped by name and
    // the test passed with the `cwd` check removed).
    for (cwd, id, prompt) in [
        ("/workspace", "stored-1", "a conversation run by hand"),
        (
            "/workspace-old",
            "stored-2",
            "a conversation from somewhere else",
        ),
        ("/workspace", "stored-3", "a conversation still running"),
    ] {
        let dir = root.join("cc/projects").join(cwd.replace('/', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"user","cwd":"{cwd}","sessionId":"{id}","timestamp":"2026-10-01T00:00:00.000Z","message":{{"role":"user","content":"{prompt}"}}}}"#
            ) + "\n",
        )
        .unwrap();
    }

    // `stored-3` is running: Claude Code's live-session file says so, naming this test's own
    // process, which is alive with exactly that start time. Never listed, never resumable.
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
    let start: u64 = after_comm.split(' ').nth(19).unwrap().parse().unwrap();
    std::fs::create_dir_all(root.join("cc/sessions")).unwrap();
    std::fs::write(
        root.join(format!("cc/sessions/{}.json", std::process::id())),
        format!(
            r#"{{"pid":{},"sessionId":"stored-3","procStart":{start},"kind":"interactive"}}"#,
            std::process::id()
        ),
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
    let rest = term.read_for(Duration::from_millis(500));
    let frame = String::from_utf8_lossy(&[first.clone(), rest].concat()).into_owned();
    assert!(frame.contains("Closed"), "no Closed group: {frame:?}");
    assert!(
        frame.contains("a conversation run by hand"),
        "the workspace's stored conversation is listed: {frame:?}"
    );
    assert!(
        !frame.contains("somewhere else"),
        "one started outside the workspace is not: {frame:?}"
    );
    assert!(
        !frame.contains("still running"),
        "a running conversation is not: {frame:?}"
    );

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
    // that read nothing above was really looking. Claude Code renames the conversation.
    std::fs::write(
        reg.join("claude-1.json"),
        record("claude-1", "retire the new tunnel", &transcript),
    )
    .unwrap();
    let redraw = term.read_until(b"new tunnel", Duration::from_secs(6));
    assert!(
        String::from_utf8_lossy(&redraw).contains("new tunnel"),
        "a session that was renamed was never redrawn: {:?}",
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
