//! `claude-sessions statusline` as Claude Code runs it: a JSON payload on stdin, one line out,
//! exit 0 — and never held by a stdin whose writer stays open (round 1 of the 0.4.5 review).

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

/// Runs `program args` with a payload written to its stdin and the pipe left open, and says
/// whether it exited within `within`, with its stdout.
fn run_with_open_stdin(program: &str, args: &[&str], within: Duration) -> (bool, String) {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"{\"session_id\":\"x\"}").unwrap();
    let deadline = Instant::now() + within;
    let exited = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let ok = exited.is_some_and(|s| s.success());
    if exited.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    drop(stdin);
    let mut out = String::new();
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        out.push_str(&line.unwrap());
        out.push('\n');
    }
    (ok, out)
}

#[test]
fn it_prints_one_line_and_exits_although_stdin_stays_open() {
    // Calibration: a reader that waits for the end of stdin is still running at the deadline,
    // so the check can fail.
    let (cat_exited, _) = run_with_open_stdin("cat", &[], Duration::from_millis(800));
    assert!(!cat_exited, "calibration: cat should still be waiting");
    let (ok, out) = run_with_open_stdin(BIN, &["statusline"], Duration::from_secs(3));
    assert!(ok, "statusline did not exit 0 in time; printed {out:?}");
    assert_eq!(out.lines().count(), 1, "{out:?}");
    assert!(out.starts_with("RAM: "), "{out:?}");
}
