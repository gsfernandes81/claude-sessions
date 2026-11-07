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
  attach)
    # A start that takes a while and fails: a wake with room stays in flight this long.
    sleep 1
    exit 2 ;;
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

    /// The host's available memory as the router reads it, in MB.
    fn host_free(&self, mb: u64) {
        fs::write(
            self.dir.join("meminfo"),
            format!("MemTotal: 8388608 kB\nMemAvailable: {} kB\n", mb * 1024),
        )
        .unwrap();
    }

    fn route(&mut self, hosts: &str) {
        if !self.dir.join("meminfo").exists() {
            self.host_free(8192);
        }
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
            .env("CLAUDE_SESSIONS_MEMINFO", self.dir.join("meminfo"))
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

    /// What to read when a test fails: everything the router said, and each container's
    /// agent log, which says what its relay took, bounced or dropped, and why.
    fn story(&mut self) -> String {
        if let Some(rx) = self.log.as_ref() {
            while let Ok(l) = rx.try_recv() {
                self.seen.push_str(&l);
                self.seen.push('\n');
            }
        }
        let mut out = format!("the router said:\n{}", self.seen);
        for host in ["a", "b"] {
            let log =
                fs::read_to_string(self.d(host).join("registry/agent.log")).unwrap_or_default();
            out.push_str(&format!("{host}'s agent log:\n{}\n", log.trim()));
        }
        out
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
fn delivered(inbox: &UnixListener, root: &mut Root) -> String {
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
                if Instant::now() >= until {
                    panic!("nothing was delivered\n{}", root.story());
                }
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
    let got = delivered(&b_in, &mut root);
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
    // The router's heartbeat, beside its queue, for infra's health check.
    let beat = fs::read_to_string(root.dir.join("queue/.beat")).expect("the router beats");
    assert!(beat.contains(" answering, "), "{beat:?}");

    // b's claude replies to that address, and it reaches a, from b/claude-1.
    let b_sock = root.d("b").join("in.sock");
    send(&back, &frame(&b_sock, "whoever", "pong"));
    let got = delivered(&a_in, &mut root);
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
    // The router asks b to wake it; the stand-in zmx here cannot, and the slot is put back as
    // it was. Waited for, so that putting it back cannot land after the slot is made live below.
    root.logs("did not wake b/claude-2");
    root.route("a b");
    root.logs("1 waiting");
    root.live("b", "claude-2");
    let got = delivered(&b_in, &mut root);
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
    // Its record goes first, then its socket.
    while !root.stand_ins("b").is_empty() || to_a1.exists() {
        assert!(Instant::now() < until, "b's stand-ins stayed");
        std::thread::sleep(Duration::from_millis(100));
    }
    // A message for b is bounced by its agent, and kept rather than lost.
    let a_sock = root.d("a").join("in.sock");
    send(
        &root.stand_in("a", "b/claude-1"),
        &frame(&a_sock, "x", "while off"),
    );
    root.logs("a/claude-1 to b/claude-1: message kept: bounced");
    assert!(quiet(&b_in));
}

#[test]
fn a_message_to_an_offloaded_slot_asks_to_wake_it_and_tells_the_sender_why_not() {
    let mut root = Root::new("wake");
    root.container("a");
    root.container("b");
    root.live("a", "claude-1");
    root.live("b", "claude-1");
    root.offloaded("b", "claude-2");
    // b has 100 MB free: too little for another session, which costs about 250 MB.
    fs::write(root.d("b").join("cgroup/memory.max"), "4294967296\n").unwrap();
    fs::write(
        root.d("b").join("cgroup/memory.current"),
        format!("{}\n", 4294967296u64 - 100 * 1024 * 1024),
    )
    .unwrap();
    let a_in = root.inbox("a", 1);
    let _b_in = root.inbox("b", 1);
    root.route("a b");
    // Offloaded, the stand-in claims no status, so nothing calls it idle; a live one does.
    root.stand_in("a", "b/claude-2");
    root.stand_in("a", "b/claude-1");
    let record_of = |name: &str| -> String {
        fs::read_dir(root.d("a").join("cc/sessions"))
            .unwrap()
            .flatten()
            .map(|e| fs::read_to_string(e.path()).unwrap_or_default())
            .find(|t| field(t, "name").as_deref() == Some(name))
            .unwrap()
    };
    assert!(
        record_of("b/claude-1").contains("\"status\":\"idle\""),
        "calibration"
    );
    assert!(!record_of("b/claude-2").contains("\"status\""));

    let a_sock = root.d("a").join("in.sock");
    send(
        &root.stand_in("a", "b/claude-2"),
        &frame(&a_sock, "x", "wake up"),
    );
    root.logs("a/claude-1 to b/claude-2: message kept");
    root.logs("asked b to wake b/claude-2");
    root.logs("did not wake b/claude-2: no-room");
    let got = delivered(&a_in, &mut root);
    let content = field(&got, "content").expect("a notice");
    assert!(content.contains("from-name=\"hub/router\""), "{content}");
    assert!(content.contains("has no room to wake it"), "{content}");
    assert!(content.contains("100M free of 4.0G"), "{content}");
    // Still kept: it reaches the slot once it runs.
    root.live("b", "claude-2");
    root.logs("a/claude-1 to b/claude-2: message handed on (was waiting)");
}

#[test]
fn a_second_wake_is_asked_for_only_once_the_first_has_answered() {
    let mut root = Root::new("one");
    root.container("a");
    root.container("b");
    root.live("a", "claude-1");
    root.live("b", "claude-1");
    root.offloaded("b", "claude-2");
    root.offloaded("b", "claude-3");
    let _a_in = root.inbox("a", 1);
    let _b_in = root.inbox("b", 1);
    root.route("a b");
    let to2 = root.stand_in("a", "b/claude-2");
    let to3 = root.stand_in("a", "b/claude-3");
    // b has room, so the first wake starts its slot, which takes a second: the second
    // message arrives while that wake is in flight, and its wake waits for the answer.
    let a_sock = root.d("a").join("in.sock");
    send(&to2, &frame(&a_sock, "x", "wake up"));
    root.logs("asked b to wake b/claude-2");
    send(&to3, &frame(&a_sock, "x", "you too"));
    root.logs("a/claude-1 to b/claude-3: message kept");
    root.logs("did not wake b/claude-2");
    root.logs("asked b to wake b/claude-3");
    let at = |text: &str| root.seen.find(text).unwrap();
    assert!(
        at("did not wake b/claude-2") < at("asked b to wake b/claude-3"),
        "one at a time:\n{}",
        root.seen
    );
}

#[test]
fn a_wake_is_not_asked_for_when_the_host_has_no_room_whatever_the_container_says() {
    let mut root = Root::new("host");
    root.container("a");
    root.container("b");
    root.live("a", "claude-1");
    root.live("b", "claude-1");
    root.offloaded("b", "claude-2");
    // b's own cgroup has room to spare (8G ceiling, 1G used), but the host has 300 MB.
    root.host_free(300);
    let a_in = root.inbox("a", 1);
    let _b_in = root.inbox("b", 1);
    root.route("a b");
    let a_sock = root.d("a").join("in.sock");
    send(
        &root.stand_in("a", "b/claude-2"),
        &frame(&a_sock, "x", "wake up"),
    );
    root.logs("a/claude-1 to b/claude-2: message kept");
    root.logs("did not ask to wake b/claude-2: the host has 300M available");
    assert!(
        !root.seen.contains("asked b to wake"),
        "no container was asked:\n{}",
        root.seen
    );
    let got = delivered(&a_in, &mut root);
    let content = field(&got, "content").expect("a notice");
    assert!(content.contains("from-name=\"hub/router\""), "{content}");
    assert!(
        content.contains("it was not woken: the host has 300M available"),
        "{content}"
    );
}

#[test]
fn a_message_bounced_for_a_moment_is_tried_again_without_waiting_for_a_snapshot() {
    let mut root = Root::new("retry");
    root.container("a");
    root.container("b");
    root.live("a", "claude-1");
    root.live("b", "claude-1");
    let _a_in = root.inbox("a", 1);
    root.route("a b");
    let to_b1 = root.stand_in("a", "b/claude-1");
    root.stand_in("b", "a/claude-1");
    // b's claude has no inbox yet: the message is bounced, and kept.
    let a_sock = root.d("a").join("in.sock");
    send(&to_b1, &frame(&a_sock, "x", "early"));
    root.logs("a/claude-1 to b/claude-1: message kept: bounced");
    // Its inbox comes; nothing in b's rows changes, so no snapshot would hand it on.
    let b_in = root.inbox("b", 1);
    let got = delivered(&b_in, &mut root);
    let content = field(&got, "content").unwrap();
    assert!(
        content.ends_with("\nearly\n</cross-session-message>"),
        "{content}"
    );
}
