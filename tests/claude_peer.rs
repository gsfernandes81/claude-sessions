//! Agent messaging against the **real Claude Code**: every behaviour of Claude Code's own
//! cross-session messaging that the relay (`src/peer.rs`) relies on, each in a test named for
//! it, so a Claude Code bump either passes mechanically or names the behaviour that changed.
//!
//! **What is real and what stands in.** The `claude` binary is real, and so is this crate's
//! `agent`, which holds the stand-ins and delivers into claude's inbox exactly as in a
//! container. The test plays the hub, writing the agent's stdin and reading its stdout. The
//! model is `support/fake_api.rs`, a local stand-in for the Anthropic API that records what
//! claude sends and answers with scripted tool calls. So no request leaves the machine, no
//! account is used, and the shared `~/.claude` is never touched: each claude gets a scratch
//! `CLAUDE_CONFIG_DIR` and `XDG_RUNTIME_DIR`.
//!
//! **Which claude**: `CLAUDE_SESSIONS_TEST_CLAUDE`, else `claude` on `PATH`. Without one the
//! tests say so and pass, except under CI, where a missing binary is a failure, so a broken
//! install cannot pass for the wrong reason. CI installs the version infra pins.
//!
//! **Asserted by meaning, not by bytes.** A check reads a frame's fields and a wrapper's
//! attributes. It never compares Claude Code's output whole, so wording or attribute order
//! Claude Code is free to change does not fail a test, and a test that fails says which
//! promise broke. One claude runs at a time: each holds a couple of hundred megabytes.

#[allow(dead_code)]
#[path = "support/fake_api.rs"]
mod fake_api;
#[allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

use fake_api::{FakeApi, Reply, Request, Script};
use json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");
/// Claude Code starts in about two seconds here; a wait is generous so a slow CI runner is
/// not a failure.
const WAIT: Duration = Duration::from_secs(45);
/// How long "nothing happens" is watched for.
const QUIET: Duration = Duration::from_secs(4);
/// The other container's slot the agent holds a stand-in for.
const FAR: &str = "far/claude-3";
/// The local slot the real claude runs in.
const SLOT: &str = "claude-1";

/// One claude at a time.
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
            "claude_peer: no claude (CLAUDE_SESSIONS_TEST_CLAUDE, or claude on PATH); not run"
        );
    }
    found
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Field 22 of `/proc/<pid>/stat`.
fn start_time(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let tail = &stat[stat.rfind(')').unwrap() + 2..];
    tail.split_whitespace().nth(19).unwrap().to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut o = Value::obj();
    for (k, v) in pairs {
        o.set(k, v);
    }
    o
}

fn str_of<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str()
}

/// An attribute of the wrapper Claude Code puts a peer's message in.
fn attr(content: &str, name: &str) -> Option<String> {
    let open = content.split('>').next()?;
    let at = open.find(&format!(" {name}=\""))? + name.len() + 3;
    Some(open[at..].split('"').next()?.to_string())
}

/// The body of a peer message's wrapper.
fn body(content: &str) -> Option<&str> {
    let (_, rest) = content.split_once(">\n")?;
    rest.rsplit_once("\n</").map(|(b, _)| b)
}

/// A message as Claude Code 2.1.311 writes one, from `from_name` at `from_sock`, in
/// permission class `mode`.
fn peer_frame(from_sock: &str, from_name: &str, mode: &str, text: &str) -> Value {
    let from = format!("uds:{from_sock}");
    let content = format!(
        "<cross-session-message from=\"{from}\" from-name=\"{from_name}\" from-mode=\"{mode}\">\n{text}\n</cross-session-message>"
    );
    obj(vec![
        ("msgV", Value::num(1)),
        ("msg_id", Value::string(format!("m-{}", now_ms()))),
        ("type", Value::string("user")),
        (
            "message",
            obj(vec![
                ("role", Value::string("user")),
                ("content", Value::string(content)),
            ]),
        ),
        ("priority", Value::string("next")),
        ("from", Value::string(from)),
    ])
}

/// What the model does, by what claude sent: send `message` to `to` with `SendMessage`,
/// fetching the tool through `ToolSearch` first when it is deferred, then end the turn.
fn send_message(to: impl Fn(&Request) -> String + Send + Sync + 'static, message: &str) -> Script {
    let message = message.to_string();
    Arc::new(move |r: &Request| {
        let results = r.results();
        if results.iter().any(|(n, _)| n == "SendMessage") {
            return Reply::Text("done".into());
        }
        let loaded = results.iter().any(|(n, _)| n == "ToolSearch");
        if r.tools().iter().any(|t| t == "SendMessage") || loaded {
            let mut input = Value::obj();
            input.set("to", Value::string(to(r)));
            input.set("message", Value::string(&message));
            input.set("summary", Value::string("test"));
            return Reply::Tool {
                name: "SendMessage".into(),
                input,
            };
        }
        let mut input = Value::obj();
        input.set("query", Value::string("select:SendMessage"));
        input.set("max_results", Value::num(1));
        Reply::Tool {
            name: "ToolSearch".into(),
            input,
        }
    })
}

/// A real claude in a slot, the real agent beside it, and the fake model.
struct Rig {
    dir: PathBuf,
    api: FakeApi,
    claude: Child,
    claude_in: ChildStdin,
    claude_out: mpsc::Receiver<Value>,
    agent: Child,
    agent_in: ChildStdin,
    agent_out: mpsc::Receiver<Value>,
    /// The inbox claude's record names.
    inbox: String,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for c in [&mut self.claude, &mut self.agent] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn lines(out: impl std::io::Read + Send + 'static) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Ok(v) = json::parse(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

/// The first line from `rx` that `want` accepts within `wait`; what else came is dropped.
fn first(
    rx: &mpsc::Receiver<Value>,
    wait: Duration,
    want: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let until = Instant::now() + wait;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(v) if want(&v) => return Some(v),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

impl Rig {
    /// `settings`, when given, is passed to claude with `--settings`.
    fn start(tag: &str, script: Script, settings: Option<&str>) -> Option<Rig> {
        let claude = real_claude()?;
        let turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("cs-cc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["cc", "work", "registry", "zmx", "cgroup", "agent-work"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        // Claude Code wants its socket directory's parent private.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.join("run"))
            .unwrap();
        fs::write(dir.join("cgroup/memory.max"), "8589934592\n").unwrap();
        fs::write(dir.join("cgroup/memory.current"), "1073741824\n").unwrap();
        let api = FakeApi::start(script);

        let mut cmd = Command::new(&claude);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--allowedTools",
            "ListAgents,SendMessage,ToolSearch",
        ]);
        if let Some(s) = settings {
            fs::write(dir.join("settings.json"), s).unwrap();
            cmd.arg("--settings").arg(dir.join("settings.json"));
        }
        for k in [
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_SESSIONS_SLOT",
            "ZMX_SESSION",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            cmd.env_remove(k);
        }
        let mut c = cmd
            .current_dir(dir.join("work"))
            .env("CLAUDE_CONFIG_DIR", dir.join("cc"))
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("ANTHROPIC_BASE_URL", &api.url)
            .env("ANTHROPIC_API_KEY", "sk-ant-test-not-a-key")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(dir.join("claude.stderr")).unwrap())
            .spawn()
            .expect("claude runs");
        let pid = c.id();
        let claude_in = c.stdin.take().unwrap();
        let claude_out = lines(c.stdout.take().unwrap());

        // Its record, naming its inbox: what everything below stands on.
        let record = dir.join(format!("cc/sessions/{pid}.json"));
        let until = Instant::now() + WAIT;
        let inbox = loop {
            let v = fs::read_to_string(&record)
                .ok()
                .and_then(|t| json::parse(&t).ok());
            if let Some(sock) = v.as_ref().and_then(|v| str_of(v, &["messagingSocketPath"])) {
                break sock.to_string();
            }
            assert!(
                Instant::now() < until,
                "claude wrote no session record naming an inbox at {}; stderr: {}",
                record.display(),
                fs::read_to_string(dir.join("claude.stderr")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        };

        // The slot it runs in, as the registry would have it.
        fs::write(
            dir.join(format!("registry/{SLOT}.json")),
            format!(
                r#"{{"slot":"{SLOT}","state":"live","pid":{pid},"proc_start":{start},"title":"t",
                    "session_id":"s","cwd":"{work}","needs_you":false,"busy":false,
                    "registered":true,"last_activity_ms":{at}}}"#,
                start = start_time(pid),
                work = dir.join("work").display(),
                at = now_ms(),
            ),
        )
        .unwrap();
        fs::write(dir.join(format!("zmx/{SLOT}")), format!("{pid} 0\n")).unwrap();
        write_script(
            &dir.join("zmx.sh"),
            &format!(
                r#"case $1 in
  list)
    for f in "{z}"/*; do
      [ -f "$f" ] || continue
      read pid clients < "$f"
      printf 'name=%s\tpid=%s\tclients=%s\tcreated=1\n' "${{f##*/}}" "$pid" "$clients"
    done
    exit 0 ;;
esac
exit 2"#,
                z = dir.join("zmx").display()
            ),
        );

        // The agent, as the hub runs it, its stand-ins beside claude's inbox.
        let socks = Path::new(&inbox).parent().unwrap().to_path_buf();
        let mut a = Command::new(BIN)
            .arg("agent")
            .env("CLAUDE_SESSIONS_DIR", dir.join("registry"))
            .env("CLAUDE_SESSIONS_ZMX", dir.join("zmx.sh"))
            .env("ZMX_DIR", dir.join("zmx"))
            .env("CLAUDE_SESSIONS_WORKSPACE", dir.join("agent-work"))
            .env("CLAUDE_SESSIONS_CGROUP", dir.join("cgroup"))
            .env("CLAUDE_CONFIG_DIR", dir.join("cc"))
            .env("CLAUDE_SESSIONS_PEER_SOCKS", &socks)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let agent_in = a.stdin.take().unwrap();
        let agent_out = lines(a.stdout.take().unwrap());
        let mut rig = Rig {
            dir,
            api,
            claude: c,
            claude_in,
            claude_out,
            agent: a,
            agent_in,
            agent_out,
            inbox,
            _turn: turn,
        };
        // No title, so the stand-in is listed as FAR alone; the tests of titles give one.
        let mut far = Value::obj();
        far.set("repo", Value::string("far"));
        far.set("slot", Value::string("claude-3"));
        far.set("live", Value::Bool(true));
        rig.tell(obj(vec![("roster", Value::Arr(vec![far]))]));
        rig.stand_in();
        Some(rig)
    }

    /// A prompt, as [`Self::prompt`], and back once its turn has ended: a prompt typed during
    /// a turn is queued into it in another form.
    fn turn(&mut self, text: &str) {
        self.prompt(text);
        first(&self.claude_out, WAIT, |v| {
            str_of(v, &["type"]) == Some("result")
        })
        .unwrap_or_else(|| panic!("the turn for {text:?} never ended\n{}", self.story()));
    }

    /// The hub retitles [`FAR`]; back once its record is listed as `listed`.
    fn retitle(&mut self, title: &str, listed: &str) {
        let mut far = Value::obj();
        far.set("repo", Value::string("far"));
        far.set("slot", Value::string("claude-3"));
        far.set("title", Value::string(title));
        far.set("live", Value::Bool(true));
        self.tell(obj(vec![("roster", Value::Arr(vec![far]))]));
        let until = Instant::now() + WAIT;
        while self.record_name().as_deref() != Some(listed) {
            assert!(
                Instant::now() < until,
                "the stand-in was never listed as {listed:?}; it is {:?}",
                self.record_name()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The name [`FAR`]'s stand-in record is listed by.
    fn record_name(&self) -> Option<String> {
        fs::read_dir(self.dir.join("cc/sessions"))
            .ok()?
            .flatten()
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|t| json::parse(&t).ok())
            .filter_map(|v| str_of(&v, &["name"]).map(str::to_string))
            .find(|n| n == FAR || n.starts_with(&format!("{FAR} ")))
    }

    fn tell(&mut self, v: Value) {
        writeln!(self.agent_in, "{}", json::to_line(&v)).unwrap();
        self.agent_in.flush().unwrap();
    }

    /// A prompt typed to claude, starting a turn the fake model plays.
    fn prompt(&mut self, text: &str) {
        let v = obj(vec![
            ("type", Value::string("user")),
            (
                "message",
                obj(vec![
                    ("role", Value::string("user")),
                    ("content", Value::string(text)),
                ]),
            ),
        ]);
        writeln!(self.claude_in, "{}", json::to_line(&v)).unwrap();
        self.claude_in.flush().unwrap();
    }

    /// The stand-in the agent holds for [`FAR`]: its socket, once its record is written.
    fn stand_in(&self) -> String {
        let until = Instant::now() + WAIT;
        loop {
            for e in fs::read_dir(self.dir.join("cc/sessions"))
                .unwrap()
                .flatten()
            {
                let Some(v) = fs::read_to_string(e.path())
                    .ok()
                    .and_then(|t| json::parse(&t).ok())
                else {
                    continue;
                };
                if str_of(&v, &["name"])
                    .is_some_and(|n| n == FAR || n.starts_with(&format!("{FAR} ")))
                {
                    return str_of(&v, &["messagingSocketPath"]).unwrap().to_string();
                }
            }
            assert!(
                Instant::now() < until,
                "the agent never held a stand-in for {FAR}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A message from [`FAR`], delivered by the agent as the hub would have it delivered.
    fn deliver(&mut self, frame: Value) {
        self.tell(obj(vec![(
            "deliver",
            obj(vec![
                ("to", Value::string(SLOT)),
                ("from", Value::string(FAR)),
                ("frame", frame),
            ]),
        )]));
    }

    /// The next frame the agent hands the hub as sent to [`FAR`].
    fn sent(&self, want: impl Fn(&Value) -> bool) -> Option<Value> {
        first(&self.agent_out, WAIT, |v| {
            v.get("peer")
                .is_some_and(|p| str_of(p, &["to"]) == Some(FAR))
                && want(&v["peer"])
        })
        .map(|v| v.get("peer").cloned().unwrap())
    }
}

impl std::ops::Index<&str> for Value {
    type Output = Value;
    fn index(&self, k: &str) -> &Value {
        self.get(k).unwrap_or(&Value::Null)
    }
}

fn is_hold(v: &Value) -> bool {
    str_of(v, &["type"]) == Some("system") && str_of(v, &["subtype"]) == Some("peer_message_hold")
}

/// The request in which the model first sees a peer message carrying `text`.
fn seen_by_model(api: &FakeApi, text: &str, wait: Duration) -> Option<String> {
    api.wait_for(wait, |r| r.mentions(text))?
        .texts()
        .into_iter()
        .find(|t| t.contains(text))
}

impl Rig {
    /// What to read when a test fails: each request claude made of the model (the tools
    /// offered, the results it carried, the last text), and claude's own stderr.
    fn story(&self) -> String {
        let mut out = String::new();
        for (i, r) in self.api.requests().iter().enumerate() {
            let last = r.texts().last().cloned().unwrap_or_default();
            out.push_str(&format!(
                "request {i}: tools {:?}\n  results {:?}\n  last text {:?}\n",
                r.tools(),
                r.results(),
                last.chars().take(400).collect::<String>()
            ));
        }
        let stderr = fs::read_to_string(self.dir.join("claude.stderr")).unwrap_or_default();
        out.push_str(&format!("claude's stderr: {}\n", stderr.trim()));
        let agent = fs::read_to_string(self.dir.join("registry/agent.log")).unwrap_or_default();
        out.push_str(&format!("the agent's log: {}", agent.trim()));
        out
    }
}

// ── the tests ───────────────────────────────────────────────────────────────

#[test]
fn claude_publishes_an_inbox_speaking_peer_protocol_1() {
    let Some(rig) = Rig::start("record", Arc::new(|_| Reply::Text("ok".into())), None) else {
        return;
    };
    let record = rig
        .dir
        .join(format!("cc/sessions/{}.json", rig.claude.id()));
    let v = json::parse(&fs::read_to_string(record).unwrap()).unwrap();
    assert_eq!(
        v.get("peerProtocol").and_then(Value::as_u64),
        Some(1),
        "the relay speaks peer protocol 1, and switches itself off for any other"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(&rig.inbox).is_ok(),
        "the inbox the record names answers"
    );
    let socks = Path::new(&rig.inbox).parent().unwrap();
    assert_eq!(
        socks,
        rig.dir.join("run/cc-socks"),
        "inboxes are under $XDG_RUNTIME_DIR/cc-socks, where the agent puts its stand-ins"
    );
    // And the check infra runs before a pin says the same of this claude, leaving nothing in
    // the home or the temporary directory it ran with (infra runs it in live containers).
    let home = rig.dir.join("check-home");
    let tmp = rig.dir.join("check-tmp");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    let out = Command::new(BIN)
        .args(["peer-check", real_claude().unwrap().to_str().unwrap()])
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "peer-check: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let left = |d: &Path| -> Vec<PathBuf> {
        fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect()
    };
    assert_eq!(
        left(&home),
        Vec::<PathBuf>::new(),
        "peer-check left this in the home"
    );
    assert_eq!(
        left(&tmp),
        Vec::<PathBuf>::new(),
        "peer-check left this behind"
    );
}

#[test]
fn claude_lists_the_agents_stand_in_for_a_slot_in_another_container() {
    let script: Script = Arc::new(|r: &Request| {
        if r.results().iter().any(|(n, _)| n == "ListAgents") {
            Reply::Text("done".into())
        } else {
            Reply::Tool {
                name: "ListAgents".into(),
                input: Value::obj(),
            }
        }
    });
    let Some(mut rig) = Rig::start("list", script, None) else {
        return;
    };
    rig.prompt("who is there?");
    let listed = rig
        .api
        .wait_for(WAIT, |r| r.results().iter().any(|(n, _)| n == "ListAgents"))
        .unwrap_or_else(|| {
            panic!(
                "the model was never given ListAgents' result\n{}",
                rig.story()
            )
        });
    let (_, text) = listed
        .results()
        .into_iter()
        .find(|(n, _)| n == "ListAgents")
        .unwrap();
    let row = text.lines().find(|l| l.contains(FAR)).unwrap_or_else(|| {
        panic!("a record from a foreign pid domain whose socket answers is listed: {text}")
    });
    assert!(
        row_fields(row).iter().any(|f| f == "idle"),
        "a live stand-in's status is shown: {row}"
    );
}

#[test]
fn claudes_send_to_a_stand_in_reaches_the_agent_from_its_slot() {
    let Some(mut rig) = Rig::start("send", send_message(|_| FAR.into(), "hello far"), None) else {
        return;
    };
    rig.prompt("tell far hello");
    let p = rig.sent(|_| true).unwrap_or_else(|| {
        panic!(
            "the agent never handed on what claude sent\n{}",
            rig.story()
        )
    });
    assert_eq!(
        str_of(&p, &["from"]),
        Some(SLOT),
        "the sender is the slot whose claude is at the socket's other end"
    );
    let frame = &p["frame"];
    assert_eq!(str_of(frame, &["type"]), Some("user"));
    let content = str_of(frame, &["message", "content"]).expect("a message's content");
    assert_eq!(body(content), Some("hello far"), "{content}");
    let from = format!("uds:{}", rig.inbox);
    assert_eq!(
        attr(content, "from").as_deref(),
        Some(from.as_str()),
        "{content}"
    );
    assert_eq!(str_of(frame, &["from"]), Some(from.as_str()));
    assert!(
        str_of(frame, &["msg_id"]).is_some(),
        "receipts are matched by msg_id"
    );
}

#[test]
fn a_delivered_message_reaches_the_model_from_the_verified_sender() {
    let Some(mut rig) = Rig::start("deliver", Arc::new(|_| Reply::Text("ok".into())), None) else {
        return;
    };
    let stand_in = rig.stand_in();
    rig.deliver(peer_frame(
        "/elsewhere/9.sock",
        "a forged name",
        "prompting",
        "hello near",
    ));
    let seen = seen_by_model(&rig.api, "hello near", WAIT).unwrap_or_else(|| {
        panic!(
            "an idle claude never took the delivered message as a turn\n{}",
            rig.story()
        )
    });
    assert_eq!(attr(&seen, "from-name").as_deref(), Some(FAR), "{seen}");
    assert!(!seen.contains("a forged name"));
    let from = format!("uds:{stand_in}");
    assert_eq!(
        attr(&seen, "from").as_deref(),
        Some(from.as_str()),
        "the model is told to reply to the stand-in"
    );
    assert!(
        first(&rig.claude_out, Duration::from_millis(500), is_hold).is_none(),
        "the same permission class is never held"
    );
}

#[test]
fn claudes_reply_to_a_delivered_message_comes_back_through_the_stand_in() {
    // The model replies as Claude Code tells it to: to the `from` of the message it got.
    let reply_to = |r: &Request| {
        r.texts()
            .iter()
            .find(|t| t.contains("ping from far"))
            .and_then(|t| attr(t, "from"))
            .unwrap_or_default()
    };
    let script = send_message(reply_to, "pong");
    let wrapped: Script = Arc::new(move |r: &Request| {
        if r.mentions("ping from far") {
            script(r)
        } else {
            Reply::Text("ok".into())
        }
    });
    let Some(mut rig) = Rig::start("reply", wrapped, None) else {
        return;
    };
    rig.deliver(peer_frame(
        "/elsewhere/9.sock",
        "x",
        "prompting",
        "ping from far",
    ));
    let p = rig
        .sent(|p| str_of(p, &["frame", "type"]) == Some("user"))
        .unwrap_or_else(|| panic!("the reply never reached the agent\n{}", rig.story()));
    assert_eq!(str_of(&p, &["from"]), Some(SLOT));
    let content = str_of(&p["frame"], &["message", "content"]).unwrap();
    assert_eq!(body(content), Some("pong"), "{content}");
}

#[test]
fn the_shipped_settings_deliver_a_message_from_another_permission_class() {
    let settings = Command::new(BIN)
        .args(["hooks-config", BIN])
        .output()
        .unwrap();
    let settings = String::from_utf8(settings.stdout).unwrap();
    assert!(
        settings.contains("crossSessionInbound"),
        "calibration: {settings}"
    );
    let Some(mut rig) = Rig::start(
        "accept",
        Arc::new(|_| Reply::Text("ok".into())),
        Some(&settings),
    ) else {
        return;
    };
    rig.deliver(peer_frame(
        "/elsewhere/9.sock",
        "x",
        "bypass",
        "from a bypassing sender",
    ));
    assert!(
        seen_by_model(&rig.api, "from a bypassing sender", WAIT).is_some(),
        "crossSessionInbound: accept should let a message from another class through\n{}",
        rig.story()
    );
}

/// The calibration for the test above: without the setting, the same message is held, and
/// the sender is told so through the stand-in. Not run where a managed setting already
/// decides `crossSessionInbound`, since there it cannot be left unset.
#[test]
fn without_the_setting_a_message_from_another_permission_class_is_held_and_the_sender_told() {
    let managed = ["/etc/claude-code/managed-settings.json"]
        .into_iter()
        .map(PathBuf::from)
        .chain(
            fs::read_dir("/etc/claude-code/managed-settings.d")
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path()),
        )
        .any(|p| fs::read_to_string(p).is_ok_and(|t| t.contains("crossSessionInbound")));
    if managed {
        eprintln!("claude_peer: a managed setting decides crossSessionInbound here; not run");
        return;
    }
    let Some(mut rig) = Rig::start("hold", Arc::new(|_| Reply::Text("ok".into())), None) else {
        return;
    };
    rig.deliver(peer_frame("/elsewhere/9.sock", "x", "bypass", "held words"));
    assert!(
        first(&rig.claude_out, WAIT, is_hold).is_some(),
        "a sender in another class should be held while crossSessionInbound is unset\n{}",
        rig.story()
    );
    let receipt = rig
        .sent(|p| str_of(p, &["frame", "type"]) == Some("control"))
        .unwrap_or_else(|| panic!("no hold receipt reached the agent\n{}", rig.story()));
    assert_eq!(
        str_of(&receipt["frame"], &["action"]),
        Some("peer_message_status")
    );
    assert_eq!(str_of(&receipt["frame"], &["status"]), Some("held"));
    assert!(
        seen_by_model(&rig.api, "held words", QUIET).is_none(),
        "a held message does not reach the model"
    );
}

/// The calibration for the listing test: a stand-in whose agent has died (its record left
/// behind, its socket answering nobody) is not listed. So the listing above was the socket
/// answering, not merely a file being there.
#[test]
fn a_stand_in_whose_agent_has_died_is_not_listed() {
    let script: Script = Arc::new(|r: &Request| {
        if r.results().iter().any(|(n, _)| n == "ListAgents") {
            Reply::Text("done".into())
        } else {
            Reply::Tool {
                name: "ListAgents".into(),
                input: Value::obj(),
            }
        }
    });
    let Some(mut rig) = Rig::start("dead", script, None) else {
        return;
    };
    // SIGKILL: the agent cannot take its stand-ins down, so the record stays.
    rig.agent.kill().unwrap();
    rig.agent.wait().unwrap();
    rig.stand_in();
    rig.prompt("who is there?");
    let listed = rig
        .api
        .wait_for(WAIT, |r| r.results().iter().any(|(n, _)| n == "ListAgents"))
        .unwrap_or_else(|| {
            panic!(
                "the model was never given ListAgents' result\n{}",
                rig.story()
            )
        });
    let (_, text) = listed
        .results()
        .into_iter()
        .find(|(n, _)| n == "ListAgents")
        .unwrap();
    assert!(!text.contains(FAR), "a dead stand-in was listed: {text}");
}

/// The calibration for the send test: what a claude in no slot sends a stand-in is not carried,
/// since it has no address. So the sender named above was found from the socket, not assumed.
#[test]
fn a_send_from_a_claude_in_no_slot_is_not_carried() {
    let Some(mut rig) = Rig::start("noslot", send_message(|_| FAR.into(), "from nowhere"), None)
    else {
        return;
    };
    fs::remove_file(rig.dir.join(format!("registry/{SLOT}.json"))).unwrap();
    rig.prompt("tell far");
    rig.api
        .wait_for(WAIT, |r| {
            r.results().iter().any(|(n, _)| n == "SendMessage")
        })
        .unwrap_or_else(|| panic!("claude never sent\n{}", rig.story()));
    assert!(
        first(&rig.agent_out, QUIET, |v| v.get("peer").is_some()).is_none(),
        "a claude in no slot was given an address"
    );
}

/// An offloaded slot's stand-in carries no status (Claude Code shows only its own four), and
/// Claude Code still lists it, with none, rather than dropping the record or calling it idle.
#[test]
fn claude_lists_an_offloaded_stand_in_with_no_status() {
    let script: Script = Arc::new(|r: &Request| {
        if r.results().iter().any(|(n, _)| n == "ListAgents") {
            Reply::Text("done".into())
        } else {
            Reply::Tool {
                name: "ListAgents".into(),
                input: Value::obj(),
            }
        }
    });
    let Some(mut rig) = Rig::start("offloaded", script, None) else {
        return;
    };
    let mut far = Value::obj();
    far.set("repo", Value::string("far"));
    far.set("slot", Value::string("claude-3"));
    far.set("live", Value::Bool(false));
    rig.tell(obj(vec![("roster", Value::Arr(vec![far]))]));
    // The record is rewritten without a status.
    let until = Instant::now() + WAIT;
    loop {
        let has_status = fs::read_dir(rig.dir.join("cc/sessions"))
            .unwrap()
            .flatten()
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .find(|t| t.contains(&format!("\"{FAR}\"")))
            .is_some_and(|t| t.contains("\"status\""));
        if !has_status {
            break;
        }
        assert!(Instant::now() < until, "the stand-in kept its status");
        std::thread::sleep(Duration::from_millis(100));
    }
    rig.prompt("who is there?");
    let listed = rig
        .api
        .wait_for(WAIT, |r| r.results().iter().any(|(n, _)| n == "ListAgents"))
        .unwrap_or_else(|| {
            panic!(
                "the model was never given ListAgents' result\n{}",
                rig.story()
            )
        });
    let (_, text) = listed
        .results()
        .into_iter()
        .find(|(n, _)| n == "ListAgents")
        .unwrap();
    let row = text
        .lines()
        .find(|l| l.contains(FAR))
        .unwrap_or_else(|| panic!("an offloaded stand-in was not listed: {text}"));
    let fields = row_fields(row);
    assert!(
        fields.iter().any(|f| f == "interactive"),
        "calibration, the row reads as fields: {row}"
    );
    for status in ["busy", "idle", "shell", "waiting"] {
        assert!(!fields.iter().any(|f| f == status), "{row}");
    }
}

/// A ListAgents row's fields, as Claude Code joins them with `·`.
fn row_fields(row: &str) -> Vec<String> {
    row.split('\u{b7}').map(|f| f.trim().to_string()).collect()
}

/// The hub's notices come from `hub/router`, an address with no record behind it: Claude Code
/// takes a delivered message whatever records there are, so the notice reaches the model.
#[test]
fn a_notice_from_the_hub_reaches_the_model() {
    let Some(mut rig) = Rig::start("notice", Arc::new(|_| Reply::Text("ok".into())), None) else {
        return;
    };
    rig.tell(obj(vec![(
        "deliver",
        obj(vec![
            ("to", Value::string(SLOT)),
            ("from", Value::string("hub/router")),
            (
                "frame",
                peer_frame(
                    "/hub",
                    "hub/router",
                    "prompting",
                    "far/claude-3 is offloaded",
                ),
            ),
        ]),
    )]));
    let seen = seen_by_model(&rig.api, "far/claude-3 is offloaded", WAIT)
        .unwrap_or_else(|| panic!("the hub's notice never reached the model\n{}", rig.story()));
    assert_eq!(
        attr(&seen, "from-name").as_deref(),
        Some("hub/router"),
        "{seen}"
    );
    let names: Vec<String> = fs::read_dir(rig.dir.join("cc/sessions"))
        .unwrap()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter(|t| t.contains("hub/router"))
        .collect();
    assert!(
        names.is_empty(),
        "the hub's address is never listed: {names:?}"
    );
}

/// With the settings we ship, every session is told what an offloaded peer is and costs, by
/// `peer-context` on SessionStart: at its start, and again after a compaction, when Claude Code
/// fires SessionStart with source `compact`.
#[test]
fn the_shipped_settings_tell_a_session_about_offloaded_peers_at_start_and_after_compact() {
    const LINE: &str = "A peer listed with no status (neither busy nor idle) is offloaded.";
    let settings = Command::new(BIN)
        .args(["hooks-config", BIN])
        .output()
        .unwrap();
    let settings = String::from_utf8(settings.stdout).unwrap();
    assert!(settings.contains("peer-context"), "calibration: {settings}");
    let Some(mut rig) = Rig::start(
        "context",
        Arc::new(|_| Reply::Text("ok".into())),
        Some(&settings),
    ) else {
        return;
    };
    rig.prompt("the first prompt, before any compaction");
    rig.api
        .wait_for(WAIT, |r| r.mentions("the first prompt") && r.mentions(LINE))
        .unwrap_or_else(|| {
            panic!(
                "the lines were not in the session at its start\n{}",
                rig.story()
            )
        });
    rig.prompt("/compact");
    rig.prompt("the prompt after compaction");
    // After a compaction the first prompt is gone, so the lines there came back with it.
    rig.api
        .wait_for(WAIT, |r| {
            r.mentions("the prompt after compaction") && !r.mentions("the first prompt")
        })
        .filter(|r| r.mentions(LINE))
        .unwrap_or_else(|| {
            panic!(
                "the lines did not come back after a compaction\n{}",
                rig.story()
            )
        });
}

/// The model's part in the tests of titled names: on each prompt saying `send to <name>`, one
/// `SendMessage` to that name, then `ListAgents` on a prompt saying `list`, and nothing else.
fn on_each_prompt() -> Script {
    Arc::new(|r: &Request| {
        // An earlier prompt comes back with a line break after it.
        let texts: Vec<String> = r.texts().iter().map(|t| t.trim().to_string()).collect();
        let asks: Vec<&String> = texts
            .iter()
            .filter(|t| t.starts_with("send to ") || t.as_str() == "list")
            .collect();
        let answered = r
            .results()
            .iter()
            .filter(|(n, _)| n == "SendMessage" || n == "ListAgents")
            .count();
        let Some(ask) = asks.get(answered) else {
            return Reply::Text("done".into());
        };
        if ask.as_str() == "list" {
            return Reply::Tool {
                name: "ListAgents".into(),
                input: Value::obj(),
            };
        }
        let loaded = r.results().iter().any(|(n, _)| n == "ToolSearch");
        if !(r.tools().iter().any(|t| t == "SendMessage") || loaded) {
            let mut input = Value::obj();
            input.set("query", Value::string("select:SendMessage"));
            input.set("max_results", Value::num(1));
            return Reply::Tool {
                name: "ToolSearch".into(),
                input,
            };
        }
        let mut input = Value::obj();
        input.set("to", Value::string(ask.trim_start_matches("send to ")));
        input.set("message", Value::string(format!("msg {answered}")));
        input.set("summary", Value::string("test"));
        Reply::Tool {
            name: "SendMessage".into(),
            input,
        }
    })
}

/// The text of the `n`th result (from 0) of `tool` the model was given.
fn nth_result(rig: &Rig, tool: &str, n: usize) -> String {
    let req = rig
        .api
        .wait_for(WAIT, |r| {
            r.results().iter().filter(|(t, _)| t == tool).count() > n
        })
        .unwrap_or_else(|| {
            panic!(
                "the model was never given {tool} result {n}\n{}",
                rig.story()
            )
        });
    req.results()
        .into_iter()
        .filter(|(t, _)| t == tool)
        .nth(n)
        .unwrap()
        .1
}

#[test]
fn a_peer_is_listed_by_its_address_and_title_and_reached_by_that_name_in_any_case() {
    let Some(mut rig) = Rig::start("titled", on_each_prompt(), None) else {
        return;
    };
    rig.retitle("Tunnel cert renewal", "far/claude-3 Tunnel cert renewal");
    rig.turn("list");
    let text = nth_result(&rig, "ListAgents", 0);
    let row = text
        .lines()
        .find(|l| l.contains("far/claude-3 Tunnel cert renewal"))
        .unwrap_or_else(|| panic!("listed by its address and title: {text}"));
    assert!(row_fields(row).iter().any(|f| f == "idle"), "{row}");
    // Claude Code matches a name ignoring case, a run of spaces as one hyphen.
    rig.turn("send to FAR/claude-3-tunnel-CERT-renewal");
    let p = rig
        .sent(|p| str_of(p, &["frame", "type"]) == Some("user"))
        .unwrap_or_else(|| panic!("the send never reached the stand-in\n{}", rig.story()));
    assert_eq!(str_of(&p, &["from"]), Some(SLOT));
    let content = str_of(&p["frame"], &["message", "content"]).unwrap();
    assert_eq!(body(content), Some("msg 1"), "{content}");
}

#[test]
fn a_renamed_peer_is_not_reached_by_its_old_name_but_found_again_by_listing() {
    let Some(mut rig) = Rig::start("renamed", on_each_prompt(), None) else {
        return;
    };
    rig.retitle("first name", "far/claude-3 first name");
    rig.turn("send to far/claude-3 first name");
    rig.sent(|p| str_of(p, &["frame", "type"]) == Some("user"))
        .unwrap_or_else(|| panic!("calibration: the first send\n{}", rig.story()));
    rig.retitle("second name", "far/claude-3 second name");

    // Not even by the session that sent to it under that name: Claude Code holds no pin to it
    // (2.1.311). It is told so, and to list again, and nothing is carried.
    rig.turn("send to far/claude-3 first name");
    let refused = nth_result(&rig, "SendMessage", 1);
    assert!(
        refused.contains("No agent named") && refused.contains("ListAgents"),
        "{refused}"
    );
    assert!(
        first(&rig.agent_out, QUIET, |v| v.get("peer").is_some()).is_none(),
        "an old name reaches nobody"
    );

    // The listing shows the new name, and beside it the one it had.
    rig.turn("list");
    let text = nth_result(&rig, "ListAgents", 0);
    let row = text
        .lines()
        .find(|l| l.contains("far/claude-3 second name"))
        .unwrap_or_else(|| panic!("listed by its new name: {text}"));
    assert!(
        row.contains("says it was far/claude-3 first name"),
        "the name it had is shown beside it: {row}"
    );
    rig.turn("send to far/claude-3 second name");
    rig.sent(|p| {
        str_of(p, &["frame", "message", "content"]).is_some_and(|c| body(c) == Some("msg 3"))
    })
    .unwrap_or_else(|| panic!("the new name reaches it\n{}", rig.story()));
}
