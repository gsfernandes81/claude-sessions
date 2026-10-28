//! `claude-sessions statusline` as Claude Code runs it: a JSON payload on stdin, one line out,
//! exit 0 — and never held by a stdin whose writer stays open (round 1 of the 0.4.5 review).

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

/// Runs `program args` with `env` and `payload` written to its stdin, which is then closed if
/// `close` and otherwise left open, and says whether it exited 0 within `within`, with its
/// stdout and whether the whole payload went in.
fn run(
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    payload: Vec<u8>,
    close: bool,
    within: Duration,
) -> (bool, String, bool) {
    // No colour, so the line starts with its text whatever this machine's memory reads —
    // and NO_COLOR is checked end to end.
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().copied())
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    // On a thread: a payload larger than the pipe holds is written only as it is read.
    let (open_tx, open_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let wrote = stdin.write_all(&payload).is_ok();
        if !close {
            let _ = open_tx.send(stdin);
        }
        wrote
    });
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
    let wrote = writer.join().unwrap();
    drop(open_rx);
    let mut out = String::new();
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        out.push_str(&line.unwrap());
        out.push('\n');
    }
    (ok, out, wrote)
}

const PAYLOAD: &[u8] = b"{\"session_id\":\"x\"}";

#[test]
fn it_prints_one_line_and_exits_although_stdin_stays_open() {
    // Calibration: a reader that waits for the end of stdin is still running at the deadline,
    // so the check can fail.
    let short = Duration::from_millis(800);
    let (cat_exited, _, _) = run("cat", &[], &[], PAYLOAD.into(), false, short);
    assert!(!cat_exited, "calibration: cat should still be waiting");
    let (ok, out, _) = run(
        BIN,
        &["statusline"],
        &[],
        PAYLOAD.into(),
        false,
        Duration::from_secs(3),
    );
    assert!(ok, "statusline did not exit 0 in time; printed {out:?}");
    assert_eq!(out.lines().count(), 1, "{out:?}");
    assert!(out.starts_with("RAM: ") && !out.contains('\x1b'), "{out:?}");
}

#[test]
fn a_payload_larger_than_a_pipe_is_taken_whole_and_its_end_ends_the_wait() {
    // Four pipes' worth, then closed: every byte is read, so Claude Code never meets a broken
    // pipe, and the end of input is the end of the wait rather than more of it.
    let mut big = b"{\"session_id\":\"x\",\"pad\":\"".to_vec();
    big.resize(256 * 1024, b'a');
    big.extend_from_slice(b"\"}");
    let began = Instant::now();
    let (ok, out, wrote) = run(BIN, &["statusline"], &[], big, true, Duration::from_secs(5));
    assert!(ok, "{out:?}");
    assert!(wrote, "the payload met a closed pipe");
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out.lines().count(), 1, "{out:?}");
}

#[test]
fn the_line_says_the_pool_and_the_machines_load() {
    // A stand-in pool: 3 GiB used of 8 GiB.
    let dir = std::env::temp_dir().join(format!("cs-statusline-pool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let g = 1024u64 * 1024 * 1024;
    std::fs::write(dir.join("memory.max"), format!("{}\n", 8 * g)).unwrap();
    std::fs::write(dir.join("memory.current"), format!("{}\n", 3 * g)).unwrap();
    let pool = [("CLAUDE_SESSIONS_POOL", dir.to_str().unwrap())];
    let within = Duration::from_secs(3);
    let (ok, out, _) = run(BIN, &["statusline"], &pool, PAYLOAD.into(), true, within);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ok, "{out:?}");
    assert!(out.starts_with("RAM: 3.0G / 8.0G"), "{out:?}");
    // Every Linux has a load average and a boot id, so the load is a figure.
    assert!(
        out.contains("Load: ") && !out.contains("Load: ?"),
        "{out:?}"
    );
}
