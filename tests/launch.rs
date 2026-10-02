//! A slot started the way the menu starts one is bound by the real hook.
//!
//! The menu starts a slot as
//! `abduco -c <slot> env CLAUDE_SESSIONS_SLOT=<slot> sh -c '<wrap>' sh <stderr> claude …`,
//! and a hook binds to a slot only when its claude is the direct child of the abduco server
//! (`src/bind.rs`). The shell in the middle is there to capture stderr and must `exec` itself
//! away, or no `SessionStart` would ever bind the slot's own claude and every slot would read
//! as unbound for good. The launcher's unit tests check claude's parent is abduco; this checks
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
        let pids = std::fs::read_to_string(self.0.join("pids")).unwrap_or_default();
        for pid in pids.split_whitespace() {
            let _ = Command::new("kill")
                .args(["-9", pid])
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in abduco whose client detaches at once: a forked copy of itself — named `abduco`,
/// as a real server is — runs the command, and the client returns 0. And a stand-in claude
/// that fires a `SessionStart` through the real hook, says something on stderr, and stays up.
fn setup(tag: &str) -> Root {
    let root = std::env::temp_dir().join(format!("cs-launch-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["bin", "registry", "abduco", "work"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    script(
        &root.join("bin/abduco"),
        &format!(
            r#"[ "$1" = -c ] || exit 2
name=$2; shift 2
sock="{}/$name@test"
: > "$sock"
( "$@" </dev/null >/dev/null 2>&1; rm -f "$sock" ) &
exit 0"#,
            root.join("abduco").display()
        ),
    );
    script(
        &root.join("bin/claude"),
        &format!(
            r#"echo $$ >> "{pids}"
printf '{{"hook_event_name":"SessionStart","source":"startup","session_id":"conv-e2e","cwd":"%s"}}' "$PWD" | "{bin}" hook
echo "claude said this on stderr" >&2
exec sleep 600"#,
            pids = root.join("pids").display(),
            bin = BIN,
        ),
    );
    Root(root)
}

fn start(root: &Path, slot: &str, wrap: &str) {
    let status = Command::new(root.join("bin/abduco"))
        .arg("-c")
        .arg(slot)
        .arg("env")
        .arg(format!("CLAUDE_SESSIONS_SLOT={slot}"))
        .arg("sh")
        .arg("-c")
        .arg(wrap)
        .arg("sh")
        .arg(root.join(format!("registry/{slot}.stderr")))
        .arg(root.join("bin/claude"))
        .current_dir(root.join("work"))
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        .env("ABDUCO_SOCKET_DIR", root.join("abduco"))
        .status()
        .expect("the stand-in abduco runs");
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
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        number(&body, "pid"),
        Some(claude),
        "bound to the slot's own claude: {body}"
    );
    assert!(body.contains("\"conv-e2e\""), "{body}");
    assert!(number(&body, "proc_start").is_some(), "with its start time");
    let stderr = std::fs::read_to_string(root.0.join("registry/claude-1.stderr")).unwrap();
    assert_eq!(stderr, "claude said this on stderr\n", "stderr is captured");
}

#[test]
fn the_same_line_without_exec_is_not_bound() {
    let root = setup("nested");
    start(&root.0, "claude-2", WRAP_NESTED);
    let body = record(&root.0, "claude-2");
    assert_eq!(
        number(&body, "pid"),
        None,
        "a shell between the server and claude makes it look nested: {body}"
    );
    assert!(!body.contains("\"conv-e2e\""), "{body}");
}
