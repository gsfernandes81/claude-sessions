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

/// `ms` as Claude Code writes a transcript's `timestamp`: `2026-10-01T00:00:00.000Z`. The
/// stored conversations are dated from now, not from a fixed day, because 30 days unused
/// archives one: a fixed date would move a listed row into the archive a month later and
/// fail this test for no change at all. Days to civil date as in Howard Hinnant's
/// `civil_from_days`.
fn iso(ms: u64) -> String {
    let (days, rem) = ((ms / 86_400_000) as i64, ms % 86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, s, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1_000 % 60,
        rem % 1_000,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

#[test]
fn iso_dates_as_claude_code_writes_them() {
    assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso(1_790_812_800_000), "2026-10-01T00:00:00.000Z");
    assert_eq!(iso(1_709_208_000_123), "2024-02-29T12:00:00.123Z");
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
        (
            "/workspace",
            "stored-4",
            "a conversation from two months ago",
        ),
    ] {
        // Two days and an hour old, or sixty: either side of the 30 days that archive one,
        // and well clear of any minute boundary in its age.
        let days = if id == "stored-4" { 60 } else { 2 };
        let ts = iso(now_ms() - days * 86_400_000 - 3_600_000);
        let dir = root.join("cc/projects").join(cwd.replace('/', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"user","cwd":"{cwd}","sessionId":"{id}","timestamp":"{ts}","message":{{"role":"user","content":"{prompt}"}}}}"#
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
    // Unused for more than 30 days, it is archived on its own (owner, 2026-10-03): behind
    // the Archived heading, which is shut when the menu starts.
    assert!(
        frame.contains("Archived · 1"),
        "the archive's heading: {frame:?}"
    );
    assert!(
        !frame.contains("two months ago"),
        "an archived conversation is behind the shut heading: {frame:?}"
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

/// The spinner while a session closes (owner, 2026-10-04): it shows only after a quarter of a
/// second, steps through Compose's frames one at a time with none skipped, at an even pace,
/// writes a line or two per frame rather than the screen, and leaves the menu idle and silent
/// once the close is done.
#[test]
fn closing_a_session_turns_the_spinner_smoothly_then_falls_silent() {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let root = std::env::temp_dir().join(format!("cs-menu-spin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (reg, abd) = (root.join("registry"), root.join("abduco"));
    std::fs::create_dir_all(&reg).unwrap();
    std::fs::create_dir_all(&abd).unwrap();

    // A claude that ignores TERM, so the close waits out its five-second grace and the
    // spinner has time to turn. The ignore is inherited across the exec.
    let mut stubborn = Command::new("sh")
        .args(["-c", "trap '' TERM; exec sleep 60"])
        .spawn()
        .unwrap();
    let pid = stubborn.id();
    std::thread::sleep(Duration::from_millis(100));
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
    let start: u64 = after_comm.split(' ').nth(19).unwrap().parse().unwrap();
    let at = now_ms() - 3_600_000;
    std::fs::write(
        reg.join("claude-1.json"),
        format!(
            r#"{{"slot":"claude-1","state":"live","pid":{pid},"proc_start":{start},
                "session_id":"s-1","cwd":"/workspace","title":"a stubborn session",
                "needs_you":false,"last_activity_ms":{at},"timers":[]}}"#
        ),
    )
    .unwrap();

    let mut cmd = Command::new(BIN);
    cmd.env("CLAUDE_SESSIONS_DIR", &reg)
        .env("ABDUCO_SOCKET_DIR", &abd)
        .env("CLAUDE_SESSIONS_WORKSPACE", "/workspace")
        .env("CLAUDE_CONFIG_DIR", root.join("cc"));
    let mut term = pty::Pty::spawn(cmd, 40, 24).expect("spawn the menu on a pty");
    let first = term.read_until(b"a stubborn session", Duration::from_secs(5));
    assert!(String::from_utf8_lossy(&first).contains("a stubborn session"));
    term.read_for(Duration::from_millis(300));

    // c asks; y closes.
    term.write(b"c");
    term.read_until(b"y close", Duration::from_secs(3));
    term.write(b"y");
    let pressed = std::time::Instant::now();
    let chunks = term.read_timed(Duration::from_secs(7));

    // The frames, in order, each with when it first arrived: the spinner is drawn on its row
    // and on the status line, so a frame is a run of one glyph.
    let mut frames: Vec<(usize, std::time::Instant, usize)> = Vec::new();
    for (when, chunk) in &chunks {
        let text = String::from_utf8_lossy(chunk);
        for c in text.chars() {
            if let Some(f) = FRAMES.iter().position(|g| *g == c) {
                if frames.last().map(|l| l.0) != Some(f) {
                    frames.push((f, *when, 0));
                }
            }
        }
        if let Some(last) = frames.last_mut() {
            last.2 += chunk.len();
        }
    }
    let all = String::from_utf8_lossy(&chunks.iter().flat_map(|c| c.1.clone()).collect::<Vec<_>>())
        .into_owned();
    assert!(
        all.contains("closing session"),
        "it says what it is doing: {all:?}"
    );
    assert!(
        frames.len() >= 30,
        "about five seconds of frames, got {}",
        frames.len()
    );

    // Not before a quarter of a second.
    let shown = frames[0].1.duration_since(pressed);
    assert!(shown >= Duration::from_millis(200), "shown after {shown:?}");
    assert_eq!(frames[0].0, 0, "it starts on the first frame");

    // One step at a time: never a frame missed out.
    for pair in frames.windows(2) {
        assert_eq!(
            pair[1].0,
            (pair[0].0 + 1) % FRAMES.len(),
            "a frame was skipped: {:?}",
            frames.iter().map(|f| FRAMES[f.0]).collect::<String>()
        );
    }

    // An even pace: a frame every 80 ms, give or take what a loaded test machine adds.
    let gaps: Vec<u128> = frames
        .windows(2)
        .map(|p| p[1].1.duration_since(p[0].1).as_millis())
        .collect();
    let mean = gaps.iter().sum::<u128>() / gaps.len() as u128;
    assert!((70..=100).contains(&mean), "mean gap {mean} ms: {gaps:?}");
    let ragged = gaps.iter().filter(|g| !(40..=140).contains(*g)).count();
    assert!(ragged * 10 <= gaps.len(), "uneven frames: {gaps:?}");

    // A line or two per frame, not the screen.
    let per_frame = frames[1..frames.len() - 1]
        .iter()
        .map(|f| f.2)
        .max()
        .unwrap();
    assert!(per_frame < 400, "{per_frame} bytes in one frame");

    // Done: the stubborn claude was stopped, and the menu says so and then says nothing.
    let mut waited = 0;
    while stubborn.try_wait().unwrap().is_none() && waited < 50 {
        std::thread::sleep(Duration::from_millis(100));
        waited += 1;
    }
    assert!(
        stubborn.try_wait().unwrap().is_some(),
        "the close stopped it"
    );
    assert!(all.contains("closed"), "{all:?}");
    let idle = term.read_for(Duration::from_secs(3));
    assert!(
        idle.is_empty(),
        "an idle menu wrote {} bytes after the close",
        idle.len()
    );

    term.write(b"q");
    term.wait(Duration::from_secs(3))
        .expect("the menu exits on q");
    let _ = std::fs::remove_dir_all(&root);
}

/// The first frame while the list is still being read (owner, 2026-10-04): drawn at once, a
/// spinner where the list will go, and the list in its place when the reading is done. The
/// reading is held open by a named pipe in the registry, which blocks until it is written.
#[test]
fn a_slow_first_reading_draws_the_frame_with_a_spinner_then_the_list() {
    let root = std::env::temp_dir().join(format!("cs-menu-first-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (reg, abd) = (root.join("registry"), root.join("abduco"));
    std::fs::create_dir_all(&reg).unwrap();
    std::fs::create_dir_all(&abd).unwrap();
    let pipe = reg.join("claude-1.json");
    let made = Command::new("mkfifo").arg(&pipe).status().unwrap();
    assert!(made.success(), "mkfifo");

    let mut cmd = Command::new(BIN);
    cmd.env("CLAUDE_SESSIONS_DIR", &reg)
        .env("ABDUCO_SOCKET_DIR", &abd)
        .env("CLAUDE_SESSIONS_WORKSPACE", "/workspace")
        .env("CLAUDE_CONFIG_DIR", root.join("cc"));
    let mut term = pty::Pty::spawn(cmd, 40, 24).expect("spawn the menu on a pty");
    let first = term.read_until(b"reading sessions", Duration::from_secs(3));
    let first = String::from_utf8_lossy(&first).into_owned();
    assert!(first.contains("reading sessions"), "{first:?}");
    assert!(
        first.contains('⠋'),
        "the spinner, from its first frame: {first:?}"
    );
    assert!(
        first.contains(" shell"),
        "the footer is drawn already: {first:?}"
    );
    // It turns while it waits.
    let turning = String::from_utf8_lossy(&term.read_for(Duration::from_millis(400))).into_owned();
    assert!(turning.contains('⠙'), "{turning:?}");

    // Let the reading finish: the record arrives through the pipe.
    let at = now_ms() - 3_600_000;
    let body = format!(
        r#"{{"slot":"claude-1","state":"offloaded","title":"behind the pipe","session_id":"s-1",
            "cwd":"/workspace","needs_you":false,"last_activity_ms":{at},"timers":[]}}"#
    );
    std::fs::write(&pipe, &body).unwrap();
    // A regular file in the pipe's place at once, before the next reading two seconds on:
    // that reading would open the pipe again and wait for ever, and the menu with it (it
    // passed locally and failed in CI, where the next reading came before the q).
    let swap = reg.join(".swap");
    std::fs::write(&swap, &body).unwrap();
    std::fs::rename(&swap, &pipe).unwrap();
    // An offloaded slot is listed only with a conversation on disk; this one has none, so
    // the list it settles on is the empty one — the point is that the reading arrived.
    let after = term.read_until(b"nothing open", Duration::from_secs(3));
    let after = String::from_utf8_lossy(&after).into_owned();
    assert!(
        after.contains("nothing open"),
        "the list replaced the spinner: {after:?}"
    );
    // Past the next reading (every two seconds), which must find the file and carry on.
    let idle = term.read_for(Duration::from_secs(3));
    assert!(idle.is_empty(), "the spinner stopped: {} bytes", idle.len());

    term.write(b"q");
    term.wait(Duration::from_secs(3))
        .expect("the menu exits on q");
    let _ = std::fs::remove_dir_all(&root);
}
