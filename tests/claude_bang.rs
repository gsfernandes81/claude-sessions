//! `!` commands from the remote view, against the **real Claude Code**: run by the holder as
//! Claude Code 2.1.311 runs its own (`src/bang.rs`) — a `cd` carrying to the next — and added
//! to the conversation as Claude Code adds them: the command and its output, Claude answering
//! unless `respondToBashCommands` is false (`docs/remote-view.md`).
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
use json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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
            "claude_bang: no claude (CLAUDE_SESSIONS_TEST_CLAUDE, or claude on PATH); not run"
        );
    }
    found
}

/// A holder over the real claude in a scratch place, `settings` its user settings, and a tap.
struct Run {
    dir: PathBuf,
    api: FakeApi,
    holder: Child,
    tap: UnixStream,
    lines: BufReader<UnixStream>,
}

impl Run {
    fn start(tag: &str, claude: &Path, settings: &str) -> Run {
        let dir = std::env::temp_dir().join(format!("cs-bang-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["cc", "work/sub", "registry"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        fs::write(dir.join("cc/settings.json"), settings).unwrap();
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
            "BASH_DEFAULT_TIMEOUT_MS",
        ] {
            cmd.env_remove(k);
        }
        let holder = cmd
            .arg("holder")
            .current_dir(dir.join("work"))
            .env("CLAUDE_SESSIONS_SLOT", "claude-1")
            .env("CLAUDE_SESSIONS_CLAUDE", claude)
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
        let lines = BufReader::new(tap.try_clone().unwrap());
        Run {
            dir,
            api,
            holder,
            tap,
            lines,
        }
    }

    /// Send a `!` command as the relay does, and wait for the row of what it printed.
    fn bang(&mut self, command: &str) -> String {
        let mut v = Value::obj();
        v.set("type", Value::string("bash_command"));
        v.set("command", Value::string(command));
        v.set(
            "uuid",
            Value::string("00000000-0000-4000-8000-0000000000b1"),
        );
        (&self.tap)
            .write_all(format!("{}\n", json::to_line(&v)).as_bytes())
            .unwrap();
        self.until(|l| l.contains("\"by\":\"holder\"") && l.contains("<bash-stdout>"))
    }

    /// The next line from the holder that `want` takes.
    fn until(&mut self, want: impl Fn(&str) -> bool) -> String {
        let mut line = String::new();
        loop {
            line.clear();
            let got = self.lines.read_line(&mut line);
            let log =
                fs::read_to_string(self.dir.join("registry/claude-1.log")).unwrap_or_default();
            assert!(matches!(got, Ok(n) if n > 0), "{got:?}: {log}");
            if want(&line) {
                return line;
            }
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.holder.kill();
        let _ = self.holder.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn count(texts: &[String], needle: &str) -> usize {
    texts.iter().filter(|t| t.contains(needle)).count()
}

#[test]
fn a_cd_carries_to_the_next_command_and_claude_answers_each_once() {
    let Some(claude) = real_claude() else {
        return;
    };
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut r = Run::start("cd", &claude, "{}");
    let work = r.dir.join("work").canonicalize().unwrap();
    let sub = work.join("sub");
    let out = r.bang("cd sub");
    assert!(out.contains("<bash-exit-code>0</bash-exit-code>"), "{out}");
    // Claude answers it, as Claude Code's does by default.
    let first = r
        .api
        .wait_for(WAIT, |q| q.mentions("<bash-input>cd sub</bash-input>"))
        .expect("claude was asked to answer the command");
    let texts = first.texts();
    assert_eq!(
        count(&texts, "<bash-input>cd sub</bash-input>"),
        1,
        "{texts:?}"
    );
    assert_eq!(count(&texts, "<bash-stdout></bash-stdout>"), 1, "{texts:?}");
    assert_eq!(
        count(&texts, &format!("commands now run in {}.", sub.display())),
        1,
        "Claude is told its own Bash has not moved: {texts:?}"
    );
    r.until(|l| l.contains("\"type\":\"result\"") && l.contains("\"num_turns\":1"));
    // Where the last one ended.
    let out = r.bang("pwd && cd - >/dev/null && pwd");
    assert!(
        out.contains(&format!(
            "<bash-stdout>{}\\n{}</bash-stdout>",
            sub.display(),
            work.display()
        )),
        "{out}"
    );
}

#[test]
fn with_respond_to_bash_commands_off_it_waits_for_the_next_prompt_behind_the_caveat() {
    let Some(claude) = real_claude() else {
        return;
    };
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut r = Run::start("quiet", &claude, r#"{"respondToBashCommands":false}"#);
    r.bang("echo zebra-42 && echo oops >&2");
    // Until claude has taken in the caveat, the command and its output.
    let mut taken = 0;
    while taken < 3 {
        r.until(|l| l.contains("\"command_lifecycle\"") && l.contains("\"state\":\"completed\""));
        taken += 1;
    }
    assert!(r.api.requests().is_empty(), "no turn of its own");
    (&r.tap)
        .write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n")
        .unwrap();
    let q = r
        .api
        .wait_for(WAIT, |q| q.mentions("hi"))
        .expect("claude asked the model");
    let texts = q.texts();
    assert_eq!(
        count(
            &texts,
            "<local-command-caveat>The command below was run directly"
        ),
        1,
        "{texts:?}"
    );
    assert_eq!(
        count(
            &texts,
            "<bash-input>echo zebra-42 &amp;&amp; echo oops &gt;&amp;2</bash-input>"
        ),
        1,
        "{texts:?}"
    );
    assert_eq!(
        count(
            &texts,
            "<bash-stdout>zebra-42</bash-stdout><bash-stderr>oops</bash-stderr>"
        ),
        1,
        "{texts:?}"
    );
    assert_eq!(count(&texts, "commands now run in"), 0, "it did not move");
}
