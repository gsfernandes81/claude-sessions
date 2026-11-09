//! A remote-view slot as the hub reaches it: `claude-sessions holder` over a stand-in claude,
//! and `claude-sessions tap` into it, as ssh runs it for the relay.
//!
//! The holder's core is tested in `src/holder.rs`; this is the two processes and the slot's
//! record between them: the tap carries lines both ways, a connected view stamps the record
//! as it comes and goes, and an answer from the view clears the slot's amber mark.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

/// Only for a failure: nothing here waits on a clock to pass.
const DEADLINE: Duration = Duration::from_secs(30);

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let end = Instant::now() + DEADLINE;
    while !ok() {
        assert!(Instant::now() < end, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A number field of the slot's record, as its JSON says it.
fn record_field(root: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(root.join("registry/claude-7.json")).ok()?;
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = text[at..].trim_start();
    let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

#[test]
fn a_tap_carries_lines_both_ways_stamps_the_slot_and_an_answer_clears_its_mark() {
    let root = std::env::temp_dir().join(format!("cs-holder-tap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("registry")).unwrap();
    // A claude waiting on the owner, never yet looked at.
    std::fs::write(
        root.join("registry/claude-7.json"),
        r#"{"slot":"claude-7","needs_you":true,"last_attach_ms":0}"#,
    )
    .unwrap();
    // A claude that writes back each line it is given.
    let claude = root.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nwhile IFS= read -r l; do printf '%s\\n' \"$l\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = |cmd: &mut Command| {
        cmd.env("CLAUDE_SESSIONS_DIR", root.join("registry"))
            .env("CLAUDE_CONFIG_DIR", root.join("config"))
            .env("HOME", &root)
            .current_dir(&root);
    };
    let mut holder = Command::new(BIN);
    holder
        .arg("holder")
        .env("CLAUDE_SESSIONS_SLOT", "claude-7")
        .env("CLAUDE_SESSIONS_CLAUDE", &claude)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env(&mut holder);
    let mut holder = holder.spawn().unwrap();
    let sock = root.join("registry/claude-7.sock");
    wait_for("the holder never listened", || sock.exists());

    let mut tap = Command::new(BIN);
    tap.args(["tap", "claude-7"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    env(&mut tap);
    let mut tap = tap.spawn().unwrap();
    let mut to = tap.stdin.take().unwrap();
    let mut from = BufReader::new(tap.stdout.take().unwrap());
    let mut next = || {
        let mut l = String::new();
        from.read_line(&mut l).unwrap();
        l
    };
    assert!(next().contains("\"run\""), "the holder's header");
    assert!(next().starts_with("1\t"), "the holder's `_cfg` line");
    // Connected: the slot is kept, and the owner has seen it.
    assert!(record_field(&root, "keep_until_ms").is_some());
    assert_ne!(record_field(&root, "last_attach_ms").as_deref(), Some("0"));

    // claude asks; the view's line goes in, and claude's comes out.
    let ask = r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#;
    writeln!(to, "{ask}").unwrap();
    assert!(next().starts_with("2\t"), "what the view sent");
    assert!(next().starts_with("3\t"), "what claude wrote");
    // Answered from the view: the amber mark goes now, not at the turn's end.
    let yes = r#"{"type":"control_response","response":{"subtype":"success","request_id":"p1","response":{"behavior":"allow"}}}"#;
    writeln!(to, "{yes}").unwrap();
    wait_for("the answer left the mark", || {
        record_field(&root, "needs_you").as_deref() == Some("false")
    });

    // As if the owner had not looked since: going stamps it again.
    let text = std::fs::read_to_string(root.join("registry/claude-7.json")).unwrap();
    let at = record_field(&root, "last_attach_ms").unwrap();
    std::fs::write(
        root.join("registry/claude-7.json"),
        text.replace(
            &format!("\"last_attach_ms\": {at}"),
            "\"last_attach_ms\": 0",
        ),
    )
    .unwrap();
    assert_eq!(
        record_field(&root, "last_attach_ms").as_deref(),
        Some("0"),
        "calibration"
    );
    // The relay gone: the tap goes, cleanly.
    drop(to);
    let end = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(s) = tap.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < end, "the tap outlived its input");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "{status}");
    assert_ne!(record_field(&root, "last_attach_ms").as_deref(), Some("0"));

    // A record an older release rewrote says nothing of the holder; its socket answering
    // still makes the running slot the remote view's to open.
    let pid = holder.id().to_string();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    std::fs::write(
        root.join("registry/claude-7.json"),
        format!(r#"{{"slot":"claude-7","state":"live","pid":{pid},"proc_start":{start}}}"#),
    )
    .unwrap();
    let mut open = Command::new(BIN);
    open.args(["remote", "open", "claude-7"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    env(&mut open);
    let opened = open.output().unwrap();
    let first = String::from_utf8_lossy(&opened.stdout);
    assert_eq!(
        first.lines().next(),
        Some(r#"{"slot":"claude-7"}"#),
        "{first}"
    );

    holder.kill().unwrap();
    holder.wait().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
