//! The relay, run as the hub runs it for a phone, over a stand-in ssh.
//!
//! Its core is tested as a state machine in `src/relay.rs`; this is the threads and pipes
//! around it: the client's lines in and out, the conversation's command started, written to
//! and ended, a side question run beside it, a home action handed to a container's agent, and
//! a container that does not answer asked again only when its retry is due. Both containers'
//! agents fail as ssh does, exit 255 and a line on stderr; the conversation's command is a
//! script that says its slot, a header and one line, then keeps what it is given and ignores
//! its input closing, so only the relay ending it ends it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

/// Only for a failure: nothing here waits on a clock to pass.
const DEADLINE: Duration = Duration::from_secs(30);

fn stand_in_ssh(root: &Path) -> PathBuf {
    let p = root.join("ssh");
    let body = format!(
        r#"#!/bin/sh
d='{d}'
while [ "$1" != "--" ]; do shift; done
shift
host=$1
shift 2
echo "$host $*" >> "$d/calls"
case "$1" in
  agent) echo "ssh: connect to host $host port 22: Connection refused" >&2; exit 255;;
esac
case "$2" in
  open)
    echo "$$" > "$d/pid-$3"
    printf '{{"slot":"%s"}}\n' "$3"
    echo '{{"run":"r1","next":2}}'
    printf '1\t%s\n' '{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"text","text":"hello there"}}]}}}}'
    while IFS= read -r l; do printf '%s\n' "$l" >> "$d/in-$3"; done
    exec sleep 1000;;
  btw)
    q=$(cat)
    printf '{{"answer":"you asked %s"}}\n' "$q";;
esac
"#,
        d = root.display()
    );
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Client {
    to: Option<ChildStdin>,
    from: Receiver<String>,
}

/// The value of `"key":"…"` or `"key":…` in a line the relay sent, as written: this test
/// reads only a few fields, none of them nested or escaped.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &line[at..];
    if let Some(s) = rest.strip_prefix('"') {
        return s.find('"').map(|e| &s[..e]);
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(&rest[..end])
}

impl Client {
    fn send(&mut self, line: &str) {
        let to = self.to.as_mut().expect("still connected");
        writeln!(to, "{line}").unwrap();
        to.flush().unwrap();
    }

    /// The first line the relay sends from now on that `want` accepts.
    fn until(&self, what: &str, want: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + DEADLINE;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.from.recv_timeout(left) {
                Ok(l) if want(&l) => return l,
                Ok(_) => {}
                Err(_) => panic!("the relay never sent {what}"),
            }
        }
    }

    fn until_t(&self, t: &str) -> String {
        self.until(t, |l| field(l, "t") == Some(t))
    }
}

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let end = Instant::now() + DEADLINE;
    while !ok() {
        assert!(Instant::now() < end, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pid(root: &Path, slot: &str) -> String {
    let f = root.join(format!("pid-{slot}"));
    wait_for(&format!("{slot}'s command never started"), || f.exists());
    std::fs::read_to_string(f).unwrap().trim().to_string()
}

fn gone(pid: &str) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn the_relay_carries_a_conversation_a_side_question_and_a_home_action_and_ends_what_it_started() {
    let root = std::env::temp_dir().join(format!("cs-relay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("registry")).unwrap();
    let ssh = stand_in_ssh(&root);
    let mut relay = Command::new(BIN)
        .arg("relay")
        .env("CLAUDE_SESSIONS_HOSTS", "c1 c2")
        .env("CLAUDE_SESSIONS_SSH", &ssh)
        .env("CLAUDE_SESSIONS_DIR", root.join("registry"))
        // Neither comes due while the test runs: a second try at either would be early.
        .env("CLAUDE_SESSIONS_RETRY_MS", "3600000")
        .env("CLAUDE_SESSIONS_LOAD_MS", "3600000")
        .env("HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let out = relay.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                return;
            }
        }
    });
    let mut c = Client {
        to: relay.stdin.take(),
        from: rx,
    };

    // The hello, then the home: the hub's own header, read when the relay started.
    c.send(r#"{"t":"hi","v":1}"#);
    c.until_t("hi");
    c.until_t("hub");

    // A home action goes to the container's agent, which is not answering.
    c.send(r#"{"t":"offload","c":"c1","k":{"slot":"claude-1"}}"#);
    let did = c.until_t("did");
    assert_eq!(field(&did, "ok"), Some("false"), "{did}");

    // A conversation: its command's lines reach the client, and the client's reach it.
    c.send(r#"{"t":"open","c":"c1","k":{"slot":"claude-3"}}"#);
    c.until_t("open");
    c.until("its line", |l| l.contains("hello there"));
    let first = pid(&root, "claude-3");
    c.send(r#"{"t":"say","x":"hello relay"}"#);
    let input = root.join("in-claude-3");
    wait_for("the prompt never reached the command", || {
        std::fs::read_to_string(&input).is_ok_and(|t| t.contains("hello relay"))
    });

    // A side question runs beside it, and its answer comes back.
    c.send(r#"{"t":"btw","x":"why"}"#);
    let btw = c.until_t("btw");
    assert_eq!(field(&btw, "x"), Some("you asked why"), "{btw}");

    // Closed: its command is ended, though it ignores its input closing.
    c.send(r#"{"t":"close"}"#);
    wait_for("the closed conversation's command lives on", || {
        gone(&first)
    });

    // One left open when the client goes is ended with the relay.
    c.send(r#"{"t":"open","c":"c1","k":{"slot":"claude-4"}}"#);
    c.until_t("open");
    let second = pid(&root, "claude-4");
    drop(c.to.take());
    let end = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(s) = relay.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < end, "the relay outlived its client");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "{status}");
    assert!(
        gone(&second),
        "the open conversation's command outlived the relay"
    );

    // Each container's agent was asked once: neither retry came due.
    let calls = std::fs::read_to_string(root.join("calls")).unwrap();
    for host in ["c1", "c2"] {
        let asked = calls
            .lines()
            .filter(|l| *l == format!("{host} agent"))
            .count();
        assert_eq!(asked, 1, "{calls}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
