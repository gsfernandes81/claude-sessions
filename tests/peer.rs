//! Agent messaging, end to end: the real binary as the hub's router and as each container's
//! agent, two containers over a stand-in ssh, each with its own registry, zmx and Claude Code
//! store. This test process plays each slot's claude: its pid is the slot's registered
//! claude, and it holds the inbox socket that claude's record names, so what it writes to a
//! stand-in is attributed to that slot and what is delivered to the slot reaches it.
//!
//! The known-good case beside each known-bad one: stand-ins for the other container's slots
//! AND none for a container's own; a message delivered with the sender's verified address AND
//! the sender's own claim of a name discarded; a reply coming back the same way; a message to
//! an offloaded slot kept across a router restart AND delivered once the slot is live; a
//! relay that switches itself off for a claude speaking another protocol.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");
const WAIT: Duration = Duration::from_secs(15);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Field 22 of `/proc/<pid>/stat`.
fn start_time(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let tail = &stat[stat.rfind(')').unwrap() + 2..];
    tail.split_whitespace().nth(19).unwrap().to_string()
}

/// A JSON string's contents, for building frames by hand.
fn quote(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// A message as Claude Code 2.1.311 writes it, from the inbox at `from_sock`.
fn frame(from_sock: &Path, name: &str, body: &str) -> String {
    let from = format!("uds:{}", from_sock.display());
    let content = format!(
        "<cross-session-message from=\"{from}\" from-name=\"{name}\" from-mode=\"prompting\">\n{body}\n</cross-session-message>"
    );
    format!(
        r#"{{"msgV":1,"msg_id":"m-{n}","type":"user","message":{{"role":"user","content":"{c}"}},"priority":"next","from":"{from}"}}"#,
        n = now_ms(),
        c = quote(&content),
    )
}

/// A field of a JSON line, read crudely: `"key":"value"`, the value unescaped of `\"`.
fn field(line: &str, key: &str) -> Option<String> {
    let at = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let mut out = String::new();
    let mut esc = false;
    for c in line[at..].chars() {
        match (esc, c) {
            (true, c) => {
                out.push(match c {
                    'n' => '\n',
                    c => c,
                });
                esc = false;
            }
            (false, '\\') => esc = true,
            (false, '"') => return Some(out),
            (false, c) => out.push(c),
        }
    }
    None
}

struct Root {
    dir: PathBuf,
    router: Option<Child>,
    log: Option<mpsc::Receiver<String>>,
    seen: String,
}

impl Drop for Root {
    fn drop(&mut self) {
        if let Some(mut r) = self.router.take() {
            let _ = r.kill();
            let _ = r.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Root {
    fn new(tag: &str) -> Root {
        let dir = std::env::temp_dir().join(format!("cs-peer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        script(
            &dir.join("bin/ssh"),
            &format!(
                r#"while [ $# -gt 0 ]; do
  case $1 in -t|-T) shift ;; -o) shift 2 ;; --) shift; break ;; *) break ;; esac
done
host=$1; shift
d="{root}/$host"
if [ -f "$d/down" ] || [ ! -d "$d" ]; then
  echo "ssh: connect to host $host port 2222: Connection refused" >&2
  exit 255
fi
. "$d/env"
exec "$@""#,
                root = dir.display()
            ),
        );
        Root {
            dir,
            router: None,
            log: None,
            seen: String::new(),
        }
    }

    fn d(&self, host: &str) -> PathBuf {
        self.dir.join(host)
    }

    /// A container: registry, zmx, Claude Code's store, workspace, stand-in sockets.
    fn container(&self, host: &str) {
        let d = self.d(host);
        for sub in ["registry", "zmx", "cc/sessions", "work", "cgroup", "s"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        fs::write(d.join("cgroup/memory.max"), "8589934592\n").unwrap();
        fs::write(d.join("cgroup/memory.current"), "1073741824\n").unwrap();
        script(
            &d.join("zmx.sh"),
            &format!(
                r#"dir="{d}/zmx"
case $1 in
  list)
    for f in "$dir"/*; do
      [ -f "$f" ] || continue
      read pid clients < "$f"
      printf 'name=%s\tpid=%s\tclients=%s\tcreated=1\n' "${{f##*/}}" "$pid" "$clients"
    done
    exit 0 ;;
esac
exit 2"#,
                d = d.display()
            ),
        );
        fs::write(
            d.join("env"),
            format!(
                "export CLAUDE_SESSIONS_DIR='{d}/registry' CLAUDE_SESSIONS_ZMX='{d}/zmx.sh' \
                 ZMX_DIR='{d}/zmx' CLAUDE_SESSIONS_WORKSPACE='{d}/work' CLAUDE_CONFIG_DIR='{d}/cc' \
                 CLAUDE_SESSIONS_CGROUP='{d}/cgroup' CLAUDE_SESSIONS_PEER_SOCKS='{d}/s'\n",
                d = d.display()
            ),
        )
        .unwrap();
    }

    /// A live slot in `host` whose claude is this test process, with an inbox the test holds.
    fn live(&self, host: &str, slot: &str) {
        let d = self.d(host);
        let pid = std::process::id();
        fs::write(
            d.join(format!("registry/{slot}.json")),
            format!(
                r#"{{"slot":"{slot}","state":"live","pid":{pid},"proc_start":{start},
                    "title":"{slot} at work","session_id":"s-{host}-{slot}","cwd":"{work}",
                    "needs_you":false,"busy":false,"registered":true,"last_activity_ms":{at}}}"#,
                start = start_time(pid),
                work = d.join("work").display(),
                at = now_ms(),
            ),
        )
        .unwrap();
        fs::write(d.join(format!("zmx/{slot}")), format!("{pid} 0\n")).unwrap();
    }

    fn offloaded(&self, host: &str, slot: &str) {
        let d = self.d(host);
        let t = d.join(format!("{slot}.jsonl"));
        fs::write(
            &t,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        )
        .unwrap();
        fs::write(
            d.join(format!("registry/{slot}.json")),
            format!(
                r#"{{"slot":"{slot}","state":"offloaded","title":"{slot} put away",
                    "session_id":"s-{host}-{slot}","cwd":"{work}","transcript_path":"{t}",
                    "needs_you":false,"registered":true,"last_activity_ms":{at}}}"#,
                work = d.join("work").display(),
                t = t.display(),
                at = now_ms() - 3_600_000,
            ),
        )
        .unwrap();
    }

    /// The inbox of this process's claude in `host`, named by its Claude Code record.
    fn inbox(&self, host: &str, protocol: u32) -> UnixListener {
        let d = self.d(host);
        let sock = d.join("in.sock");
        let _ = fs::remove_file(&sock);
        let l = UnixListener::bind(&sock).unwrap();
        self.claim_protocol(host, protocol);
        l
    }

    fn claim_protocol(&self, host: &str, protocol: u32) {
        let d = self.d(host);
        fs::write(
            d.join(format!("cc/sessions/{}.json", std::process::id())),
            format!(
                r#"{{"pid":{},"messagingSocketPath":"{}","peerProtocol":{protocol},"version":"2.1.311"}}"#,
                std::process::id(),
                d.join("in.sock").display()
            ),
        )
        .unwrap();
    }

    fn route(&mut self, hosts: &str) {
        if let Some(mut old) = self.router.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        let mut child = Command::new(BIN)
            .arg("router")
            .env("CLAUDE_SESSIONS_HOSTS", hosts)
            .env("CLAUDE_SESSIONS_SSH", self.dir.join("bin/ssh"))
            .env("CLAUDE_SESSIONS_REMOTE", BIN)
            .env("CLAUDE_SESSIONS_RETRY_MS", "1000")
            .env("CLAUDE_SESSIONS_DIR", self.dir.join("hub-registry-unused"))
            .env("HUB_QUEUE", self.dir.join("queue"))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        self.router = Some(child);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        self.log = Some(rx);
    }

    /// Wait for the router to log a line containing `text`; everything it logged meanwhile
    /// is kept for the failure message.
    fn logs(&mut self, text: &str) {
        let until = Instant::now() + WAIT;
        while !self.seen.contains(text) {
            let left = until.saturating_duration_since(Instant::now());
            match self.log.as_ref().unwrap().recv_timeout(left) {
                Ok(l) => {
                    self.seen.push_str(&l);
                    self.seen.push('\n');
                }
                Err(_) => panic!("the router never said {text:?}; it said:\n{}", self.seen),
            }
        }
    }

    /// The stand-in socket `host`'s agent holds for `addr`, once its record is there.
    fn stand_in(&self, host: &str, addr: &str) -> PathBuf {
        let until = Instant::now() + WAIT;
        loop {
            if let Some(p) = self.stand_ins(host).into_iter().find(|(a, _)| a == addr) {
                return p.1;
            }
            assert!(
                Instant::now() < until,
                "{host} never held a stand-in for {addr}; it holds {:?}",
                self.stand_ins(host)
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every stand-in record in `host`'s Claude Code store: its name and socket.
    fn stand_ins(&self, host: &str) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.d(host).join("cc/sessions"))
            .unwrap()
            .flatten()
        {
            let text = fs::read_to_string(e.path()).unwrap_or_default();
            if !text.contains("claude-sessions:") {
                continue;
            }
            if let (Some(name), Some(sock)) =
                (field(&text, "name"), field(&text, "messagingSocketPath"))
            {
                out.push((name, PathBuf::from(sock)));
            }
        }
        out.sort();
        out
    }
}

/// Write `line` to `sock`, as a claude's `SendMessage` does.
fn send(sock: &Path, line: &str) {
    let mut s = UnixStream::connect(sock).unwrap();
    s.write_all(format!("{line}\n").as_bytes()).unwrap();
}

/// The next line delivered to `inbox`.
fn delivered(inbox: &UnixListener) -> String {
    inbox.set_nonblocking(true).unwrap();
    let until = Instant::now() + WAIT;
    loop {
        match inbox.accept() {
            Ok((mut c, _)) => {
                c.set_nonblocking(false).unwrap();
                c.set_read_timeout(Some(WAIT)).unwrap();
                let mut s = String::new();
                c.read_to_string(&mut s).unwrap();
                return s.trim_end().to_string();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "nothing was delivered");
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// Whether anything reaches `inbox` within a short while.
fn quiet(inbox: &UnixListener) -> bool {
    inbox.set_nonblocking(true).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    inbox.accept().is_err()
}

#[test]
fn a_message_crosses_containers_with_its_verified_sender_and_the_reply_comes_back() {
    let mut root = Root::new("cross");
    root.container("a");
    root.container("b");
    root.live("a", "claude-1");
    root.live("b", "claude-1");
    root.offloaded("b", "claude-2");
    let a_in = root.inbox("a", 1);
    let b_in = root.inbox("b", 1);
    root.route("a b");

    // Stand-ins for the other container's slots, an offloaded one included; none for its own.
    let to_b1 = root.stand_in("a", "b/claude-1");
    root.stand_in("a", "b/claude-2");
    root.stand_in("b", "a/claude-1");
    let names = |h: &str| -> Vec<String> { root.stand_ins(h).into_iter().map(|p| p.0).collect() };
    assert_eq!(names("a"), ["b/claude-1", "b/claude-2"]);
    assert_eq!(names("b"), ["a/claude-1"]);

    // a/claude-1 writes to its stand-in for b/claude-1, claiming a name of its own.
    let a_sock = root.d("a").join("in.sock");
    send(&to_b1, &frame(&a_sock, "the owner", "hello from a"));
    let got = delivered(&b_in);
    let content = field(&got, "content").expect("a message");
    assert!(
        content.contains("from-name=\"a/claude-1\""),
        "the verified address, never the claim: {content}"
    );
    assert!(!content.contains("the owner"));
    assert!(content.ends_with("\nhello from a\n</cross-session-message>"));
    // `from` names a's stand-in in b, which answers, so b's claude can reply to it.
    let back = root.stand_in("b", "a/claude-1");
    let from = format!("uds:{}", back.display());
    assert_eq!(field(&got, "from").as_deref(), Some(from.as_str()));
    assert!(content.starts_with(&format!("<cross-session-message from=\"{from}\"")));
    root.logs("a/claude-1 to b/claude-1: message handed on");

    // b's claude replies to that address, and it reaches a, from b/claude-1.
    let b_sock = root.d("b").join("in.sock");
    send(&back, &frame(&b_sock, "whoever", "pong"));
    let got = delivered(&a_in);
    let content = field(&got, "content").unwrap();
    assert!(content.contains("from-name=\"b/claude-1\""), "{content}");
    assert!(content.ends_with("\npong\n</cross-session-message>"));
    assert_eq!(
        field(&got, "from").unwrap(),
        format!("uds:{}", to_b1.display())
    );

    // Something that is not a message in Claude Code's form is dropped by the sending agent:
    // never delivered, never kept.
    send(
        &to_b1,
        r#"{"type":"user","message":{"role":"user","content":"raw words"}}"#,
    );
    assert!(quiet(&b_in), "a malformed message reached a claude");

    // To the offloaded slot: kept, through a router restart, and delivered once it is live.
    send(
        &root.stand_in("a", "b/claude-2"),
        &frame(&a_sock, "x", "for later"),
    );
    root.logs("a/claude-1 to b/claude-2: message kept");
    root.route("a b");
    root.logs("1 waiting");
    root.live("b", "claude-2");
    let got = delivered(&b_in);
    let content = field(&got, "content").unwrap();
    assert!(
        content.ends_with("\nfor later\n</cross-session-message>"),
        "{content}"
    );
    assert!(content.contains("from-name=\"a/claude-1\""));
    root.logs("a/claude-1 to b/claude-2: message handed on (was waiting)");
}

#[test]
fn a_relay_switches_itself_off_for_a_claude_that_speaks_another_protocol() {
    let mut root = Root::new("off");
    root.container("a");
    root.container("b");
    root.live("a", "claude-1");
    root.live("b", "claude-1");
    let _a_in = root.inbox("a", 1);
    let b_in = root.inbox("b", 1);
    root.route("a b");
    root.stand_in("a", "b/claude-1");
    let to_a1 = root.stand_in("b", "a/claude-1");

    // b's claude now speaks protocol 2: b's stand-ins go and the router is told.
    root.claim_protocol("b", 2);
    root.logs("b's relay is off: claude-1's claude speaks peer protocol 2, not 1");
    let until = Instant::now() + WAIT;
    while !root.stand_ins("b").is_empty() {
        assert!(Instant::now() < until, "b's stand-ins stayed");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!to_a1.exists());
    // A message for b is bounced by its agent, and kept rather than lost.
    let a_sock = root.d("a").join("in.sock");
    send(
        &root.stand_in("a", "b/claude-1"),
        &frame(&a_sock, "x", "while off"),
    );
    root.logs("a/claude-1 to b/claude-1: message kept: bounced");
    assert!(quiet(&b_in));
}
