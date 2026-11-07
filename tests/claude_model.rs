//! The model and effort the holder says claude started with, against the **real Claude Code**.
//!
//! The holder writes them as its log's first line (`_cfg`), worked out from claude's flags,
//! the environment and the settings files, because claude's own lines never say the alias or
//! the effort, and `/model` and `/effort` mark the session's own entry by them. That is a
//! claim about how Claude Code decides, so each case here runs the real holder over the real
//! claude and checks the claim against what claude does: the model and `output_config.effort`
//! it puts in its request to the API. The API is `support/fake_api.rs`, so no request leaves
//! the machine; claude gets a scratch config, so the shared `~/.claude` is never read.
//!
//! **Which claude**, and what happens without one, as in `claude_peer.rs`.

#[allow(dead_code)]
#[path = "support/fake_api.rs"]
mod fake_api;
#[allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

use fake_api::{FakeApi, Reply};
use json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");
const WAIT: Duration = Duration::from_secs(45);

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn real_claude() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLAUDE_SESSIONS_TEST_CLAUDE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("claude"))
            .find(|p| p.is_file())
    });
    if found.is_none() {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must provide CLAUDE_SESSIONS_TEST_CLAUDE; without it these tests prove nothing"
        );
        eprintln!(
            "claude_model: no claude (CLAUDE_SESSIONS_TEST_CLAUDE, or claude on PATH); not run"
        );
    }
    found
}

/// What one run says: the holder's `_cfg` line, and the model and effort claude asked for.
struct Seen {
    cfg_model: String,
    cfg_effort: String,
    model: String,
    effort: String,
}

/// Run the real holder over the real claude with `user` and `project` settings (JSON, or
/// empty for none), `env` set and `args` passed, send one prompt, and say what happened.
fn run(tag: &str, user: &str, project: &str, env: &[(&str, &str)], args: &[&str]) -> Option<Seen> {
    let claude = real_claude()?;
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let dir = std::env::temp_dir().join(format!("cs-model-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for sub in ["cc", "work/.claude", "registry"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir.join("run"))
        .unwrap();
    if !user.is_empty() {
        fs::write(dir.join("cc/settings.json"), user).unwrap();
    }
    if !project.is_empty() {
        fs::write(dir.join("work/.claude/settings.json"), project).unwrap();
    }
    let api = FakeApi::start(Arc::new(|_| Reply::Text("ok".into())));
    let mut cmd = Command::new(BIN);
    cmd.arg("holder").args(args);
    for k in [
        "ANTHROPIC_MODEL",
        "CLAUDE_CODE_EFFORT_LEVEL",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "ZMX_SESSION",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut holder: Child = cmd
        .current_dir(dir.join("work"))
        .env("CLAUDE_SESSIONS_SLOT", "claude-1")
        .env("CLAUDE_SESSIONS_CLAUDE", &claude)
        .env("CLAUDE_SESSIONS_DIR", dir.join("registry"))
        .env("CLAUDE_CONFIG_DIR", dir.join("cc"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("ANTHROPIC_BASE_URL", &api.url)
        .env("ANTHROPIC_API_KEY", "sk-ant-test-not-a-key")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // A tap, as the relay is one, and a prompt through it.
    let sock = dir.join("registry/claude-1.sock");
    let until = Instant::now() + WAIT;
    let tap = loop {
        if let Ok(c) = UnixStream::connect(&sock) {
            break c;
        }
        assert!(Instant::now() < until, "the holder never listened");
        std::thread::sleep(Duration::from_millis(50));
    };
    tap.set_read_timeout(Some(WAIT)).unwrap();
    (&tap).write_all(b"{\"from\":0,\"run\":\"\"}\n").unwrap();
    (&tap)
        .write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n")
        .unwrap();
    let mut first = String::new();
    let mut r = BufReader::new(tap.try_clone().unwrap());
    r.read_line(&mut first).unwrap(); // the tap's header
    let request = api
        .wait_for(WAIT, |r| r.body.get("messages").is_some())
        .expect("claude asked the model");
    let _ = holder.kill();
    let _ = holder.wait();

    let log = fs::read_to_string(dir.join("registry/claude-1.log")).unwrap();
    let cfg = json::parse(log.lines().next().unwrap()).unwrap();
    assert_eq!(
        cfg.get("type").and_then(Value::as_str),
        Some("_cfg"),
        "{log}"
    );
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let seen = Seen {
        cfg_model: s(&cfg, "model"),
        cfg_effort: s(&cfg, "effort"),
        model: s(&request.body, "model"),
        effort: request
            .body
            .get("output_config")
            .map(|o| s(o, "effort"))
            .unwrap_or_default(),
    };
    let _ = fs::remove_dir_all(&dir);
    Some(seen)
}

/// The model claude asked for is the one an alias names: `claude-sonnet-5-5` for `sonnet`.
fn names(model: &str, alias: &str) -> bool {
    model.contains(alias)
}

#[test]
fn with_nothing_set_the_holder_says_default_and_the_models_own_effort() {
    let Some(s) = run("none", "", "", &[], &[]) else {
        return;
    };
    assert_eq!(s.cfg_model, "default");
    assert_eq!(
        s.cfg_effort, "",
        "the model's own: claude asked for {:?}",
        s.effort
    );
    assert!(
        !s.model.is_empty() && !s.effort.is_empty(),
        "calibration: claude asked for a model and an effort"
    );
}

#[test]
fn the_users_settings_choose_the_model_but_not_the_effort_headless_claude_uses() {
    let Some(s) = run(
        "user",
        r#"{"model":"sonnet","effortLevel":"high"}"#,
        "",
        &[],
        &[],
    ) else {
        return;
    };
    assert_eq!(s.cfg_model, "sonnet");
    assert!(names(&s.model, "sonnet"), "claude used {}", s.model);
    // 2.1.311 run headless ignores the user's effortLevel: the model's own applies, which
    // the holder says as no effort.
    assert_ne!(
        s.effort, "high",
        "claude honours the user's effortLevel now"
    );
    assert_eq!(s.cfg_effort, "", "claude used {:?}", s.effort);
}

#[test]
fn a_flag_chooses_the_effort_when_nothing_outranks_it() {
    let Some(s) = run("flag-alone", "", "", &[], &["--effort", "high"]) else {
        return;
    };
    assert_eq!(s.cfg_effort, "high");
    assert_eq!(s.effort, "high");
}

#[test]
fn the_projects_settings_outrank_the_users_as_they_do_for_claude() {
    let Some(s) = run(
        "project",
        r#"{"model":"sonnet","effortLevel":"high"}"#,
        r#"{"model":"haiku","effortLevel":"low"}"#,
        &[],
        &[],
    ) else {
        return;
    };
    assert_eq!(
        (s.cfg_model.as_str(), s.cfg_effort.as_str()),
        ("haiku", "low")
    );
    assert!(names(&s.model, "haiku"), "claude used {}", s.model);
    assert_eq!(s.effort, "low");
}

#[test]
fn the_environment_outranks_the_settings_as_it_does_for_claude() {
    let Some(s) = run(
        "env",
        r#"{"model":"sonnet","effortLevel":"high"}"#,
        "",
        &[
            ("ANTHROPIC_MODEL", "haiku"),
            ("CLAUDE_CODE_EFFORT_LEVEL", "low"),
        ],
        &[],
    ) else {
        return;
    };
    assert_eq!(
        (s.cfg_model.as_str(), s.cfg_effort.as_str()),
        ("haiku", "low")
    );
    assert!(names(&s.model, "haiku"), "claude used {}", s.model);
    assert_eq!(s.effort, "low");
}

#[test]
fn a_flag_outranks_the_environment_for_the_model_and_not_for_the_effort() {
    let Some(s) = run(
        "flag",
        "",
        "",
        &[
            ("ANTHROPIC_MODEL", "haiku"),
            ("CLAUDE_CODE_EFFORT_LEVEL", "low"),
        ],
        &["--model", "sonnet", "--effort", "high"],
    ) else {
        return;
    };
    assert_eq!(
        (s.cfg_model.as_str(), s.cfg_effort.as_str()),
        ("sonnet", "low")
    );
    assert!(names(&s.model, "sonnet"), "claude used {}", s.model);
    assert_eq!(s.effort, "low");
}
