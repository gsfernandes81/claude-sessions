//! The two promises the rest of the design leans on, tested against the real binary.
//!
//! **`hook` always exits 0.** `UserPromptSubmit` and `Stop` are *blocking* hooks: a non-zero
//! exit on the first blocks the owner's prompt, and on the second tells Claude it has more to
//! do. A bug in the registry must never wedge a session, so those two are asserted by name as
//! well as in the general sweep.
//!
//! **A record is never caught half-written.** The registry is read on the ssh path, so a
//! reader landing mid-write must still get valid JSON. That is what the temp-file-then-rename
//! in `registry::store` is for, and it is tested here with several writers at once because
//! that is the only way the failure would ever show up.

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cs-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// Run `claude-sessions hook` with `body` on stdin, in `dir`, and return its exit code.
fn run_hook(dir: &std::path::Path, slot: Option<&str>, body: &str) -> i32 {
    let mut cmd = Command::new(BIN);
    cmd.arg("hook")
        .env("CLAUDE_SESSIONS_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match slot {
        Some(s) => cmd.env("CLAUDE_SESSIONS_SLOT", s),
        None => cmd.env_remove("CLAUDE_SESSIONS_SLOT"),
    };
    let mut child = cmd.spawn().expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(body.as_bytes())
        .expect("write payload");
    child.wait().expect("wait").code().unwrap_or(-1)
}

#[test]
fn hook_exits_zero_on_the_two_blocking_events_whatever_the_payload() {
    let dir = tmpdir("blocking");
    // Each of these is a payload the hook could actually be handed: well-formed, malformed,
    // truncated, and well-formed-but-wrong-shape.
    let payloads = [
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","cwd":"/workspace"}"#,
        r#"{"hook_event_name":"Stop","session_id":"s1"}"#,
        r#"{"hook_event_name":"UserPromptSubmit","session_id":null}"#,
        r#"{"hook_event_name":"Stop""#,
        r#"{"hook_event_name":"Stop","tool_input":"not an object"}"#,
    ];
    for body in payloads {
        let code = run_hook(&dir, Some("claude-1"), body);
        assert_eq!(code, 0, "a blocking hook must exit 0; payload was {body}");
    }
}

#[test]
fn hook_exits_zero_on_rubbish_and_on_nothing_at_all() {
    let dir = tmpdir("rubbish");
    for body in [
        "",
        "   ",
        "not json at all",
        "[]",
        "null",
        "{}",
        "\u{0}\u{1}",
    ] {
        assert_eq!(
            run_hook(&dir, Some("claude-2"), body),
            0,
            "exit 0 even for {body:?}"
        );
    }
    // And with no slot in the environment at all — a hook fired by something that is not one
    // of ours, which must be a no-op rather than an error.
    assert_eq!(run_hook(&dir, None, r#"{"hook_event_name":"Stop"}"#), 0);
}

#[test]
fn a_hook_writes_a_readable_record() {
    let dir = tmpdir("writes");
    assert_eq!(
        run_hook(
            &dir,
            Some("claude-7"),
            r#"{"hook_event_name":"Stop","session_id":"abc"}"#
        ),
        0
    );
    let body = std::fs::read_to_string(dir.join("claude-7.json")).expect("the record exists");
    assert!(body.contains("\"slot\": \"claude-7\""), "got: {body}");
    // And `list` can read what `hook` wrote — the two halves of the contract meeting.
    let out = Command::new(BIN)
        .arg("list")
        .env("CLAUDE_SESSIONS_DIR", &dir)
        .output()
        .expect("list runs");
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("claude-7"),
        "list should show the slot: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn several_writers_at_once_never_leave_a_record_half_written() {
    let dir = tmpdir("writers");
    let slot = "claude-3";
    // Prime the file so the reader has something to read from the first attempt.
    run_hook(&dir, Some(slot), r#"{"hook_event_name":"Stop"}"#);

    let mut writers = Vec::new();
    for i in 0..6 {
        let dir = dir.clone();
        writers.push(std::thread::spawn(move || {
            for _ in 0..12 {
                let body =
                    format!(r#"{{"hook_event_name":"UserPromptSubmit","session_id":"w{i}"}}"#);
                run_hook(&dir, Some(slot), &body);
            }
        }));
    }

    // Read the record continuously while they write. A reader that catches a partial write
    // gets invalid JSON, which is exactly what the atomic rename is there to prevent.
    let path = dir.join(format!("{slot}.json"));
    let mut reads = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while writers.iter().any(|h| !h.is_finished()) && std::time::Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(&path) {
            assert!(
                body.contains("\"slot\"") && body.trim_end().ends_with('}'),
                "caught a partial record after {reads} reads:\n{body}"
            );
            reads += 1;
        }
    }
    for h in writers {
        h.join().expect("a writer panicked");
    }
    assert!(
        reads > 0,
        "the reader never managed a read; the test proved nothing"
    );
}

#[test]
fn version_works_on_a_bare_binary() {
    // The image's build runs this as a FATAL check, so it has to answer with no config, no
    // registry and no environment at all.
    let out = Command::new(BIN)
        .arg("--version")
        .env_clear()
        .output()
        .expect("runs with an empty environment");
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("claude-sessions "),
        "got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn hooks_config_prints_settings_that_name_the_given_binary() {
    // The image build pipes this into a managed-settings drop-in, so it has to work bare and
    // print nothing but the document.
    let out = Command::new(BIN)
        .args(["hooks-config", "/usr/local/bin/claude-sessions"])
        .env_clear()
        .output()
        .expect("runs");
    assert!(out.status.success());
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.trim_start().starts_with('{') && body.trim_end().ends_with('}'),
        "got {body}"
    );
    assert!(
        body.contains(r#""command": "/usr/local/bin/claude-sessions hook""#),
        "got {body}"
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_unknown_subcommand_fails_loudly() {
    // The door falls through to a shell on a NON-ZERO exit, so a typo must not look like
    // success — and neither may a flag `offload` does not know, which on a timer's command
    // line would otherwise be a stop nobody asked for.
    for args in [&["frobnicate"][..], &["offload", "--force"][..]] {
        let arg = args.join(" ");
        let out = Command::new(BIN).args(args).output().expect("runs");
        assert!(!out.status.success(), "{arg} should have failed");
        assert!(
            !String::from_utf8_lossy(&out.stderr).is_empty(),
            "{arg} should say why"
        );
    }
}

#[test]
fn a_prompt_outlasts_a_busy_lock_that_session_end_gives_up_on() {
    // Issue #1: every event waited only SessionEnd's 400 ms. A UserPromptSubmit dropped that
    // way leaves a working claude reading as idle. Hold the slot's lock for a second.
    let dir = tmpdir("lockwait");
    let slot = "claude-4";
    let hold = |secs: &str| {
        let child = Command::new("flock")
            .arg(dir.join(format!("{slot}.lock")))
            .args(["sleep", secs])
            .spawn()
            .expect("flock(1) runs");
        std::thread::sleep(std::time::Duration::from_millis(150));
        child
    };
    let mut holder = hold("1");
    assert_eq!(
        run_hook(
            &dir,
            Some(slot),
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"go"}"#
        ),
        0
    );
    holder.wait().ok();
    // The hook runs outside any claude here, so it binds as nested and only the event's
    // arrival is recorded — which is exactly the question: was it written, or dropped?
    let body = std::fs::read_to_string(dir.join(format!("{slot}.json")))
        .expect("the prompt waited out the lock and was recorded");
    assert!(body.contains("\"UserPromptSubmit\""), "got {body}");

    // Calibration: SessionEnd keeps its short wait, inside the 1.5 s budget all SessionEnd
    // hooks share — so the same held lock makes it give up, and nothing is written.
    let mut holder = hold("1");
    assert_eq!(
        run_hook(
            &dir,
            Some(slot),
            r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#
        ),
        0,
        "even a dropped event exits 0"
    );
    holder.wait().ok();
    let body = std::fs::read_to_string(dir.join(format!("{slot}.json"))).unwrap();
    assert!(
        !body.contains("\"SessionEnd\""),
        "SessionEnd should have given up on the held lock: {body}"
    );
}

#[test]
fn a_clear_end_takes_no_lock_and_a_dropped_event_says_which_it_was() {
    // Infra's Stage B review found SessionEnd lock failures that could not be told apart: a
    // /clear's end racing its own SessionStart, or a real end dropped. Now a clear or resume
    // end takes no lock at all, and a drop names the event and reason.
    let dir = tmpdir("namedrop");
    let slot = "claude-6";
    let hold = || {
        let child = Command::new("flock")
            .arg(dir.join(format!("{slot}.lock")))
            .args(["sleep", "2"])
            .spawn()
            .expect("flock(1) runs");
        std::thread::sleep(std::time::Duration::from_millis(150));
        child
    };
    let log = || std::fs::read_to_string(dir.join("hook.log")).unwrap_or_default();

    let mut holder = hold();
    let started = std::time::Instant::now();
    assert_eq!(
        run_hook(
            &dir,
            Some(slot),
            r#"{"hook_event_name":"SessionEnd","reason":"clear"}"#
        ),
        0
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(300),
        "a clear end must not wait for the lock: {:?}",
        started.elapsed()
    );
    assert!(!log().contains("dropped"), "nothing was dropped: {}", log());

    // Calibration: a real end, with the same lock held, does give up — and says what it was.
    assert_eq!(
        run_hook(
            &dir,
            Some(slot),
            r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#
        ),
        0
    );
    assert!(
        log().contains("claude-6: SessionEnd (logout) dropped, lock:"),
        "got: {}",
        log()
    );
    holder.wait().ok();
}
