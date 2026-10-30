//! The shipped pi extension, driven by node against a stand-in hook.
//!
//! pi's events reach `claude-sessions hook` one at a time, in the order pi fired them, and
//! the ones pi does not wait for never hold it. The registry orders what lands by the 10 ms
//! kernel tick of the hook's start, so two hooks spawned a microtask apart can land in
//! either order; serialising them in the extension is what keeps a prompt answered the
//! instant it opened from leaving the amber `!` behind (infra's smoke run of 2026-10-30).
//!
//! **Calibrated**: the stand-in hook sleeps on `ui_prompt_start` and not on `ui_prompt_end`,
//! so unserialised spawns log the end first; the extension before the chain fails this.
//!
//! Skipped where there is no `node`: the fleet's containers have one for pi itself.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const DRIVER: &str = r#"
const ext = (await import(process.argv[2])).default;
process.env.CLAUDE_SESSIONS_SLOT = "pi-1";
const handlers = {};
ext({ on: (name, fn) => { handlers[name] = fn; } });
let id = "s1";
const ctx = { cwd: "/w", sessionManager: { getSessionId: () => id, getSessionFile: () => null } };
const fire = (type, extra = {}) => handlers[type]({ type, ...extra }, ctx);
const fail = (why) => { console.error(why); process.exit(1); };

const started = fire("session_start", { reason: "startup" });
if (!(started instanceof Promise)) fail("session_start is waited for");
await started;
const a = fire("ui_prompt_start", { reason: "ui_prompt", kind: "confirm", title: "Trust /w?" });
const b = fire("ui_prompt_end", { reason: "ui_prompt", kind: "confirm" });
if (a !== undefined || b !== undefined) fail("a fire-and-forget event held pi");
// /new: the shutdown is fired for s1 and queued behind the prompt's hooks; the id has moved
// on by the time its hook runs.
const shut = fire("session_shutdown", { reason: "new" });
id = "s2";
await shut;
await fire("session_start", { reason: "new" });
await fire("session_shutdown", { reason: "quit" });
"#;

#[test]
fn events_reach_the_hook_one_at_a_time_in_the_order_pi_fired_them() {
    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("cs-pi-ext-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let log = root.join("hook.log");
    let hook = root.join("hook.sh");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nbody=$(cat)\ncase \"$body\" in *ui_prompt_start*) sleep 0.3;; esac\nprintf '%s\\n' \"$body\" >> '{}'\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let src: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "extensions",
        "claude-sessions.js",
    ]
    .iter()
    .collect();
    let ext = root.join("claude-sessions.mjs");
    fs::write(
        &ext,
        fs::read_to_string(&src)
            .unwrap()
            .replace("__CLAUDE_SESSIONS_BIN__", &hook.display().to_string()),
    )
    .unwrap();
    let driver = root.join("driver.mjs");
    fs::write(&driver, DRIVER).unwrap();

    let out = Command::new("node")
        .arg(&driver)
        .arg(&ext)
        .env_remove("ZMX_SESSION")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "driver: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let landed = fs::read_to_string(&log).unwrap();
    let events: Vec<(String, String)> = landed
        .lines()
        .map(|l| (field(l, "pi_event"), field(l, "session_id")))
        .collect();
    let want = [
        ("session_start", "s1"),
        ("ui_prompt_start", "s1"),
        ("ui_prompt_end", "s1"),
        ("session_shutdown", "s1"),
        ("session_start", "s2"),
        ("session_shutdown", "s2"),
    ];
    assert_eq!(
        events,
        want.iter()
            .map(|(e, s)| (e.to_string(), s.to_string()))
            .collect::<Vec<_>>(),
        "what landed, in order:\n{landed}"
    );
    assert!(
        landed.contains(r#""title":"Trust /w?""#) && landed.contains(r#""cwd":"/w""#),
        "the fields the hook reads are carried: {landed}"
    );
    let _ = fs::remove_dir_all(&root);
}

fn field(line: &str, key: &str) -> String {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat).map(|i| i + pat.len()).unwrap_or(line.len());
    line[start..].split('"').next().unwrap_or("").to_string()
}
