//! Agent mail, end to end: the real binary as the hub's mail server, as each session's client
//! and as Claude Code runs its hook; three containers over a stand-in ssh, each with its own
//! registry and zmx, served by the real `agent`.
//!
//! The known-good case beside each known-bad one, because a check never seen to fail proves
//! nothing: a message delivered AND one refused for an offloaded session, one queued for a
//! container that is down and delivered when it comes up; a live slot accepted AND a forged
//! one refused; the hook announcing AND then silent, under the slot's own claude AND nothing
//! under a claude that is not; a `Stop` that blocks AND the one after it that does not.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");
const NUDGE: &str = "[claude-sessions mail] you have mail; the hook names it";
const PROMPTED: &str = r#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;

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

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

struct Root {
    dir: PathBuf,
    /// The stand-in claudes.
    children: Vec<Child>,
    server: Option<Child>,
    url: String,
    log: Option<mpsc::Receiver<String>>,
}

impl Drop for Root {
    fn drop(&mut self) {
        for c in self.children.iter_mut().chain(self.server.as_mut()) {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Root {
    fn new(tag: &str) -> Root {
        let dir = std::env::temp_dir().join(format!("cs-mail2-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        // ssh, as the hub runs it: a host with a `down` file is refused as ssh refuses; any
        // other runs the command in the host's environment.
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
        // A slot's own claude, as the hook sees its parents: claude under zmx. Each runs its
        // command as a child, so both stay in the line.
        fs::create_dir_all(dir.join("line")).unwrap();
        script(&dir.join("line/zmx"), r#""$@""#);
        script(&dir.join("line/claude"), r#""$@""#);
        Root {
            dir,
            children: Vec::new(),
            server: None,
            url: String::new(),
            log: None,
        }
    }

    /// A container: registry, zmx, Claude Code's store, workspace. Its zmx lists a file per
    /// session (`<pid> <clients>`) and writes what it is sent to `sent`.
    fn container(&self, host: &str) -> PathBuf {
        let d = self.dir.join(host);
        for sub in ["registry", "zmx", "cc", "work", "cgroup"] {
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
  send) printf '%s|%s\n' "$2" "$3" >> "{d}/sent"; exit 0 ;;
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
                 CLAUDE_SESSIONS_CGROUP='{d}/cgroup'\n",
                d = d.display()
            ),
        )
        .unwrap();
        d
    }

    /// A live slot in `host`: a running process the record names, its zmx session listed.
    fn live(&mut self, host: &str, slot: &str, attached: bool) {
        let d = self.dir.join(host);
        let child = Command::new("sleep").arg("600").spawn().unwrap();
        let pid = child.id();
        self.children.push(child);
        fs::write(
            d.join(format!("registry/{slot}.json")),
            format!(
                r#"{{"slot":"{slot}","state":"live","pid":{pid},"proc_start":{start},
                    "title":"{slot} at work","session_id":"s-{host}-{slot}","cwd":"{work}",
                    "needs_you":false,"busy":false,"last_activity_ms":{at}}}"#,
                start = start_time(pid),
                work = d.join("work").display(),
                at = now_ms(),
            ),
        )
        .unwrap();
        fs::write(
            d.join(format!("zmx/{slot}")),
            format!("{pid} {}\n", u8::from(attached)),
        )
        .unwrap();
    }

    fn offloaded(&self, host: &str, slot: &str) {
        let d = self.dir.join(host);
        let t = d.join(format!("{slot}.jsonl"));
        fs::write(&t, PROMPTED).unwrap();
        fs::write(
            d.join(format!("registry/{slot}.json")),
            format!(
                r#"{{"slot":"{slot}","state":"offloaded","title":"{slot} put away",
                    "session_id":"s-{host}-{slot}","cwd":"{work}","transcript_path":"{t}",
                    "needs_you":false,"last_activity_ms":{at}}}"#,
                work = d.join("work").display(),
                t = t.display(),
                at = now_ms() - 3_600_000,
            ),
        )
        .unwrap();
    }

    fn down(&self, host: &str, is: bool) {
        let flag = self.dir.join(host).join("down");
        if is {
            fs::write(flag, "").unwrap();
        } else {
            let _ = fs::remove_file(flag);
        }
    }

    /// The hub's mail server, on a port of its own; what it logs arrives on `self.log`.
    fn serve(&mut self, hosts: &str) {
        if let Some(mut old) = self.server.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        let mut child = Command::new(BIN)
            .arg("mail-server")
            .env("CLAUDE_SESSIONS_HOSTS", hosts)
            .env("CLAUDE_SESSIONS_SSH", self.dir.join("bin/ssh"))
            .env("CLAUDE_SESSIONS_REMOTE", BIN)
            .env("CLAUDE_SESSIONS_RETRY_MS", "1000")
            .env("CLAUDE_SESSIONS_DIR", self.dir.join("hub-registry-unused"))
            .env("HUB_MAIL_LISTEN", "127.0.0.1:0")
            .env("HUB_MAIL", self.dir.join("store"))
            .env("HUB_MAIL_TOKENS", self.dir.join("tokens"))
            .env("HUB_MAIL_MAX_BYTES", "256")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        self.server = Some(child);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let line = rx
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .expect("the server said where it listens");
            if let Some(at) = line.split(" on 127.0.0.1:").nth(1) {
                let port = at.split(';').next().unwrap();
                self.url = format!("http://127.0.0.1:{port}");
                break;
            }
        }
        self.log = Some(rx);
    }

    /// Everything the server has logged since last asked.
    fn logged(&self) -> String {
        let rx = self.log.as_ref().unwrap();
        let mut s = String::new();
        while let Ok(l) = rx.try_recv() {
            s.push_str(&l);
            s.push('\n');
        }
        s
    }

    fn token(&self, host: &str) -> PathBuf {
        self.dir.join("tokens").join(host).join("mail-token")
    }

    fn client(&self, host: &str, slot: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("mail")
            .args(args)
            .env("CLAUDE_SESSIONS_MAIL_URL", &self.url)
            .env("CLAUDE_SESSIONS_MAIL_TOKEN", self.token(host))
            .env("CLAUDE_SESSIONS_DIR", self.dir.join(host).join("registry"))
            .env_remove("ZMX_SESSION");
        if slot.is_empty() {
            cmd.env_remove("CLAUDE_SESSIONS_SLOT");
        } else {
            cmd.env("CLAUDE_SESSIONS_SLOT", slot);
        }
        cmd
    }

    /// `claude-sessions mail <args>` as `host/slot`, `body` on stdin.
    fn mail(&self, host: &str, slot: &str, args: &[&str], body: &str) -> Output {
        let mut child = self
            .client(host, slot, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, host: &str, slot: &str, args: &[&str], body: &str) -> String {
        let out = self.mail(host, slot, args, body);
        assert!(
            out.status.success(),
            "mail {args:?} as {host}/{slot} failed: {}",
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    /// The mail hook as Claude Code runs it for `host/slot`'s own claude — under `claude`
    /// under `zmx` — or, `own` false, under a claude with no zmx above it.
    fn hook(&self, host: &str, slot: &str, own: bool, payload: &str) -> (String, Duration) {
        let line = self.dir.join("line");
        let mut cmd = if own {
            let mut c = Command::new(line.join("zmx"));
            c.arg(line.join("claude"));
            c
        } else {
            Command::new(line.join("claude"))
        };
        cmd.args([BIN, "mail-hook"])
            .env("CLAUDE_SESSIONS_MAIL_URL", &self.url)
            .env("CLAUDE_SESSIONS_MAIL_TOKEN", self.token(host))
            .env("CLAUDE_SESSIONS_SLOT", slot)
            .env("CLAUDE_SESSIONS_DIR", self.dir.join(host).join("registry"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let start = Instant::now();
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let took = start.elapsed();
        assert!(out.status.success(), "the hook exits 0");
        assert!(out.stderr.is_empty(), "the hook says nothing on stderr");
        (text(&out.stdout), took)
    }

    /// Until `who`, as `host/slot` sees it, contains each of `want`.
    fn until_who(&self, host: &str, slot: &str, want: &[&str]) -> String {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.mail(host, slot, &["who"], "");
            let said = text(&out.stdout);
            if want.iter().all(|w| said.contains(w)) {
                return said;
            }
            assert!(
                Instant::now() < until,
                "who never showed {want:?}: {said}{}",
                text(&out.stderr)
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn prompt(p: &str) -> String {
    format!(
        r#"{{"hook_event_name":"UserPromptSubmit","prompt":{}}}"#,
        json_str(p)
    )
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

const START: &str = r#"{"hook_event_name":"SessionStart","source":"startup"}"#;
const STOP: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false}"#;
const STOP_AGAIN: &str = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;

/// The id `mail send` printed.
fn id_of(sent: &str) -> String {
    sent.lines()
        .next()
        .and_then(|l| l.strip_prefix("sent "))
        .unwrap_or_else(|| panic!("not a send: {sent}"))
        .to_string()
}

#[test]
fn mail_moves_between_sessions_and_says_what_became_of_each() {
    let mut root = Root::new("moves");
    root.container("infra");
    root.live("infra", "claude-1", true);
    root.offloaded("infra", "claude-2");
    root.container("or3");
    root.live("or3", "claude-1", false);
    root.container("dossier");
    root.down("dossier", true);
    root.serve("infra or3 dossier");
    let who = root.until_who(
        "infra",
        "claude-1",
        &[
            "infra/claude-1  live",
            "or3/claude-1  live",
            "infra/claude-2  offloaded",
        ],
    );
    assert!(who.contains("you are infra/claude-1"), "{who}");
    assert!(who.contains("dossier  not answering"), "{who}");

    // One send, every outcome.
    let sent = root.ok(
        "infra",
        "claude-1",
        &[
            "send",
            "--to",
            "or3/claude-1,infra/claude-2",
            "--cc",
            "dossier",
            "--cc",
            "nope/claude-1",
            "--subject",
            "the base image moved",
        ],
        "From infra's agent.\n\nThe base moved to 2026.10.29.\n",
    );
    let id = id_of(&sent);
    for want in [
        "delivered or3/claude-1",
        "refused infra/claude-2 (offloaded)",
        "queued dossier (its container is not answering)",
        "refused nope/claude-1 (no such repo)",
    ] {
        assert!(sent.contains(want), "{want:?} not in {sent}");
    }
    let log = root.logged();
    assert!(
        log.contains(&format!("{id}: from infra/claude-1, ")),
        "{log}"
    );
    assert!(
        !log.contains("The base moved"),
        "never a body in the log: {log}"
    );
    assert!(
        log.contains("nudged or3/claude-1\n"),
        "idle and detached: {log}"
    );

    // or3/claude-1 is idle and nobody is on it: nudged, the fixed line then Enter.
    let sent_file = root.dir.join("or3/sent");
    let until = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&sent_file).is_ok_and(|s| s.contains('\r')) {
        assert!(Instant::now() < until, "or3/claude-1 was never nudged");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        fs::read_to_string(&sent_file).unwrap(),
        format!("claude-1|{NUDGE}\nclaude-1|\r\n")
    );

    // A claude under the slot that is not its own hears nothing; its own does, once.
    let (said, _) = root.hook("or3", "claude-1", false, &prompt(NUDGE));
    assert_eq!(said, "", "a nested claude takes nothing");
    let (said, _) = root.hook("or3", "claude-1", true, &prompt(NUDGE));
    assert!(
        said.contains("typed by the hub"),
        "the nudge is said for what it is: {said}"
    );
    assert!(
        said.contains(&format!(
            "new message from infra/claude-1: the base image moved (claude-sessions mail read {id})"
        )),
        "{said}"
    );
    let (said, _) = root.hook("or3", "claude-1", true, START);
    assert_eq!(said, "", "announced once");
    let (said, _) = root.hook("or3", "claude-1", true, STOP);
    assert_eq!(said, "", "and not again at the turn's end");

    // Read: the hub's headers, then the body; and its sender sees it read.
    let listed = root.ok("or3", "claude-1", &["list", "--unread"], "");
    assert!(
        listed.contains(&id) && listed.contains("announced"),
        "{listed}"
    );
    let read = root.ok("or3", "claude-1", &["read", &id], "");
    assert!(
        read.starts_with(
            "From: infra/claude-1\nTo: or3/claude-1, infra/claude-2\nCc: dossier/*, nope/claude-1\n\
             Subject: the base image moved\n"
        ),
        "{read}"
    );
    assert!(read.contains("Reply-Wanted: no\n"), "{read}");
    assert!(read.ends_with("\n\nFrom infra's agent.\n\nThe base moved to 2026.10.29.\n"));
    assert_eq!(
        root.ok("or3", "claude-1", &["list", "--unread"], ""),
        "no mail\n"
    );
    let status = root.ok("infra", "claude-1", &["status", &id], "");
    assert!(status.contains("or3/claude-1  read"), "{status}");
    assert!(status.contains("dossier  queued"), "{status}");
    let theirs = root.mail("or3", "claude-1", &["status", &id], "");
    assert!(!theirs.status.success(), "status is the sender's");

    // A reply, and the sender waiting for it — one already there counts.
    let reply = root.ok("or3", "claude-1", &["reply", &id], "Ack.\n");
    let reply_id = id_of(&reply);
    assert!(reply.contains("delivered infra/claude-1"), "{reply}");
    let got = root.ok(
        "infra",
        "claude-1",
        &["wait", "--for", &id, "--timeout", "20s"],
        "",
    );
    assert!(got.contains("Subject: Re: the base image moved\n"), "{got}");
    assert!(got.contains(&format!("In-Reply-To: <{id}@hub>\n")), "{got}");
    assert!(got.contains("Reply-Wanted: yes\n"), "to one session: {got}");
    // infra/claude-1 is attached: not nudged. The server says so before it answers a send.
    let log = root.logged();
    assert!(
        !log.contains("nudged infra/claude-1"),
        "an attached session is never typed into: {log}"
    );
    let status = root.ok("or3", "claude-1", &["status", &reply_id], "");
    assert!(
        status.contains("infra/claude-1  read"),
        "wait read it: {status}"
    );
    // Nothing more to wait for: it says so at its timeout.
    let start = Instant::now();
    let timed_out = root.mail("infra", "claude-1", &["wait", "--timeout", "2s"], "");
    assert!(!timed_out.status.success());
    assert!(
        text(&timed_out.stderr).contains("no mail came"),
        "{}",
        text(&timed_out.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(10));

    // A Stop with mail unannounced blocks once; the next does not.
    let next = id_of(&root.ok(
        "or3",
        "claude-1",
        &["send", "--to", "infra/claude-1", "--subject", "one more"],
        "x\n",
    ));
    let (said, _) = root.hook("infra", "claude-1", true, STOP);
    assert!(
        said.starts_with(
            r#"{"decision":"block","reason":"new message from or3/claude-1: one more"#
        ),
        "{said}"
    );
    assert!(said.contains(&next));
    let (said, _) = root.hook("infra", "claude-1", true, STOP);
    assert_eq!(said, "", "blocked once");
    // A turn the block kept going is never blocked again, whatever has come since.
    root.ok(
        "or3",
        "claude-1",
        &["send", "--to", "infra/claude-1", "--subject", "and another"],
        "y\n",
    );
    let (said, _) = root.hook("infra", "claude-1", true, STOP_AGAIN);
    assert_eq!(said, "", "no chain of blocks");
    let (said, _) = root.hook("infra", "claude-1", true, START);
    assert!(
        said.contains("and another"),
        "it waits for the next look: {said}"
    );

    // The container that was down comes up with a session: the queued message is its.
    root.live("dossier", "claude-3", true);
    root.down("dossier", false);
    root.until_who("dossier", "claude-3", &["dossier/claude-3  live"]);
    let until = Instant::now() + Duration::from_secs(10);
    let listed = loop {
        let l = root.ok("dossier", "claude-3", &["list"], "");
        if l.contains(&id) {
            break l;
        }
        assert!(Instant::now() < until, "the queued message never came: {l}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(listed.contains("infra/claude-1"), "{listed}");
    let read = root.ok("dossier", "claude-3", &["read", &id], "");
    assert!(read.contains("\nQueued-Since: "), "{read}");
    let log = root.logged();
    assert!(log.contains("dossier is answering"), "{log}");
    assert!(
        log.contains(&format!("{id}: delivered dossier/claude-3 (was queued)")),
        "{log}"
    );

    // Seconds and several sends later, still nothing typed into the attached sessions.
    assert!(
        !root.dir.join("infra/sent").exists(),
        "an attached session is never typed into"
    );
    assert!(!root.dir.join("dossier/sent").exists());

    // A store read back after a restart serves the same mail.
    root.serve("infra or3 dossier");
    root.until_who("or3", "claude-1", &["or3/claude-1  live"]);
    let again = root.ok("or3", "claude-1", &["read", &id], "");
    assert!(again.contains("Subject: the base image moved"), "{again}");
}

#[test]
fn who_is_calling_is_the_hubs_to_say() {
    let mut root = Root::new("auth");
    root.container("infra");
    root.live("infra", "claude-1", true);
    root.container("or3");
    root.live("or3", "claude-1", true);
    root.serve("infra or3");
    root.until_who("infra", "claude-1", &["or3/claude-1  live"]);
    let send = |host: &str, slot: &str| {
        root.mail(
            host,
            slot,
            &["send", "--to", "or3/claude-1", "--subject", "hi"],
            "x",
        )
    };
    // Calibration: the real session sends.
    assert!(send("infra", "claude-1").status.success());
    // A slot that is not live in the token's own container is refused, even one live in
    // another container: one repo's token names only its own sessions.
    let forged = send("infra", "claude-9");
    assert!(!forged.status.success());
    assert!(
        text(&forged.stderr).contains("infra/claude-9 is not a live session"),
        "{}",
        text(&forged.stderr)
    );
    // No slot at all: no address.
    let none = send("infra", "");
    assert!(
        text(&none.stderr).contains("mail is for a session"),
        "{}",
        text(&none.stderr)
    );
    // A token that is not one.
    fs::write(root.dir.join("bad-token"), "0".repeat(64)).unwrap();
    let out = root
        .client("infra", "claude-1", &["who"])
        .env("CLAUDE_SESSIONS_MAIL_TOKEN", root.dir.join("bad-token"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no valid mail token"),
        "{}",
        text(&out.stderr)
    );
    // From is the hub's: a From in the body is body.
    let id = id_of(&root.ok(
        "infra",
        "claude-1",
        &["send", "--to", "or3/claude-1", "--subject", "s"],
        "From: owner\n",
    ));
    let read = root.ok("or3", "claude-1", &["read", &id], "");
    assert!(read.starts_with("From: infra/claude-1\n"), "{read}");
    // Too big is refused, and nothing is sent.
    let big = root.mail(
        "infra",
        "claude-1",
        &["send", "--to", "or3/claude-1", "--subject", "s"],
        &"x".repeat(300),
    );
    assert!(!big.status.success());
    assert!(
        text(&big.stderr).contains("at most 256 bytes"),
        "{}",
        text(&big.stderr)
    );
    // A request the server cannot read is answered as such, raw.
    let mut s = std::net::TcpStream::connect(root.url.trim_start_matches("http://")).unwrap();
    s.write_all(b"NOT HTTP\r\n\r\n").unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
}

#[test]
fn the_hook_gives_up_within_its_second_and_says_nothing() {
    let mut root = Root::new("slow");
    root.container("infra");
    // A server that takes the connection and never answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    root.url = format!("http://{}", silent.local_addr().unwrap());
    fs::create_dir_all(root.dir.join("tokens/infra")).unwrap();
    fs::write(root.token("infra"), "t".repeat(64)).unwrap();
    let (said, took) = root.hook("infra", "claude-1", true, &prompt(NUDGE));
    assert!(
        said.contains("typed by the hub") && !said.contains("new message"),
        "the nudge is said even when the hub is not there: {said}"
    );
    assert!(took < Duration::from_millis(2500), "took {took:?}");
    assert!(
        took >= Duration::from_millis(900),
        "calibration: it did wait: {took:?}"
    );
    let log = fs::read_to_string(root.dir.join("infra/registry/hook.log")).unwrap_or_default();
    assert!(log.contains("did not answer"), "logged, not said: {log}");
    // No token at all: nothing, at once.
    fs::remove_file(root.token("infra")).unwrap();
    let (said, took) = root.hook("infra", "claude-1", true, STOP);
    assert_eq!(said, "");
    assert!(took < Duration::from_millis(900), "took {took:?}");
    drop(silent);
}
