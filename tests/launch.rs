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

use std::io::Write;
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
echo marker >> "{out}"
printf '{{"hook_event_name":"SessionStart","source":"startup","session_id":"conv-e2e","cwd":"%s","transcript_path":"{transcript}"}}' "$PWD" | "{bin}" hook >> "{out}"
printf '{{"hook_event_name":"SubagentStart","agent_id":"a-e2e","agent_type":"general-purpose","session_id":"conv-e2e"}}' | "{bin}" hook >> "{out}"
echo "claude said this on stderr" >&2
exec sleep 600"#,
            pids = root.join("pids").display(),
            out = root.join("hook.out").display(),
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

/// What `ready` returns once it returns something, within 10 s; else a failure naming `what`
/// with the hook's own log.
fn wait_for<T>(root: &Path, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(t) = ready() {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "{what}; hook.log: {}",
            std::fs::read_to_string(root.join("registry/hook.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A file's text once `ready` holds for it.
fn file_when(root: &Path, path: &Path, what: &str, ready: impl Fn(&str) -> bool) -> String {
    wait_for(root, what, || {
        std::fs::read_to_string(path).ok().filter(|s| ready(s))
    })
}

/// The slot's record once the hook has written it, as text.
fn record(root: &Path, slot: &str) -> String {
    let path = root.join(format!("registry/{slot}.json"));
    file_when(root, &path, &format!("the hook never wrote {slot}"), |b| {
        b.contains("SessionStart")
    })
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
    // Idle since its start, which is what lets a slot opened and never prompted close.
    assert!(
        number(&body, "ready_ms") >= number(&body, "last_activity_ms"),
        "{body}"
    );
    // The stand-in speaks on stderr after its hooks return, so the record can land first.
    let stderr = root.0.join("registry/claude-1.stderr");
    let stderr = file_when(&root.0, &stderr, "nothing on stderr", |s| !s.is_empty());
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
    let path = root.0.join("registry/claude-1.json");
    file_when(&root.0, &path, "the agent was never listed", |b| {
        b.contains("subagent: general-purpose")
    });
    // The marker proves the capture; its being alone, that the slot's own hooks said nothing.
    let out = std::fs::read_to_string(root.0.join("hook.out")).unwrap();
    assert_eq!(out, "marker\n");
}

#[test]
fn a_record_stamped_in_another_boot_is_still_bound() {
    let root = setup("reboot");
    let far = 1u64 << 40;
    std::fs::write(
        root.0.join("registry/claude-6.json"),
        format!(
            r#"{{"slot":"claude-6","state":"closed","written":{{"conversation":{far},"life":{far},"busy":{far},"needs_you":{far},"background":{far},"wakeup":{far},"prompt":{far},"boot":"an earlier boot"}}}}"#
        ),
    )
    .unwrap();
    start(&root.0, "claude-6", WRAP);
    let body = record(&root.0, "claude-6");
    assert!(number(&body, "pid").is_some(), "{body}");
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
    let path = root.0.join("registry/claude-2.json");
    let body = file_when(&root.0, &path, "the event was never seen", |b| {
        b.contains("\"SubagentStart\"")
    });
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
        wait_for(&root.0, &format!("{slot}'s hooks never landed"), || {
            landed.exists().then_some(())
        });
        let body = std::fs::read_to_string(root.0.join(format!("registry/{slot}.json"))).unwrap();
        // Both landed, and landing order alone would read the opposite in each.
        assert!(
            body.contains("\"first_prompt\": \"go\"") && body.contains("\"Stop\""),
            "{slot}: {body}"
        );
        assert!(
            body.contains(&format!("\"busy\": {busy}")),
            "{slot}: {body}"
        );
    }
}

#[test]
fn a_hook_that_outlives_its_claude_is_stamped_from_its_fork() {
    let root = setup("orphan");
    let stop = r#"{"hook_event_name":"Stop","session_id":"conv-e2e","background_tasks":[]}"#;
    let fifo = root.0.join("late.fifo");
    script(
        &root.0.join("bin/claude"),
        &format!(
            r#"echo "$$ $(cut -d' ' -f22 /proc/$$/stat)" >> "{pids}"
printf '{{"hook_event_name":"SessionStart","source":"startup","session_id":"conv-e2e"}}' | "{bin}" hook
mkfifo "{fifo}"
"{bin}" hook < "{fifo}" &
exit 0"#,
            pids = root.0.join("pids").display(),
            fifo = fifo.display(),
            bin = BIN,
        ),
    );
    start(&root.0, "claude-5", WRAP);
    record(&root.0, "claude-5");
    let claude = std::fs::read_to_string(root.0.join("pids")).unwrap();
    let claude = claude.split_whitespace().next().unwrap().to_string();
    wait_for(&root.0, "the stand-in claude never exited", || {
        (!Path::new(&format!("/proc/{claude}")).exists()).then_some(())
    });
    std::thread::sleep(Duration::from_millis(1_200));
    let fed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&fifo)
        .unwrap()
        .write_all(stop.as_bytes())
        .unwrap();
    let path = root.0.join("registry/claude-5.json");
    let body = file_when(&root.0, &path, "the late Stop never landed", |b| {
        b.contains("\"Stop\"")
    });
    // Landing time would read as activity after claude had gone.
    let active = number(&body, "last_activity_ms").unwrap();
    assert!(active + 500 < fed, "active {active}, fed {fed}: {body}");
}
