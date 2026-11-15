//! A `!` command from the remote view, against the **real Claude Code**: run by claude beside
//! the conversation (`bash_command`), then added to it by the holder, so the model reads the
//! command and what it printed with the next prompt, as Claude Code's own `!` has it — and
//! reads each once, though claude echoes the holder's lines too (`docs/remote-view.md`).
//!
//! The API is `support/fake_api.rs`, so no request leaves the machine; claude gets a scratch
//! config, so the shared `~/.claude` is never read. **Which claude**, and what happens without
//! one, as in `claude_peer.rs`.

#[allow(dead_code)]
#[path = "support/fake_api.rs"]
mod fake_api;
#[allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

use fake_api::{FakeApi, Reply};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");
const WAIT: Duration = Duration::from_secs(45);

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
            "claude_bang: no claude (CLAUDE_SESSIONS_TEST_CLAUDE, or claude on PATH); not run"
        );
    }
    found
}

#[test]
fn a_bang_command_reaches_the_model_once_with_what_it_printed() {
    let Some(claude) = real_claude() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("cs-bang-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for sub in ["cc", "work", "registry"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir.join("run"))
        .unwrap();
    let api = FakeApi::start(Arc::new(|_| Reply::Text("ok".into())));
    let mut cmd = Command::new(BIN);
    for k in [
        "ANTHROPIC_MODEL",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "ZMX_SESSION",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        cmd.env_remove(k);
    }
    let mut holder = cmd
        .arg("holder")
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
    // As the relay sends a view's `!` (`sdk::bash_command`).
    (&tap)
        .write_all(
            b"{\"type\":\"bash_command\",\"command\":\"echo zebra-42 && echo oops >&2\",\"uuid\":\"00000000-0000-4000-8000-0000000000b1\"}\n",
        )
        .unwrap();
    // Until claude has taken in the command and the holder's two lines: three lifecycles
    // completed. Not their echoes, which 2.1.311 does not always write.
    let mut r = BufReader::new(tap.try_clone().unwrap());
    let mut taken = 0;
    let mut line = String::new();
    while taken < 3 {
        line.clear();
        let got = r.read_line(&mut line);
        let log = || fs::read_to_string(dir.join("registry/claude-1.log")).unwrap_or_default();
        assert!(matches!(got, Ok(n) if n > 0), "{got:?}: {}", log());
        if line.contains("\"command_lifecycle\"") && line.contains("\"state\":\"completed\"") {
            taken += 1;
        }
    }
    assert!(api.requests().is_empty(), "a `!` command starts no turn");
    (&tap)
        .write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n")
        .unwrap();
    let request = api
        .wait_for(WAIT, |r| r.mentions("hi"))
        .expect("claude asked the model");
    let _ = holder.kill();
    let _ = holder.wait();
    let texts = request.texts();
    let count = |needle: &str| texts.iter().filter(|t| t.contains(needle)).count();
    assert_eq!(
        count("<bash-input>echo zebra-42 &amp;&amp; echo oops &gt;&amp;2</bash-input>"),
        1,
        "{texts:?}"
    );
    assert_eq!(
        count("<bash-stdout>zebra-42</bash-stdout><bash-stderr>oops</bash-stderr>"),
        1,
        "{texts:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}
