//! The container's half of the hub: `claude-sessions agent`, run over `ssh <container>` by
//! the hub, and `claude-sessions agent exec`, what the hub runs at the owner's terminal when
//! an action here needs one.
//!
//! **Why there is a half here at all.** The hub cannot see a container's processes or its
//! sockets — the measurement the offloader stands on reads `/proc` and the socket-diagnostics
//! netlink, which answer only inside the container's own namespaces — and a slot's lock is a
//! `flock` on the container's own registry. So everything that reads or changes a container's
//! sessions runs in the container, as it always did, from the same code: `menu::gather` for
//! the rows, `launch.rs` for every action. The agent only carries it to the hub and back
//! (`proto.rs` is what it says). The binary is the hub's own: the hub installs it into a
//! volume every container mounts read-only (`claude-sessions install`), so the two are always
//! the same version, pinned in one place.
//!
//! **An action runs here; only its child runs at the hub.** `launch.rs` decides under the
//! slot's lock and writes the record before letting go, exactly as from a local menu; when it
//! reaches the point where it would hand the terminal to `zmx` or a shell, the agent's
//! [`Terminal`] sends the command to the hub instead, which runs it back in this container
//! with `ssh -t … claude-sessions agent exec <hex>`, and answers with how it ended. Then the
//! launcher carries on — reading the record, writing `last_attach` — here, under the lock it
//! knows.
//!
//! **It says nothing it was not asked, and exits when the hub goes.** Snapshots are sent when
//! a row changes and when the minute turns, never on a timer of their own, so an idle menu at
//! the hub still writes nothing to the owner's terminal. End of input is the hub gone, and the
//! agent leaves with it.

use crate::archive::{self, Mark};
use crate::clock;
use crate::json::{self, Value};
use crate::launch;
use crate::mem;
use crate::menu;
use crate::proto::{self, Child};
use crate::ui::{Header, Outcome, Row, RowKey, Terminal};
use crate::zmx;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How often the rows are read while nothing else happens — as the local menu did.
const POLL: Duration = Duration::from_secs(2);

/// The scrollback variables the owner's login brought to the hub, sent with each action: the
/// launcher leaves a variable the login set off the slot's command line, and the child it runs
/// here must then inherit the login's value (`launch::SCROLLBACK`).
static PRESETS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn presets() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    PRESETS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Whether the hub's login set this variable. `launch::Deps` asks it beside its own
/// environment.
pub fn preset(key: &str) -> bool {
    presets().contains_key(key)
}

/// Writes one message a line, from whichever thread has one. A write that fails means the
/// hub has gone; the read loop finds out on its own and ends the agent.
#[derive(Clone)]
struct Out(Arc<Mutex<Box<dyn Write + Send>>>);

impl Out {
    fn send(&self, v: &Value) {
        let mut out = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(out, "{}", json::to_line(v));
        let _ = out.flush();
    }
}

/// What the agent last said about its rows, so it says it again only when it changed.
struct Seen {
    zmx: zmx::Watch,
    rows: Vec<Row>,
    /// The age clock's minute when it last said it: ages are drawn to the minute.
    minute: u64,
    said: bool,
}

struct Agent {
    out: Out,
    workspace: String,
    seen: Mutex<Seen>,
    /// Actions waiting on the hub's `ran`, by id.
    waiting: Mutex<HashMap<u64, Sender<Value>>>,
}

fn header() -> Header {
    let m = mem::read();
    Header {
        host: menu::host(),
        memory: m.limit.zip(m.used()).map(|(limit, used)| (used, limit)),
        load: crate::statusline::host_load(),
    }
}

impl Agent {
    /// Read the rows, and send them if they changed, the minute turned, or `force` — after
    /// an action, so the hub has its result before it hears it is done. Returns the rows.
    fn look(&self, force: bool) -> Vec<Row> {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        let now = clock::now();
        let rows = menu::gather(now, &self.workspace, seen.zmx.sessions(force));
        let minute = menu::age_clock(now);
        if force || !seen.said || rows != seen.rows || minute != seen.minute {
            self.out
                .send(&proto::snapshot_to(&header(), &self.workspace, &rows));
            seen.said = true;
            seen.rows = rows.clone();
            seen.minute = minute;
        }
        rows
    }

    /// The rows the hub was last shown: what an action's key is looked for among, so an index
    /// means what it meant on the owner's screen.
    fn shown(&self) -> Vec<Row> {
        self.seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .rows
            .clone()
    }
}

/// `claude-sessions agent`: serve the hub until it goes.
pub fn serve() -> io::Result<()> {
    let agent = Arc::new(Agent {
        out: Out(Arc::new(Mutex::new(Box::new(io::stdout())))),
        workspace: menu::workspace(),
        seen: Mutex::new(Seen {
            zmx: zmx::Watch::default(),
            rows: Vec::new(),
            minute: 0,
            said: false,
        }),
        waiting: Mutex::new(HashMap::new()),
    });
    agent.look(true);
    {
        let agent = agent.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                agent.look(false);
            }
        });
    }
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = json::parse(&line) else {
            log(&format!(
                "agent: unreadable message: {}",
                line.chars().take(200).collect::<String>()
            ));
            continue;
        };
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if let Some(ran) = msg.get("ran") {
            let waiting = agent.waiting.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(tx) = waiting.get(&id) {
                let _ = tx.send(ran.clone());
            }
            continue;
        }
        if msg.get("op").is_some() {
            if let Some(Value::Obj(env)) = msg.get("env") {
                let mut p = presets();
                for (k, v) in env {
                    if let Some(v) = v.as_str() {
                        p.insert(k.clone(), v.to_string());
                    }
                }
            }
            let agent = agent.clone();
            std::thread::spawn(move || {
                let (tx, rx) = mpsc::channel();
                agent
                    .waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id, tx);
                let mut term = AtHub {
                    id,
                    out: agent.out.clone(),
                    ran: rx,
                };
                let outcome = act(&msg, &agent.shown(), &agent.workspace, &mut term);
                agent
                    .waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                agent.look(true);
                let mut done = Value::obj();
                done.set("id", Value::num(id as f64));
                done.set("done", proto::outcome_to(&outcome));
                agent.out.send(&done);
            });
        }
    }
    // The hub has gone. Whatever an action was waiting for will not come.
    Ok(())
}

/// The agent's terminal is the owner's at the hub: a child is sent there to run.
struct AtHub {
    id: u64,
    out: Out,
    ran: Receiver<Value>,
}

impl Terminal for AtHub {
    // Nothing of the agent's is on the owner's screen to hand over or take back.
    fn suspend(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn run(&mut self, cmd: &mut Command) -> io::Result<ExitStatus> {
        let mut child = Child::of(cmd);
        // The login's own values, for the variables the launcher left to it.
        for (k, v) in presets().iter() {
            child.env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        let mut msg = Value::obj();
        msg.set("id", Value::num(self.id as f64));
        msg.set("run", child.to_value());
        self.out.send(&msg);
        let gone = || io::Error::new(io::ErrorKind::BrokenPipe, "the hub went away");
        let ran = self.ran.recv().map_err(|_| gone())?;
        if let Some(e) = ran.get("error").and_then(Value::as_str) {
            return Err(io::Error::other(e.to_string()));
        }
        let code = ran.get("code").and_then(Value::as_u64).unwrap_or(1) as i32;
        Ok(ExitStatus::from_raw((code & 0xff) << 8))
    }
}

fn key_of(msg: &Value, field: &str) -> Option<RowKey> {
    proto::key_from(msg.get(field)?)
}

fn refused() -> Outcome {
    Outcome::Refused("that session is no longer listed".to_string())
}

/// One action, as the local menu did it (`run.rs` before the hub): find the row the hub
/// named among the rows it was shown, and hand it to the launcher.
fn act(msg: &Value, rows: &[Row], ws: &str, term: &mut dyn Terminal) -> Outcome {
    let index = |field: &str| {
        let key = key_of(msg, field)?;
        rows.iter().position(|r| r.key == key)
    };
    let id = |i: usize| match rows.get(i).map(|r| &r.key) {
        Some(RowKey::Conversation { id, .. }) => Some(id.clone()),
        _ => None,
    };
    match msg.get("op").and_then(Value::as_str).unwrap_or("") {
        "open" => {
            let Some(i) = index("key") else {
                return refused();
            };
            // Resumed, it leaves the archive. Marked kept first, so a resume that fails does
            // not leave it folded straight back by its age.
            if rows[i].archived {
                if let Some(id) = id(i) {
                    let _ = archive::set(&id, Mark::Kept(clock::now()));
                }
            }
            launch::open(rows, i, ws, term)
        }
        "new" => launch::new_session(rows, ws, term),
        "shell" => launch::shell(ws, term),
        // `c`: a loaded session is offloaded, an offloaded one closed — archived. An older hub
        // sends `close` for either, once its dialog is answered.
        "offload" => match index("key") {
            Some(i) => launch::offload(rows, i),
            None => refused(),
        },
        "close" => match index("key") {
            Some(i) => launch::close(rows, i),
            None => refused(),
        },
        "offload_then_open" => {
            let Some(victim) = index("victim") else {
                return refused();
            };
            let then = match msg.get("then") {
                Some(_) => match index("then") {
                    Some(i) => Some(i),
                    None => return refused(),
                },
                None => None,
            };
            launch::offload_then_open(rows, victim, then, ws, term)
        }
        op @ ("archive" | "unarchive") => {
            let Some(conv) = index("key").and_then(id) else {
                return refused();
            };
            let now = clock::now();
            let archiving = op == "archive";
            let mark = if archiving {
                Mark::Archived(now)
            } else {
                Mark::Kept(now)
            };
            Outcome::Back(Some(match archive::set(&conv, mark) {
                Ok(()) if archiving => "archived · c under Archived undoes it".into(),
                Ok(()) => "back under Closed".into(),
                Err(e) => format!("could not change the archive: {e}"),
            }))
        }
        other => Outcome::Refused(format!(
            "this container's claude-sessions does not know how to {other:?} — it may be older \
             than the hub's"
        )),
    }
}

/// `claude-sessions agent exec <hex>`: become the command the hub was sent, at the terminal
/// `ssh -t` gave it. Returns only if it could not be run.
pub fn exec(hex: &str) -> io::Error {
    let Some(child) = Child::from_hex(hex) else {
        return io::Error::new(io::ErrorKind::InvalidInput, "not a command this can run");
    };
    let Some(mut cmd) = child.command() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "an empty command");
    };
    cmd.exec()
}

fn log(line: &str) {
    let dir = crate::registry::dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("agent.log"))
    {
        let _ = writeln!(f, "{} {}", clock::now(), line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_sent_to_the_hub_carries_the_logins_scrollback_values() {
        presets().insert("CLAUDE_CODE_DISABLE_MOUSE".into(), String::new());
        let (tx, rx) = mpsc::channel();
        let mut term = AtHub {
            id: 7,
            out: Out(Arc::new(Mutex::new(Box::new(io::sink())))),
            ran: rx,
        };
        tx.send(json::parse(r#"{"code":3}"#).unwrap()).unwrap();
        let mut cmd = Command::new("zmx");
        let status = term.run(&mut cmd).unwrap();
        assert_eq!(status.code(), Some(3), "the hub's answer is the status");
        assert!(preset("CLAUDE_CODE_DISABLE_MOUSE"));
        assert!(!preset("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"));
        tx.send(json::parse(r#"{"error":"ssh failed"}"#).unwrap())
            .unwrap();
        assert!(term.run(&mut cmd).is_err(), "a child that could not run");
        drop(tx);
        assert!(term.run(&mut cmd).is_err(), "and a hub that has gone");
        presets().clear();
    }

    #[test]
    fn an_action_on_a_row_no_longer_shown_is_refused_and_an_unknown_one_says_so() {
        struct No;
        impl Terminal for No {
            fn suspend(&mut self) -> io::Result<()> {
                panic!("nothing should run")
            }
            fn resume(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let msg = json::parse(r#"{"id":1,"op":"close","key":{"slot":"claude-9"}}"#).unwrap();
        assert_eq!(act(&msg, &[], "/w", &mut No), refused());
        let msg = json::parse(r#"{"id":1,"op":"fly"}"#).unwrap();
        assert!(
            matches!(act(&msg, &[], "/w", &mut No), Outcome::Refused(why) if why.contains("fly"))
        );
    }
}
