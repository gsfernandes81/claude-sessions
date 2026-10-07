//! A slot started the way the menu starts one is bound by the real hook.
//!
//! The menu starts a slot as
//! `zmx attach <slot> env CLAUDE_SESSIONS_SLOT=<slot> CLAUDE_CODE_DISABLE_…=1 sh -c '<wrap>' sh <stderr> claude …`,
//! and a hook binds to a slot only when its claude is the direct child of the zmx daemon
//! (`src/bind.rs`). The shell in the middle is there to capture stderr and must `exec` itself
//! away, or no `SessionStart` would ever bind the slot's own claude and every slot would read
//! as unbound for good. The launcher's unit tests check claude's parent is zmx; this checks
//! the thing that parent is for — that `claude-sessions hook` then binds it.
//!
//! **The command line is spelled out here**, because a binary crate cannot be imported. The
//! unit test `the_start_command_is_the_one_tests_launch_rs_binds` in `src/launch.rs` pins the
//! launcher to the same line, so the two cannot drift apart silently.
//!
//! **Calibrated the other way round too.** The same line without the `exec` leaves a shell
//! between the server and claude, and the same hook must then refuse to bind — or this test
//! could be passing because the hook binds anything.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

/// `START_WRAP` in `src/launch.rs`.
const WRAP: &str = r#"e=$1; shift; exec "$@" 2>>"$e""#;

/// The same, with a shell left in place: `; :` stops a shell that execs its last command
/// (bash does, dash does not) from doing so.
const WRAP_NESTED: &str = r#"e=$1; shift; "$@" 2>>"$e"; :"#;

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        // Each stand-in by pid and start time: a pid alone may by now be another test's
        // process, or anything on the machine running the tests.
        let pids = std::fs::read_to_string(self.0.join("pids")).unwrap_or_default();
        for line in pids.lines() {
            let mut f = line.split_whitespace();
            if let (Some(pid), Some(start)) = (f.next(), f.next()) {
                if start_time(pid).as_deref() == Some(start) {
                    let _ = Command::new("kill")
                        .args(["-9", pid])
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Field 22 of `/proc/<pid>/stat`, as text.
fn start_time(pid: &str) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 2..];
    tail.split_whitespace().nth(19).map(str::to_string)
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in zmx whose client detaches at once: a forked copy of itself — named `zmx`, as a
/// real daemon is — runs the command, and the client returns 0. `tests/zmx_real.rs` does the
/// same against the real zmx when one is provided. And a stand-in claude
/// that fires a `SessionStart` through the real hook, says something on stderr, and stays up.
/// Its transcript holds a long reply and an `ai-title`, so the title the hook records can be
/// checked to be the title and never the reply (0.3.1).
fn setup(tag: &str) -> Root {
    let root = std::env::temp_dir().join(format!("cs-launch-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["bin", "registry", "zmx", "work"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("transcript.jsonl"),
        concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"A very long reply that must never be a title"}]}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"Retire the old tunnel","sessionId":"conv-e2e"}"#,
            "\n",
        ),
    )
    .unwrap();
    script(
        &root.join("bin/zmx"),
        &format!(
            r#"[ "$1" = attach ] || exit 2
name=$2; shift 2
sess="{}/$name"
: > "$sess"
( "$@" </dev/null >/dev/null 2>&1; rm -f "$sess" ) &
exit 0"#,
            root.join("zmx").display()
        ),
    );
    script(
        &root.join("bin/claude"),
        &format!(
            r#"echo "$$ $(cut -d' ' -f22 /proc/$$/stat)" >> "{pids}"
printf '{{"hook_event_name":"SessionStart","source":"startup","session_id":"conv-e2e","cwd":"%s","transcript_path":"{transcript}"}}' "$PWD" | "{bin}" hook
printf '{{"hook_event_name":"SubagentStart","agent_id":"a-e2e","agent_type":"general-purpose","session_id":"conv-e2e"}}' | "{bin}" hook
echo "claude said this on stderr" >&2
exec sleep 600"#,
            pids = root.join("pids").display(),
            bin = BIN,
            transcript = root.join("transcript.jsonl").display(),
        ),
    );
    Root(root)
}

fn start(root: &Path, slot: &str, wrap: &str) {
    let status = Command::new(root.join("bin/zmx"))
        .arg("attach")
        .arg(slot)
        .arg("env")
        .arg(format!("CLAUDE_SESSIONS_SLOT={slot}"))
        .args([
            "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1",
            "CLAUDE_CODE_DISABLE_MOUSE=1",
            "CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL=1",
        ])
        .arg("sh")
        .arg("-c")
        .arg(wrap)
        .arg("sh")
        .arg(root.join(format!("registry/{slot}.stderr")))
        .arg(root.join("bin/claude"))
        .current_dir(root.join("work"))
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .status()
        .expect("the stand-in zmx runs");
    assert!(status.success());
}

/// The slot's record once the hook has written it, as text.
fn record(root: &Path, slot: &str) -> String {
    let path = root.join(format!("registry/{slot}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(body) = std::fs::read_to_string(&path) {
            if body.contains("SessionStart") {
                return body;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the hook never wrote {slot}; hook.log: {}",
            std::fs::read_to_string(root.join("registry/hook.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn number(body: &str, key: &str) -> Option<u64> {
    let at = body.find(&format!("\"{key}\""))?;
    let rest = &body[at + key.len() + 2..];
    let rest = rest.trim_start_matches([':', ' ']);
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[test]
fn a_slot_started_as_the_menu_starts_it_is_bound_by_the_hook() {
    let root = setup("direct");
    start(&root.0, "claude-1", WRAP);
    let body = record(&root.0, "claude-1");

    let claude: u64 = std::fs::read_to_string(root.0.join("pids"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        number(&body, "pid"),
        Some(claude),
        "bound to the slot's own claude: {body}"
    );
    assert!(body.contains("\"conv-e2e\""), "{body}");
    assert!(number(&body, "proc_start").is_some(), "with its start time");
    // The stand-in speaks on stderr after its hooks return, so the record can land first.
    let deadline = Instant::now() + Duration::from_secs(10);
    let stderr = loop {
        let s =
            std::fs::read_to_string(root.0.join("registry/claude-1.stderr")).unwrap_or_default();
        if !s.is_empty() || Instant::now() > deadline {
            break s;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(stderr, "claude said this on stderr\n", "stderr is captured");
    // The row's title is the transcript's title — what Claude Code's own selector shows —
    // and never a reply (0.3.1).
    assert!(
        body.contains("\"ai_title\": \"Retire the old tunnel\""),
        "{body}"
    );
    assert!(!body.contains("very long reply"), "{body}");
    // The slot's own claude announcing an agent: bound by the process tree although the
    // payload names an `agent_id`, so the agent holds the slot (issue #10).
    let deadline = Instant::now() + Duration::from_secs(10);
    let body = loop {
        let body = std::fs::read_to_string(root.0.join("registry/claude-1.json")).unwrap();
        if body.contains("subagent: general-purpose") || Instant::now() > deadline {
            break body;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(body.contains("subagent: general-purpose"), "{body}");
}

#[test]
fn the_same_line_without_exec_is_not_bound() {
    let root = setup("nested");
    start(&root.0, "claude-2", WRAP_NESTED);
    let body = record(&root.0, "claude-2");
    assert_eq!(
        number(&body, "pid"),
        None,
        "a shell between the daemon and claude makes it look nested: {body}"
    );
    assert!(!body.contains("\"conv-e2e\""), "{body}");
    // Calibration for the title: a nested claude's transcript names nothing about the slot.
    assert!(!body.contains("ai_title"), "{body}");
    // And for the agent: once its SubagentStart has demonstrably been handled — the event is
    // in the record's times — a nested claude's agent is activity, not the slot's work.
    let deadline = Instant::now() + Duration::from_secs(10);
    let body = loop {
        let body = std::fs::read_to_string(root.0.join("registry/claude-2.json")).unwrap();
        if body.contains("\"SubagentStart\"") || Instant::now() > deadline {
            break body;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        body.contains("\"SubagentStart\""),
        "the event was seen: {body}"
    );
    assert!(
        !body.contains("subagent: general-purpose"),
        "and not listed: {body}"
    );
}

/// A stand-in claude whose second hook lands before its first: the first is forked, then held
/// on a fifo until the second has landed. `claude-3` fires a prompt and then its Stop;
/// `claude-4` the reverse.
fn claude_out_of_order(root: &Path) {
    let prompt = r#"{"hook_event_name":"UserPromptSubmit","prompt":"go","session_id":"conv-e2e"}"#;
    let stop = r#"{"hook_event_name":"Stop","session_id":"conv-e2e","background_tasks":[]}"#;
    script(
        &root.join("bin/claude"),
        &format!(
            r#"echo "$$ $(cut -d' ' -f22 /proc/$$/stat)" >> "{pids}"
case "$CLAUDE_SESSIONS_SLOT" in
  claude-3) first='{prompt}'; second='{stop}' ;;
  *) first='{stop}'; second='{prompt}' ;;
esac
late="{root}/$CLAUDE_SESSIONS_SLOT.fifo"
mkfifo "$late"
"{bin}" hook < "$late" &
sleep 0.3
printf '%s' "$second" | "{bin}" hook
printf '%s' "$first" > "$late"
wait
: > "{root}/$CLAUDE_SESSIONS_SLOT.landed"
exec sleep 600"#,
            pids = root.join("pids").display(),
            root = root.display(),
            bin = BIN,
        ),
    );
}

#[test]
fn a_hook_landing_late_is_ordered_by_when_claude_fired_it() {
    let root = setup("order");
    claude_out_of_order(&root.0);
    for (slot, busy) in [("claude-3", false), ("claude-4", true)] {
        start(&root.0, slot, WRAP);
        let landed = root.0.join(format!("{slot}.landed"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !landed.exists() {
            assert!(Instant::now() < deadline, "{slot}'s hooks never landed");
            std::thread::sleep(Duration::from_millis(20));
        }
        let body = std::fs::read_to_string(root.0.join(format!("registry/{slot}.json"))).unwrap();
        // Landing order alone would read the opposite in each.
        assert!(
            body.contains(&format!("\"busy\": {busy}")),
            "{slot}: {body}"
        );
    }
}
