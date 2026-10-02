//! The pty harness, calibrated before anything trusts it.
//!
//! The menu's zero-idle-bytes test will read "nothing came out" off this harness. A harness
//! that hears nothing because it is deaf would pass that test for the wrong reason, so each
//! claim here is checked both ways: output that is there is seen, silence that is there reads
//! as silence, and the two things a terminal does besides carry bytes — deliver keys and
//! deliver a resize — reach the child.

#[path = "support/pty.rs"]
mod pty;

use pty::Pty;
use std::process::Command;
use std::time::Duration;

fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    cmd
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn output_is_seen() {
    let mut p = Pty::spawn(sh("printf 'hello from the child'; sleep 5"), 80, 24).unwrap();
    let out = p.read_until(b"hello from the child", Duration::from_secs(5));
    assert!(text(&out).contains("hello from the child"), "{out:?}");
}

#[test]
fn silence_reads_as_zero_bytes() {
    let mut p = Pty::spawn(sh("sleep 5"), 80, 24).unwrap();
    let out = p.read_for(Duration::from_secs(1));
    assert_eq!(out, b"", "a sleeping child wrote {:?}", text(&out));
    // And it was silent because it was idle, not because it had died: the zero above is
    // worth something only from a child that could still have written.
    assert!(
        p.child().try_wait().unwrap().is_none(),
        "the child exited, so its silence proves nothing"
    );
}

#[test]
fn output_after_a_quiet_spell_is_seen() {
    // The case the idle test is really about: a window of silence, then bytes. A harness that
    // stopped listening after the first quiet read would report zero forever.
    let mut p = Pty::spawn(sh("sleep 1; printf late"), 80, 24).unwrap();
    assert_eq!(p.read_for(Duration::from_millis(300)), b"");
    let out = p.read_until(b"late", Duration::from_secs(5));
    assert!(text(&out).contains("late"), "{out:?}");
}

#[test]
fn the_child_has_the_pty_as_its_terminal_at_the_size_given() {
    // stty reads its stdin, and `ps -o tty` style checks would need procps; `stty size` on
    // the slave proves both the size and that stdin is the terminal.
    let mut p = Pty::spawn(sh("stty size; sleep 5"), 100, 30).unwrap();
    let out = p.read_until(b"30 100", Duration::from_secs(5));
    assert!(text(&out).contains("30 100"), "{out:?}");
    // And /dev/tty opens, which it does only for a process with a controlling terminal.
    let mut p = Pty::spawn(sh("echo ctty > /dev/tty && printf ok; sleep 5"), 80, 24).unwrap();
    let out = p.read_until(b"ok", Duration::from_secs(5));
    assert!(text(&out).contains("ctty"), "{out:?}");
}

#[test]
fn a_resize_reaches_the_child() {
    let script = "trap 'stty size; exit' WINCH; stty size; printf ready; \
                  while :; do sleep 0.05; done";
    let mut p = Pty::spawn(sh(script), 80, 24).unwrap();
    // Calibration: the size before the resize is the one given, so the one after is news.
    let out = p.read_until(b"ready", Duration::from_secs(5));
    assert!(text(&out).contains("24 80"), "{out:?}");
    p.resize(120, 40);
    let out = p.read_until(b"40 120", Duration::from_secs(5));
    assert!(
        text(&out).contains("40 120"),
        "the child never saw the resize: {out:?}"
    );
}

#[test]
fn keys_written_arrive() {
    // Echo off, so what comes back is the child's output and not the terminal echoing the
    // keys — otherwise this would pass with a child that never read anything.
    let script = "stty -echo; printf ready; head -c 3 | tr a-z A-Z; printf done; sleep 5";
    let mut p = Pty::spawn(sh(script), 80, 24).unwrap();
    let out = p.read_until(b"ready", Duration::from_secs(5));
    assert!(text(&out).contains("ready"), "{out:?}");
    // Canonical mode hands head the line at the newline.
    p.write(b"abc\n");
    let out = p.read_until(b"done", Duration::from_secs(5));
    assert!(text(&out).contains("ABCdone"), "{out:?}");
    assert!(
        !text(&out).contains("abc"),
        "echo was on after all: {out:?}"
    );
}

#[test]
fn drop_kills_the_child() {
    let mut p = Pty::spawn(sh("sleep 60"), 80, 24).unwrap();
    let pid = p.child().id();
    drop(p);
    // Reaped, so the pid no longer names a process of ours.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    assert!(stat.is_empty(), "still there: {stat}");
}
