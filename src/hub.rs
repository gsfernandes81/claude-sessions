//! The hub: one menu over every dev container, each reached over ssh.
//!
//! **What the hub knows is only what each container's agent tells it** (`agent.rs`): one
//! `ssh -T <container> claude-sessions agent` per container, held open while the menu is, its
//! snapshots arriving on their own and every action sent down it. The hub reads no registry,
//! no `/proc` and no zmx of its own; the containers' are theirs to read, under their own locks.
//!
//! **Which containers.** `CLAUDE_SESSIONS_HOSTS` — names separated by spaces or commas — else
//! the one name `local`. One source: infra's hub sets it, derived from its own repo map. A name is an ssh alias: what reaching it takes — user, port, key,
//! multiplexing — is the hub's `~/.ssh/config`, which infra writes. `local` is this machine,
//! run as a child with no ssh at all: a single container's own menu, and the tests.
//!
//! **What runs there.** `CLAUDE_SESSIONS_REMOTE` names the program in the containers (default
//! `claude-sessions`, found on their `PATH`); `CLAUDE_SESSIONS_SSH` names the ssh to run
//! (default `ssh`), which the tests point at a stand-in.

use crate::json::{self, Value};
use crate::launch::SCROLLBACK;
use crate::menu::{Place, Reach};
use crate::proto::{self, Child, Snapshot};
use crate::ui::{Outcome, Row, Terminal};
use crate::work;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The name that means this machine, with no ssh.
pub const LOCAL: &str = "local";

/// The containers the hub reaches, in the order it shows them.
pub fn hosts() -> Vec<String> {
    let split = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    };
    if let Ok(v) = std::env::var("CLAUDE_SESSIONS_HOSTS") {
        let names = split(&v);
        if !names.is_empty() {
            return dedup(names);
        }
    }
    vec![LOCAL.to_string()]
}

fn dedup(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// `claude-sessions <args…>` in `host`: through ssh — `-t` when it is to have the owner's
/// terminal, `-T` when it is a channel for messages — or, for `local`, this binary as a
/// child. `BatchMode` because nothing may ever ask a question on either: a password prompt on
/// the message channel would sit there for ever, and one at the terminal is not ours.
pub fn command(host: &str, args: &[&str], tty: bool) -> Command {
    if host == LOCAL {
        let me = std::env::current_exe().unwrap_or_else(|_| "claude-sessions".into());
        let mut cmd = Command::new(me);
        cmd.args(args);
        return cmd;
    }
    let mut cmd = Command::new(env_or("CLAUDE_SESSIONS_SSH", "ssh"));
    cmd.arg(if tty { "-t" } else { "-T" })
        .args(["-o", "BatchMode=yes", "--", host])
        .arg(env_or("CLAUDE_SESSIONS_REMOTE", "claude-sessions"))
        .args(args);
    cmd
}

/// A login shell in `host`, for a container whose agent is not answering: its door, which
/// ssh with no command reaches.
pub fn shell_command(host: &str) -> Command {
    if host == LOCAL {
        let mut cmd = Command::new(env_or("SHELL", "/bin/sh"));
        cmd.arg("-l");
        return cmd;
    }
    let mut cmd = Command::new(env_or("CLAUDE_SESSIONS_SSH", "ssh"));
    cmd.args(["-t", "-o", "BatchMode=yes", "--", host]);
    cmd
}

/// What a connection tells the menu, on the channel `connect` was given.
#[derive(Debug)]
pub enum Event {
    Snapshot(String, Snapshot),
    /// The container stopped answering, or never did, and what was said about it.
    Down(String, String),
}

/// What a connection tells an action waiting on it.
enum Reply {
    Run(Value),
    Done(Outcome),
    Gone(String),
}

/// One container's agent, over ssh.
pub struct Conn {
    pub name: String,
    stdin: Mutex<Option<ChildStdin>>,
    waiting: Mutex<HashMap<u64, Sender<Reply>>>,
    next: AtomicU64,
}

impl Conn {
    /// A connection to `name` not yet made, which [`connect`] then makes: until it is, a job
    /// sent to it is refused at once, as one sent to a container whose agent has gone.
    pub fn detached(name: &str) -> Arc<Conn> {
        Arc::new(Conn {
            name: name.to_string(),
            stdin: Mutex::new(None),
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    fn send(&self, v: &Value) -> io::Result<()> {
        let mut stdin = self.stdin.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pipe) = stdin.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "not connected"));
        };
        writeln!(pipe, "{}", json::to_line(v))?;
        pipe.flush()
    }

    fn fail_all(&self, why: &str) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|p| p.into_inner());
        for (_, tx) in waiting.drain() {
            let _ = tx.send(Reply::Gone(why.to_string()));
        }
        *self.stdin.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Start `host`'s agent. Its snapshots, and its going away, arrive on `events`, each followed
/// by a call to `wake` so a menu blocked on the terminal looks at them.
pub fn connect(host: &str, events: Sender<Event>, wake: fn()) -> Arc<Conn> {
    let conn = Conn::detached(host);
    let mut cmd = command(host, &["agent"], false);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let program = cmd.get_program().to_string_lossy().into_owned();
            let _ = events.send(Event::Down(
                host.to_string(),
                format!("could not run {program}: {e}"),
            ));
            wake();
            return conn;
        }
    };
    *conn.stdin.lock().unwrap_or_else(|p| p.into_inner()) = child.stdin.take();
    // The last thing it said on stderr, which is ssh's reason when it is ssh that failed.
    let said = Arc::new(Mutex::new(String::new()));
    let stderr = child.stderr.take();
    let stderr_reader = {
        let said = said.clone();
        std::thread::spawn(move || {
            let Some(err) = stderr else { return };
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    *said.lock().unwrap_or_else(|p| p.into_inner()) = line;
                }
            }
        })
    };
    let stdout = child.stdout.take();
    let reader = conn.clone();
    std::thread::spawn(move || {
        let name = reader.name.clone();
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(msg) = json::parse(&line) else {
                    continue;
                };
                if let Some(snap) = proto::snapshot_from(&msg, &name) {
                    let _ = events.send(Event::Snapshot(name.clone(), snap));
                    wake();
                    continue;
                }
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let reply = if let Some(run) = msg.get("run") {
                    Reply::Run(run.clone())
                } else if let Some(done) = msg.get("done") {
                    Reply::Done(proto::outcome_from(done).unwrap_or_else(|| {
                        Outcome::Refused("the container's answer could not be read".into())
                    }))
                } else {
                    continue;
                };
                let waiting = reader.waiting.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(tx) = waiting.get(&id) {
                    let _ = tx.send(reply);
                }
            }
        }
        let status = child.wait().ok();
        let _ = stderr_reader.join();
        let said = said.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let why = if !said.is_empty() {
            said
        } else {
            match status.and_then(|s| s.code()) {
                Some(code) => format!("its agent exited {code}"),
                None => "its agent was stopped".to_string(),
            }
        };
        reader.fail_all(&why);
        let _ = events.send(Event::Down(name, why));
        wake();
    });
    conn
}

/// The scrollback variables the owner's login set, for the container's launcher to leave as
/// they are (`launch::SCROLLBACK`, `agent::preset`).
fn login_env() -> Value {
    let mut env = Value::obj();
    for key in SCROLLBACK {
        if let Some(v) = std::env::var_os(key) {
            env.set(key, Value::string(v.to_string_lossy()));
        }
    }
    env
}

/// An action, done in `conn`'s container: sent there, its children run at this terminal when
/// it asks, and its outcome brought back. Runs on the menu's worker thread (`work.rs`), so the
/// menu turns its spinner meanwhile and lends the terminal through `term`.
pub fn job(conn: Arc<Conn>, mut op: Value) -> work::Job {
    Box::new(move |term: &mut dyn Terminal| {
        let name = conn.name.clone();
        let gone = |why: &str| Outcome::Refused(format!("{name} stopped answering: {why}"));
        let id = conn.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        conn.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        op.set("id", Value::num(id as f64));
        op.set("env", login_env());
        if let Err(e) = conn.send(&op) {
            conn.waiting
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return gone(&e.to_string());
        }
        let outcome = loop {
            match rx.recv() {
                Ok(Reply::Run(child)) => {
                    let ran = run_there(&name, &child, term);
                    let mut msg = Value::obj();
                    msg.set("id", Value::num(id as f64));
                    msg.set("ran", ran);
                    if let Err(e) = conn.send(&msg) {
                        break gone(&e.to_string());
                    }
                }
                Ok(Reply::Done(outcome)) => break outcome,
                Ok(Reply::Gone(why)) => break gone(&why),
                Err(_) => break gone("the connection closed"),
            }
        };
        conn.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        outcome
    })
}

/// The status ssh exits with when it is ssh that failed, not what it ran.
const SSH_FAILED: i32 = 255;

/// Run the child a container's action asked for, at this terminal, back in that container.
/// Answers `{"code": N}`, or `{"error": …}` when it could not be run — which the container's
/// launcher reports as a refusal rather than as a session that ended.
fn run_there(host: &str, child: &Value, term: &mut dyn Terminal) -> Value {
    let mut ran = Value::obj();
    let Some(child) = Child::from_value(child) else {
        ran.set(
            "error",
            Value::string("the container asked for nothing to run"),
        );
        return ran;
    };
    let hex = child.to_hex();
    let mut cmd = command(host, &["agent", "exec", &hex], true);
    match term.run(&mut cmd) {
        Ok(status) => match status.code() {
            Some(SSH_FAILED) if host != LOCAL => {
                ran.set("error", Value::string(format!("ssh to {host} failed")));
            }
            Some(code) => ran.set("code", Value::num(code)),
            None => ran.set("code", Value::num(128 + 9)),
        },
        Err(e) => ran.set("error", Value::string(e.to_string())),
    }
    ran
}

/// A shell in a container whose agent is not answering: ssh's own login, at this terminal.
pub fn shell_job(host: &str) -> work::Job {
    let host = host.to_string();
    Box::new(move |term: &mut dyn Terminal| {
        let mut cmd = shell_command(&host);
        match term.run(&mut cmd) {
            Ok(status) if status.code() == Some(SSH_FAILED) && host != LOCAL => {
                Outcome::Refused(format!("ssh to {host} failed too"))
            }
            Ok(_) => Outcome::Back(None),
            Err(e) => Outcome::Refused(format!("could not run ssh: {e}")),
        }
    })
}

/// A container's place on the menu from its latest snapshot.
pub fn place_of(name: &str, snap: &Snapshot) -> Place {
    Place {
        name: name.to_string(),
        header: snap.header.clone(),
        workspace: snap.workspace.clone(),
        reach: Reach::Up,
    }
}

/// Every container's rows in one list: each container's in its own order, and the lists
/// interleaved by when their rows were last active, newest first — so a merge never reorders
/// a container's rows among themselves, and one container is its own order exactly.
pub fn merge(lists: &[&[Row]]) -> Vec<Row> {
    let mut at = vec![0usize; lists.len()];
    let mut out = Vec::with_capacity(lists.iter().map(|l| l.len()).sum());
    loop {
        let next = (0..lists.len())
            .filter(|&i| at[i] < lists[i].len())
            .max_by_key(|&i| (lists[i][at[i]].at, std::cmp::Reverse(i)));
        let Some(i) = next else { break };
        out.push(lists[i][at[i]].clone());
        at[i] += 1;
    }
    out
}

// ── the offloader, across every container ───────────────────────────────────

/// How long one container's pass may take before it is stopped: the local loop's bound.
const PASS_BOUND: Duration = Duration::from_secs(120);

/// `claude-sessions offload-all`: one offload pass in every container at once, each its own
/// `claude-sessions offload` run in the container over ssh — the measurement only means
/// anything inside the container's namespaces. Every line a pass prints comes back prefixed
/// with its container's name; a container that could not be reached, or whose pass failed,
/// says so on a line of its own, and makes the whole run fail once every other has finished.
/// Run from the hub's timer, every three minutes: the CPU budget is per window, so the cadence
/// is part of the rule (design § The offloader).
pub fn offload_all(dry_run: bool) -> io::Result<()> {
    let hosts = hosts();
    let mut args = vec!["offload"];
    if dry_run {
        args.push("--dry-run");
    }
    let started: Vec<(String, io::Result<std::process::Child>)> = hosts
        .iter()
        .map(|h| {
            let mut cmd = command(h, &args, false);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            (h.clone(), cmd.spawn())
        })
        .collect();
    // Read each pass's output on its own threads, so one container's slow pass never holds
    // another's pipe full.
    let mut running = Vec::new();
    for (host, child) in started {
        match child {
            Ok(mut child) => {
                let read = |r: Option<Box<dyn io::Read + Send>>| {
                    std::thread::spawn(move || {
                        let mut text = String::new();
                        if let Some(mut r) = r {
                            let _ = r.read_to_string(&mut text);
                        }
                        text
                    })
                };
                let out = read(child.stdout.take().map(|o| Box::new(o) as _));
                let err = read(child.stderr.take().map(|e| Box::new(e) as _));
                running.push((host, Ok((child, out, err))));
            }
            Err(e) => running.push((host, Err(e))),
        }
    }
    let deadline = Instant::now() + PASS_BOUND;
    let mut failed = 0;
    for (host, run) in running {
        let (mut child, out, err) = match run {
            Ok(r) => r,
            Err(e) => {
                say_as(&host, &format!("pass failed: could not start it: {e}"));
                failed += 1;
                continue;
            }
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        for line in out.join().unwrap_or_default().lines() {
            say_as(&host, line);
        }
        let err = err.join().unwrap_or_default();
        match status.and_then(|s| s.code()) {
            Some(0) => {
                for line in err.lines().filter(|l| !l.trim().is_empty()) {
                    say_as(&host, line);
                }
            }
            code => {
                failed += 1;
                let why = err
                    .lines()
                    .rfind(|l| !l.trim().is_empty())
                    .map(|l| format!(": {}", l.trim()))
                    .unwrap_or_default();
                match code {
                    None => say_as(
                        &host,
                        &format!("pass failed: stopped after {}s{why}", PASS_BOUND.as_secs()),
                    ),
                    Some(c) => say_as(&host, &format!("pass failed: exit {c}{why}")),
                }
            }
        }
    }
    if failed > 0 {
        return Err(io::Error::other(format!(
            "{failed} of {} container(s) did not finish a pass",
            hosts.len()
        )));
    }
    Ok(())
}

fn say_as(host: &str, line: &str) {
    say!("{host}: {line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::RowKey;

    fn row(host: &str, title: &str, at: u64) -> Row {
        Row {
            host: host.into(),
            key: RowKey::Slot(title.into()),
            wants_you: false,
            busy: false,
            unread: false,
            attached: false,
            offloaded: false,
            closed: false,
            archived: false,
            title: title.into(),
            age: String::new(),
            at,
            nested: false,
        }
    }

    fn titles(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.title.as_str()).collect()
    }

    #[test]
    fn rows_merge_newest_first_without_reordering_a_containers_own() {
        let a = [
            row("a", "a1", 50),
            row("a", "a2", 10),
            row("a", "a-socket", 90),
        ];
        let b = [row("b", "b1", 40), row("b", "b2", 20)];
        assert_eq!(
            titles(&merge(&[&a, &b])),
            ["a1", "b1", "b2", "a2", "a-socket"],
            "a's socket row stays after a's own rows, as a lists it"
        );
        assert_eq!(
            titles(&merge(&[&a])),
            ["a1", "a2", "a-socket"],
            "one is itself"
        );
        assert!(merge(&[]).is_empty());
    }

    #[test]
    fn the_command_for_a_container_is_ssh_asking_nothing_and_local_is_this_binary() {
        let cmd = command("infra-dev", &["agent", "exec", "7b"], true);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-t",
                "-o",
                "BatchMode=yes",
                "--",
                "infra-dev",
                "claude-sessions",
                "agent",
                "exec",
                "7b"
            ]
        );
        let local = command(LOCAL, &["agent"], false);
        assert_eq!(
            local.get_program(),
            std::env::current_exe().unwrap().as_os_str()
        );
    }

    /// `offload_all(true)` run in a process of its own over `hosts`, reached through `ssh`;
    /// what it printed.
    fn offload_all_over(hosts: &str, ssh: &std::path::Path) -> String {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hub::tests::offload_all_child", "--nocapture"])
            .env("CS_OFFLOAD_ALL_CHILD", "1")
            .env("CLAUDE_SESSIONS_HOSTS", hosts)
            .env("CLAUDE_SESSIONS_SSH", ssh)
            .env("CLAUDE_SESSIONS_REMOTE", "cs")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `offload_all` over stand-ins: a container whose pass prints and succeeds, one whose
    /// ssh fails as ssh does, and one whose pass fails — every line prefixed, every failure
    /// said, the run failing only once all have finished.
    #[test]
    fn offload_all_runs_every_containers_pass_and_says_which_failed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cs-offload-all-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            r#"#!/bin/sh
while [ $# -gt 0 ]; do case $1 in -T|-t) shift;; -o) shift 2;; --) shift; break;; *) break;; esac; done
host=$1; shift
case $host in
  good) echo "claude-1: kept — attached"; echo "args: $*"; echo "a note" >&2; echo >&2 ;;
  gone) echo "ssh: connect to host gone port 2222: Connection refused" >&2; exit 255 ;;
  bad)  echo "registry unreadable" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let text = offload_all_over("good gone bad", &ssh);
        for want in [
            "good: claude-1: kept — attached",
            "good: args: cs offload --dry-run",
            "good: a note",
            "gone: pass failed: exit 255: ssh: connect to host gone port 2222: Connection refused",
            "bad: pass failed: exit 1: registry unreadable",
            "RESULT: 2 of 3 container(s) did not finish a pass",
        ] {
            assert!(text.contains(want), "{want:?} not in {text}");
        }
        // A successful pass's blank stderr lines are not said.
        assert!(!text.lines().any(|l| l.trim_end() == "good:"), "{text}");
        // All of them succeeding is success.
        let text = offload_all_over("good", &ssh);
        assert!(text.contains("RESULT: ok"), "{text}");
        // One that could not even be started is a failure too, and counted.
        let text = offload_all_over("a b", &dir.join("no-such-ssh"));
        assert!(
            text.contains("a: pass failed: could not start it"),
            "{text}"
        );
        assert!(text.contains("RESULT: 2 of 2 container(s)"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A terminal whose child exits with the raw wait status given, or cannot be run.
    struct Exits(Option<i32>);

    impl Terminal for Exits {
        fn suspend(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn run(&mut self, _: &mut Command) -> io::Result<std::process::ExitStatus> {
            use std::os::unix::process::ExitStatusExt;
            match self.0 {
                Some(raw) => Ok(std::process::ExitStatus::from_raw(raw)),
                None => Err(io::Error::other("no ssh here")),
            }
        }
    }

    /// Exited with `code`, as a wait status.
    fn exited(code: i32) -> Option<i32> {
        Some(code << 8)
    }

    #[test]
    fn a_child_run_there_answers_its_code_or_why_it_could_not_run() {
        let child = json::parse(r#"{"argv":["claude"]}"#).unwrap();
        let ran = |host: &str, raw| json::to_line(&run_there(host, &child, &mut Exits(raw)));
        assert_eq!(ran("x", exited(0)), r#"{"code":0}"#);
        assert_eq!(ran("x", exited(3)), r#"{"code":3}"#);
        // 255 from ssh is ssh failing; from a child here, just its code.
        assert_eq!(ran("x", exited(255)), r#"{"error":"ssh to x failed"}"#);
        assert_eq!(ran(LOCAL, exited(255)), r#"{"code":255}"#);
        // Killed: the code a shell would say, 128 and the signal.
        assert_eq!(ran("x", Some(9)), r#"{"code":137}"#);
        assert_eq!(ran("x", None), r#"{"error":"no ssh here"}"#);
        let nothing = run_there("x", &Value::obj(), &mut Exits(exited(0)));
        assert!(json::to_line(&nothing).contains("asked for nothing"));
    }

    #[test]
    fn a_shell_where_the_agent_is_not_answering_says_when_ssh_failed() {
        let shell = |host: &str, raw| (shell_job(host))(&mut Exits(raw));
        assert_eq!(
            shell("x", exited(255)),
            Outcome::Refused("ssh to x failed too".into())
        );
        assert_eq!(shell(LOCAL, exited(255)), Outcome::Back(None));
        assert_eq!(shell("x", exited(0)), Outcome::Back(None));
        assert_eq!(
            shell("x", None),
            Outcome::Refused("could not run ssh: no ssh here".into())
        );
        let args = |host| {
            shell_command(host)
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(args(LOCAL), ["-l"]);
        assert_eq!(args("x"), ["-t", "-o", "BatchMode=yes", "--", "x"]);
    }

    #[test]
    fn a_connection_that_fails_tells_everything_waiting_on_it_why() {
        let conn = Conn::detached("a");
        let (tx, rx) = mpsc::channel();
        conn.waiting.lock().unwrap().insert(1, tx);
        conn.fail_all("the agent exited");
        assert!(matches!(rx.try_recv(), Ok(Reply::Gone(why)) if why == "the agent exited"));
        assert!(conn.waiting.lock().unwrap().is_empty());
    }

    /// Run by the test above in a process of its own, with the environment it set.
    #[test]
    fn offload_all_child() {
        if std::env::var_os("CS_OFFLOAD_ALL_CHILD").is_none() {
            return;
        }
        match offload_all(true) {
            Ok(()) => println!("RESULT: ok"),
            Err(e) => println!("RESULT: {e}"),
        }
    }
}
