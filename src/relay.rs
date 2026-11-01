//! `claude-sessions relay`: the hub's end of the remote view, one per connected phone or WSL
//! client, run over `ssh -T -C dev claude-sessions relay` (`docs/remote-view.md` § *The
//! pieces*).
//!
//! **Two jobs.** The home: every container's snapshot, as each container's agent sends it to
//! the hub's own menu (`hub::connect`), passed on whole so the client draws the same home
//! with the same code. And one conversation at a time: `claude-sessions remote …` run in its
//! container over ssh (`launch::remote_open` and the others), its lines reduced here to the
//! few numbered items the client draws (`reduce.rs`), and the client's actions turned into
//! claude's control messages on the way back.
//!
//! **The link is metered, so what crosses it is small and only when it changes.** Home
//! messages are sent only while no conversation is open and only when one reads differently;
//! a conversation's ops are batched a tenth of a second, consecutive text merged. Nothing is
//! sent on a timer.
//!
//! **A reconnect is a new relay.** The client says which slot it had, the holder's run and the
//! last line number whose ops it holds (`re`); the relay reads the conversation's log again
//! from the start, which rebuilds the reducer exactly, and sends only the ops of later lines.
//! Another run of the holder means the client's items are stale: it is told to reset.
//! Requests claude made are answered only from lines that arrived live: one in the log from
//! before this relay was answered by whichever relay was live then.
//!
//! The core is [`Relay`], inputs in and effects out, with no I/O, so it is tested as a state
//! machine; [`serve`] is the threads and pipes around it.

use crate::hub::{self, Event};
use crate::json::{self, Value};
use crate::proto;
use crate::reduce::{self, Op, Reducer};
use crate::sdk::{self, Msg};
use crate::ui::{Row, RowKey};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The wire's version, said in the hello both ways.
pub const VERSION: u64 = 1;
/// How long ops wait to be sent together.
const BATCH: Duration = Duration::from_millis(100);
/// What claude is told when the owner says no to a tool: Claude Code's own words.
const DENY: &str = "The user doesn't want to proceed with this tool use. The tool use was \
                    rejected (eg. if it was a file edit, the new_string was NOT written to the \
                    file). STOP what you are doing and wait for the user to tell you how to \
                    proceed.";

/// Something that happened, for [`Relay::handle`].
#[derive(Debug)]
pub enum Input {
    /// A line from the client.
    Client(Value),
    Hub(Event),
    /// A line from the open conversation's `remote` command.
    Conv {
        epoch: u64,
        line: String,
    },
    /// The open conversation's command ended, and what it last said.
    ConvGone {
        epoch: u64,
        why: String,
    },
}

/// What the relay asks of the pipes around it.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// A line to the client.
    Send(Value),
    /// Run `claude-sessions <args>` in `host`, its lines to come back as `Conv { epoch }`.
    Open {
        epoch: u64,
        host: String,
        args: Vec<String>,
    },
    /// A line to the open conversation, for claude.
    ToConv { epoch: u64, line: Value },
    /// End the conversation's command.
    CloseConv { epoch: u64 },
}

/// Where a client that reconnected had got to.
#[derive(Debug, Clone)]
struct Had {
    run: String,
    n: u64,
}

struct Conv {
    epoch: u64,
    host: String,
    /// Known once the command's first line says it.
    slot: Option<String>,
    /// Known once the holder's header has come.
    run: Option<String>,
    had: Option<Had>,
    /// The first line number that arrived live, not from the log as it stood.
    live_from: u64,
    /// Ops of lines up to this the client holds already.
    skip_to: u64,
    reducer: Reducer,
    /// The id of an `initialize` sent for the client's model list.
    models_req: Option<String>,
    pending: Vec<Op>,
    pending_n: u64,
    pending_since: Option<Instant>,
}

pub struct Relay {
    hosts: Vec<String>,
    /// Each container's latest home message, and what the client was last sent of it.
    home: HashMap<String, Value>,
    sent: HashMap<String, Value>,
    hub_header: Option<Value>,
    sent_hub: Option<Value>,
    /// Each container's rows, for a stored conversation's title and cwd.
    rows: HashMap<String, Vec<Row>>,
    conv: Option<Conv>,
    epoch: u64,
    next_req: u64,
    greeted: bool,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn msg(t: &str, rest: Vec<(&str, Value)>) -> Value {
    let mut v = Value::obj();
    v.set("t", Value::string(t));
    for (k, x) in rest {
        v.set(k, x);
    }
    v
}

fn err(why: &str) -> Effect {
    Effect::Send(msg("err", vec![("x", Value::string(why))]))
}

impl Relay {
    pub fn new(hosts: Vec<String>) -> Relay {
        Relay {
            hosts,
            home: HashMap::new(),
            sent: HashMap::new(),
            hub_header: None,
            sent_hub: None,
            rows: HashMap::new(),
            conv: None,
            epoch: 0,
            next_req: 0,
            greeted: false,
        }
    }

    fn req_id(&mut self) -> String {
        self.next_req += 1;
        format!("rv{}-{}", std::process::id(), self.next_req)
    }

    /// The hub's own header, for the home's top line, as it now reads.
    pub fn set_hub_header(&mut self, h: Value) -> Vec<Effect> {
        self.hub_header = Some(h);
        let mut out = Vec::new();
        self.send_home(&mut out);
        out
    }

    /// When a batch of ops is due, if one is waiting.
    pub fn deadline(&self) -> Option<Instant> {
        self.conv
            .as_ref()
            .and_then(|c| c.pending_since)
            .map(|t| t + BATCH)
    }

    /// Send what is due by `now`.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        let mut out = Vec::new();
        if let Some(c) = self.conv.as_mut() {
            if c.pending_since.is_some_and(|t| now >= t + BATCH) {
                flush(c, &mut out);
            }
        }
        out
    }

    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let mut out = Vec::new();
        match input {
            Input::Client(v) => self.client(&v, &mut out),
            Input::Hub(ev) => {
                let (host, m) = match ev {
                    Event::Snapshot(host, snap) => {
                        let m = msg(
                            "snap",
                            vec![
                                ("c", Value::string(&host)),
                                (
                                    "s",
                                    proto::snapshot_to(&snap.header, &snap.workspace, &snap.rows),
                                ),
                            ],
                        );
                        self.rows.insert(host.clone(), snap.rows);
                        (host, m)
                    }
                    Event::Down(host, why) => {
                        let m = msg(
                            "down",
                            vec![("c", Value::string(&host)), ("x", Value::string(why))],
                        );
                        (host, m)
                    }
                };
                self.home.insert(host, m);
                self.send_home(&mut out);
            }
            Input::Conv { epoch, line } => self.conv_line(epoch, &line, now, &mut out),
            Input::ConvGone { epoch, why } => {
                if self.conv.as_ref().is_some_and(|c| c.epoch == epoch) {
                    let mut c = self.conv.take().expect("checked");
                    flush(&mut c, &mut out);
                    let why = if c.slot.is_none() && why.is_empty() {
                        "the container did not answer".to_string()
                    } else {
                        why
                    };
                    out.push(Effect::Send(msg("end", vec![("x", Value::string(why))])));
                    self.sent.clear();
                    self.sent_hub = None;
                    self.send_home(&mut out);
                }
            }
        }
        out
    }

    /// Every home message that reads differently from what the client has, while it is home.
    fn send_home(&mut self, out: &mut Vec<Effect>) {
        if self.conv.is_some() || !self.greeted {
            return;
        }
        if self.hub_header != self.sent_hub {
            if let Some(h) = &self.hub_header {
                out.push(Effect::Send(msg("hub", vec![("h", h.clone())])));
            }
            self.sent_hub = self.hub_header.clone();
        }
        for host in &self.hosts {
            if let Some(m) = self.home.get(host) {
                if self.sent.get(host) != Some(m) {
                    out.push(Effect::Send(m.clone()));
                    self.sent.insert(host.clone(), m.clone());
                }
            }
        }
    }

    fn client(&mut self, v: &Value, out: &mut Vec<Effect>) {
        let t = s(v, "t").unwrap_or_default();
        match t.as_str() {
            "hi" => {
                out.push(Effect::Send(msg(
                    "hi",
                    vec![
                        ("v", Value::num(VERSION as f64)),
                        (
                            "hosts",
                            Value::Arr(self.hosts.iter().map(Value::string).collect()),
                        ),
                    ],
                )));
                self.greeted = true;
                self.sent.clear();
                self.sent_hub = None;
                self.send_home(out);
            }
            "open" | "new" | "re" => self.open(&t, v, out),
            "close" => {
                if let Some(c) = self.conv.take() {
                    out.push(Effect::CloseConv { epoch: c.epoch });
                    self.sent.clear();
                    self.sent_hub = None;
                    self.send_home(out);
                }
            }
            _ => self.action(&t, v, out),
        }
    }

    fn open(&mut self, t: &str, v: &Value, out: &mut Vec<Effect>) {
        if let Some(c) = self.conv.take() {
            out.push(Effect::CloseConv { epoch: c.epoch });
        }
        let Some(host) = s(v, "c").filter(|h| self.hosts.contains(h)) else {
            out.push(err("no such container"));
            return;
        };
        let mut had = None;
        let args: Vec<String> = match t {
            "new" => vec!["remote".into(), "new".into()],
            "re" => {
                let Some(slot) = s(v, "s") else {
                    out.push(err("re needs the slot"));
                    return;
                };
                had = Some(Had {
                    run: s(v, "run").unwrap_or_default(),
                    n: v.get("n").and_then(Value::as_u64).unwrap_or(0),
                });
                vec!["remote".into(), "open".into(), slot]
            }
            _ => match v.get("k").and_then(proto::key_from) {
                Some(RowKey::Slot(slot)) => vec!["remote".into(), "open".into(), slot],
                Some(RowKey::Conversation {
                    id,
                    cwd,
                    harness: crate::harness::Harness::Claude,
                }) => {
                    let title = self
                        .rows
                        .get(&host)
                        .and_then(|rows| {
                            rows.iter().find(|r| {
                                matches!(&r.key, RowKey::Conversation { id: i, .. } if *i == id)
                            })
                        })
                        .map(|r| r.title.clone())
                        .unwrap_or_else(|| id.clone());
                    vec!["remote".into(), "resume".into(), id, cwd, title]
                }
                Some(RowKey::Conversation { .. }) => {
                    out.push(err("pi opens only in a terminal"));
                    return;
                }
                _ => {
                    out.push(err("that row is not a conversation"));
                    return;
                }
            },
        };
        self.epoch += 1;
        self.conv = Some(Conv {
            epoch: self.epoch,
            host: host.clone(),
            slot: None,
            run: None,
            had,
            live_from: 1,
            skip_to: 0,
            reducer: Reducer::new(),
            models_req: None,
            pending: Vec::new(),
            pending_n: 0,
            pending_since: None,
        });
        out.push(Effect::Open {
            epoch: self.epoch,
            host,
            args,
        });
    }

    fn conv_line(&mut self, epoch: u64, line: &str, now: Instant, out: &mut Vec<Effect>) {
        let Some(c) = self.conv.as_mut().filter(|c| c.epoch == epoch) else {
            return;
        };
        if c.slot.is_none() {
            let v = json::parse(line.trim()).unwrap_or(Value::Null);
            if let Some(slot) = s(&v, "slot") {
                c.slot = Some(slot);
            } else {
                let why = s(&v, "error").unwrap_or_else(|| "the container did not say".into());
                out.push(err(&why));
                out.push(Effect::CloseConv { epoch });
                self.conv = None;
                self.sent.clear();
                self.sent_hub = None;
                self.send_home(out);
            }
            return;
        }
        if c.run.is_none() {
            let v = json::parse(line.trim()).unwrap_or(Value::Null);
            let run = s(&v, "run").unwrap_or_default();
            c.live_from = v.get("next").and_then(Value::as_u64).unwrap_or(1);
            match c.had.take() {
                Some(h) if h.run == run => c.skip_to = h.n,
                Some(_) => out.push(Effect::Send(msg("reset", vec![]))),
                None => {}
            }
            out.push(Effect::Send(msg(
                "open",
                vec![
                    ("c", Value::string(&c.host)),
                    ("s", Value::string(c.slot.clone().unwrap_or_default())),
                    ("run", Value::string(&run)),
                ],
            )));
            c.run = Some(run);
            return;
        }
        let Some((n, body)) = line.split_once('\t') else {
            return;
        };
        let Ok(n) = n.parse::<u64>() else { return };
        let Ok(v) = json::parse(body.trim()) else {
            return;
        };
        let live = n >= c.live_from;
        let fed = c.reducer.feed(&v);
        if live {
            for line in fed.to_claude {
                out.push(Effect::ToConv { epoch, line });
            }
        }
        if c.models_req.is_some() {
            if let Msg::Response {
                request_id, body, ..
            } = sdk::classify(&v)
            {
                if c.models_req.as_deref() == Some(request_id.as_str()) {
                    c.models_req = None;
                    out.push(Effect::Send(models(&body)));
                }
            }
        }
        if n > c.skip_to {
            c.pending.extend(fed.ops);
        }
        c.pending_n = n;
        if live {
            if !c.pending.is_empty() && c.pending_since.is_none() {
                c.pending_since = Some(now);
            }
        } else if n + 1 >= c.live_from || c.pending.len() >= 500 {
            // Caught up with the log as it stood, or a long replay's batch is full.
            flush(c, out);
        }
    }

    fn action(&mut self, t: &str, v: &Value, out: &mut Vec<Effect>) {
        let id = self.req_id();
        let Some(c) = self.conv.as_mut() else {
            out.push(err("no conversation is open"));
            return;
        };
        if c.run.is_none() {
            out.push(err("the conversation is still opening"));
            return;
        }
        let epoch = c.epoch;
        let mut to = |line: Value| out.push(Effect::ToConv { epoch, line });
        match t {
            "say" => to(sdk::user_text(&s(v, "x").unwrap_or_default())),
            "perm" | "ans" => {
                let r = s(v, "r").unwrap_or_default();
                let Some(input) = c.reducer.waiting_input(&r).cloned() else {
                    out.push(err("answered elsewhere"));
                    return;
                };
                let no = s(v, "x").unwrap_or_else(|| DENY.to_string());
                let line = match (t, s(v, "a").as_deref()) {
                    ("perm", Some("y")) => sdk::allow(&r, input),
                    ("perm", Some("a")) => {
                        sdk::allow_always(&r, input, c.reducer.waiting_suggestions(&r))
                    }
                    ("ans", _) if matches!(v.get("a"), Some(Value::Obj(_))) => {
                        let answers: Vec<(String, String)> = match v.get("a") {
                            Some(Value::Obj(m)) => m
                                .iter()
                                .filter_map(|(q, a)| Some((q.clone(), a.as_str()?.to_string())))
                                .collect(),
                            _ => Vec::new(),
                        };
                        sdk::answer_questions(&r, &input, &answers)
                    }
                    _ => sdk::deny(&r, &no),
                };
                to(line);
            }
            "model" => to(sdk::set_model(&id, &s(v, "m").unwrap_or_default())),
            "effort" => to(sdk::set_effort(&id, s(v, "e").as_deref())),
            "mode" => to(sdk::set_permission_mode(
                &id,
                &s(v, "m").unwrap_or_default(),
            )),
            "rename" => {
                let sid = c.reducer.meta().session_id.clone();
                to(sdk::rename(&id, &s(v, "x").unwrap_or_default(), &sid));
            }
            "esc" => to(sdk::interrupt(&id)),
            "exit" => to(sdk::end_session(&id)),
            "models" => {
                c.models_req = Some(id.clone());
                to(sdk::initialize(&id));
            }
            "full" => {
                let item = v.get("id").and_then(Value::as_u64).unwrap_or(0);
                let text = c.reducer.full_result(item).unwrap_or("").to_string();
                out.push(Effect::Send(msg(
                    "full",
                    vec![("id", Value::num(item as f64)), ("x", Value::string(text))],
                )));
            }
            _ => out.push(err(&format!("{t:?} is not something the relay knows"))),
        }
    }
}

/// Send what is waiting, merged, then the line number the client now holds every op up to.
fn flush(c: &mut Conv, out: &mut Vec<Effect>) {
    c.pending_since = None;
    if c.pending.is_empty() {
        return;
    }
    for op in reduce::coalesce(std::mem::take(&mut c.pending)) {
        out.push(Effect::Send(op.to_value()));
    }
    out.push(Effect::Send(msg(
        "n",
        vec![("n", Value::num(c.pending_n as f64))],
    )));
}

/// The model list from `initialize`'s answer, cut to what `/model` draws.
fn models(body: &Value) -> Value {
    let list = body
        .get("models")
        .and_then(Value::as_arr)
        .unwrap_or(&[])
        .iter()
        .map(|m| {
            let mut o = Value::obj();
            o.set("v", Value::string(s(m, "value").unwrap_or_default()));
            o.set("d", Value::string(s(m, "displayName").unwrap_or_default()));
            o.set("x", Value::string(s(m, "description").unwrap_or_default()));
            o
        })
        .collect();
    msg("models", vec![("m", Value::Arr(list))])
}

// ── the pipes ───────────────────────────────────────────────────────────────

fn noop() {}

/// The open conversation's command, as the relay holds it.
struct Running {
    epoch: u64,
    child: Child,
    stdin: Option<ChildStdin>,
}

/// `claude-sessions relay`: serve one client on stdin and stdout until it goes.
pub fn serve() -> io::Result<()> {
    let hosts = hub::hosts();
    let (tx, rx) = mpsc::channel::<Input>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = json::parse(line.trim()) {
                    if tx.send(Input::Client(v)).is_err() {
                        return;
                    }
                }
            }
            // The client gone: an empty object that says nothing ends the loop below.
            let _ = tx.send(Input::Client(Value::Null));
        });
    }
    let (htx, hrx) = mpsc::channel::<Event>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for ev in hrx {
                if tx.send(Input::Hub(ev)).is_err() {
                    return;
                }
            }
        });
    }
    let mut conns: HashMap<String, Arc<hub::Conn>> = hosts
        .iter()
        .map(|h| (h.clone(), hub::connect(h, htx.clone(), noop)))
        .collect();
    let mut retry: HashMap<String, Instant> = HashMap::new();
    let mut relay = Relay::new(hosts);
    let mut running: Option<Running> = None;
    let load_every = crate::run::load_every();
    let mut next_load = Instant::now();
    let mut stdout = io::stdout().lock();

    loop {
        let now = Instant::now();
        let mut effects = Vec::new();
        if now >= next_load {
            let h = proto::header_to(&crate::run::home_header());
            effects.extend(relay.set_hub_header(h));
            next_load = now + load_every;
        }
        let due: Vec<String> = retry
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(h, _)| h.clone())
            .collect();
        for h in due {
            retry.remove(&h);
            conns.insert(h.clone(), hub::connect(&h, htx.clone(), noop));
        }
        let mut wake = next_load;
        if let Some(d) = relay.deadline() {
            wake = wake.min(d);
        }
        if let Some(r) = retry.values().min() {
            wake = wake.min(*r);
        }
        match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(Input::Client(Value::Null)) => break,
            Ok(input) => {
                if let Input::Hub(Event::Down(h, _)) = &input {
                    retry.insert(h.clone(), Instant::now() + crate::run::retry());
                }
                effects.extend(relay.handle(input, Instant::now()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        effects.extend(relay.tick(Instant::now()));
        for e in effects {
            match e {
                Effect::Send(v) => {
                    if writeln!(stdout, "{}", json::to_line(&v)).is_err() {
                        return finish(running);
                    }
                }
                Effect::Open { epoch, host, args } => {
                    if let Some(r) = running.take() {
                        stop(r);
                    }
                    running = open(epoch, &host, &args, &tx);
                }
                Effect::ToConv { epoch, line } => {
                    if let Some(r) = running.as_mut().filter(|r| r.epoch == epoch) {
                        if let Some(pipe) = r.stdin.as_mut() {
                            let _ = writeln!(pipe, "{}", json::to_line(&line))
                                .and_then(|()| pipe.flush());
                        }
                    }
                }
                Effect::CloseConv { epoch } => {
                    if running.as_ref().is_some_and(|r| r.epoch == epoch) {
                        stop(running.take().expect("checked"));
                    }
                }
            }
        }
        if stdout.flush().is_err() {
            break;
        }
    }
    finish(running)
}

fn finish(running: Option<Running>) -> io::Result<()> {
    if let Some(r) = running {
        stop(r);
    }
    Ok(())
}

/// End the conversation's command: its input closed, which ends the tap, and the ssh killed
/// for good measure.
fn stop(mut r: Running) {
    drop(r.stdin.take());
    let _ = r.child.kill();
    let _ = r.child.wait();
}

fn open(epoch: u64, host: &str, args: &[String], tx: &Sender<Input>) -> Option<Running> {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = hub::command(host, &argv, false);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Input::ConvGone {
                epoch,
                why: format!("could not run ssh: {e}"),
            });
            return None;
        }
    };
    let stdin = child.stdin.take();
    let said = Arc::new(Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let said = said.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    *said.lock().unwrap_or_else(|p| p.into_inner()) = line;
                }
            }
        });
    }
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx
                            .send(Input::Conv {
                                epoch,
                                line: line.clone(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            // A moment for stderr's last line, which is the reason when ssh failed.
            std::thread::sleep(Duration::from_millis(50));
            let why = said.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let _ = tx.send(Input::ConvGone { epoch, why });
        });
    }
    Some(Running {
        epoch,
        child,
        stdin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;
    use crate::proto::Snapshot;
    use crate::ui::Header;

    fn relay() -> Relay {
        Relay::new(vec!["claude-sessions".into(), "or3-dev".into()])
    }

    fn sent(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send(v) => Some(json::to_line(v)),
                _ => None,
            })
            .collect()
    }

    fn to_claude(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::ToConv { line, .. } => Some(json::to_line(line)),
                _ => None,
            })
            .collect()
    }

    fn client(r: &mut Relay, line: &str) -> Vec<Effect> {
        r.handle(Input::Client(parse(line).unwrap()), Instant::now())
    }

    fn row(slot: &str, title: &str) -> Row {
        Row {
            host: "claude-sessions".into(),
            key: RowKey::Slot(slot.into()),
            wants_you: false,
            busy: false,
            unread: false,
            attached: false,
            offloaded: false,
            closed: false,
            archived: false,
            title: title.into(),
            age: "2m".into(),
            at: 1,
            nested: false,
            remote: true,
        }
    }

    fn snap(host: &str, rows: Vec<Row>) -> Input {
        Input::Hub(Event::Snapshot(
            host.into(),
            Snapshot {
                header: Header {
                    host: host.into(),
                    memory: None,
                    load: None,
                },
                workspace: "/workspace".into(),
                rows,
            },
        ))
    }

    /// Open `claude-3` and take the command's first two lines: the slot, and a header whose
    /// next line is `next`.
    fn opened(r: &mut Relay, next: u64) -> (u64, Vec<Effect>) {
        let fx = client(
            r,
            r#"{"t":"open","c":"claude-sessions","k":{"slot":"claude-3"}}"#,
        );
        let Some(Effect::Open { epoch, host, args }) = fx.into_iter().last() else {
            panic!("no open");
        };
        assert_eq!(host, "claude-sessions");
        assert_eq!(args, ["remote", "open", "claude-3"]);
        let now = Instant::now();
        let mut fx = r.handle(
            Input::Conv {
                epoch,
                line: r#"{"slot":"claude-3"}"#.into(),
            },
            now,
        );
        fx.extend(r.handle(
            Input::Conv {
                epoch,
                line: format!(r#"{{"next":{next},"run":"run-1"}}"#),
            },
            now,
        ));
        (epoch, fx)
    }

    fn line(r: &mut Relay, epoch: u64, n: u64, body: &str) -> Vec<Effect> {
        r.handle(
            Input::Conv {
                epoch,
                line: format!("{n}\t{body}\n"),
            },
            Instant::now(),
        )
    }

    const PROMPT: &str =
        r#"{"type":"user","message":{"role":"user","content":"hi"},"isReplay":true}"#;
    const ASK: &str = r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch x"}}}"#;
    const HOOK: &str =
        r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback"}}"#;

    #[test]
    fn the_home_is_sent_after_the_hello_and_only_when_it_reads_differently() {
        let mut r = relay();
        assert!(
            sent(&r.handle(
                snap("claude-sessions", vec![row("claude-3", "a")]),
                Instant::now()
            ))
            .is_empty(),
            "nothing before the client has said hello"
        );
        let fx = client(&mut r, r#"{"t":"hi","v":1}"#);
        let lines = sent(&fx);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains(r#""t":"hi""#) && lines[0].contains("or3-dev"));
        assert!(lines[1].contains(r#""t":"snap""#) && lines[1].contains(r#""remote":true"#));
        let same = r.handle(
            snap("claude-sessions", vec![row("claude-3", "a")]),
            Instant::now(),
        );
        assert!(
            sent(&same).is_empty(),
            "the same snapshot again costs nothing"
        );
        let down = r.handle(
            Input::Hub(Event::Down("or3-dev".into(), "ssh: timed out".into())),
            Instant::now(),
        );
        assert_eq!(
            sent(&down),
            [r#"{"c":"or3-dev","t":"down","x":"ssh: timed out"}"#]
        );
        let h = r.set_hub_header(parse(r#"{"host":"dev"}"#).unwrap());
        assert_eq!(sent(&h), [r#"{"h":{"host":"dev"},"t":"hub"}"#]);
        assert!(sent(&r.set_hub_header(parse(r#"{"host":"dev"}"#).unwrap())).is_empty());

        // While a conversation is open the home is held, and sent whole when it closes.
        opened(&mut r, 1);
        let held = r.handle(
            snap("claude-sessions", vec![row("claude-3", "b")]),
            Instant::now(),
        );
        assert!(sent(&held).is_empty());
        let fx = client(&mut r, r#"{"t":"close"}"#);
        assert!(matches!(fx[0], Effect::CloseConv { .. }));
        let lines = sent(&fx);
        assert_eq!(
            lines.len(),
            3,
            "the hub's header and both containers: {lines:?}"
        );
        assert!(lines[1].contains(r#""title":"b""#));
    }

    #[test]
    fn an_open_replays_the_log_in_one_batch_and_answers_only_what_arrives_live() {
        let mut r = relay();
        client(&mut r, r#"{"t":"hi","v":1}"#);
        let (epoch, fx) = opened(&mut r, 3);
        assert_eq!(
            sent(&fx),
            [r#"{"c":"claude-sessions","run":"run-1","s":"claude-3","t":"open"}"#]
        );
        assert!(
            line(&mut r, epoch, 1, PROMPT).is_empty(),
            "held until caught up"
        );
        let fx = line(&mut r, epoch, 2, HOOK);
        assert!(
            to_claude(&fx).is_empty(),
            "a request from before was some live relay's"
        );
        assert_eq!(
            sent(&fx),
            [
                r#"{"id":1,"k":"user","t":"add","x":"hi"}"#,
                r#"{"s":"working","t":"st"}"#,
                r#"{"n":2,"t":"n"}"#
            ]
        );
        // Live: answered at once, and the ops wait for the batch.
        let fx = line(&mut r, epoch, 3, HOOK);
        assert_eq!(
            to_claude(&fx).len(),
            1,
            "refused, as the view does not serve it"
        );
        let fx = line(&mut r, epoch, 4, ASK);
        assert!(sent(&fx).is_empty());
        let due = r.deadline().expect("a batch is waiting");
        assert!(sent(&r.tick(due - Duration::from_millis(1))).is_empty());
        let lines = sent(&r.tick(due));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains(r#""k":"perm""#));
        assert_eq!(lines[2], r#"{"n":4,"t":"n"}"#);
    }

    #[test]
    fn a_reconnect_of_the_same_run_sends_only_what_the_client_lacks_and_another_run_resets() {
        for (run, want_reset) in [("run-1", false), ("run-0", true)] {
            let mut r = relay();
            client(&mut r, r#"{"t":"hi","v":1}"#);
            let fx = client(
                &mut r,
                &format!(
                    r#"{{"t":"re","c":"claude-sessions","s":"claude-3","run":"{run}","n":1}}"#
                ),
            );
            let Some(Effect::Open { epoch, args, .. }) = fx.into_iter().last() else {
                panic!("no open");
            };
            assert_eq!(args, ["remote", "open", "claude-3"]);
            let now = Instant::now();
            r.handle(
                Input::Conv {
                    epoch,
                    line: r#"{"slot":"claude-3"}"#.into(),
                },
                now,
            );
            let fx = r.handle(
                Input::Conv {
                    epoch,
                    line: r#"{"next":3,"run":"run-1"}"#.into(),
                },
                now,
            );
            assert_eq!(sent(&fx)[0] == r#"{"t":"reset"}"#, want_reset, "{run}");
            line(&mut r, epoch, 1, PROMPT);
            let lines = sent(&line(&mut r, epoch, 2, ASK));
            let has_prompt = lines.iter().any(|l| l.contains(r#""k":"user""#));
            assert_eq!(has_prompt, want_reset, "{run}: {lines:?}");
            assert!(
                lines.iter().any(|l| l.contains(r#""k":"perm""#)),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn answers_carry_claudes_input_back_and_one_not_waiting_is_answered_elsewhere() {
        let mut r = relay();
        client(&mut r, r#"{"t":"hi","v":1}"#);
        let (epoch, _) = opened(&mut r, 1);
        line(&mut r, epoch, 1, ASK);
        let fx = client(&mut r, r#"{"t":"perm","r":"p1","a":"y"}"#);
        assert_eq!(
            to_claude(&fx),
            [
                r#"{"response":{"request_id":"p1","response":{"behavior":"allow","updatedInput":{"command":"touch x"}},"subtype":"success"},"type":"control_response"}"#
            ]
        );
        let fx = client(&mut r, r#"{"t":"perm","r":"p1","a":"n"}"#);
        assert_eq!(
            to_claude(&fx).len(),
            1,
            "before the log says it was answered, the holder is the one to drop it"
        );
        // The log says it was answered (by this view or another): now it is not waiting.
        let allow = to_line_of(&sdk::allow(
            "p1",
            parse(r#"{"command":"touch x"}"#).unwrap(),
        ));
        line(
            &mut r,
            epoch,
            2,
            &format!(r#"{{"type":"_in","line":{allow}}}"#),
        );
        let fx = client(&mut r, r#"{"t":"perm","r":"p1","a":"n"}"#);
        assert!(to_claude(&fx).is_empty());
        assert_eq!(sent(&fx), [r#"{"t":"err","x":"answered elsewhere"}"#]);

        let fx = client(&mut r, r#"{"t":"say","x":"/compact"}"#);
        assert_eq!(
            to_claude(&fx),
            [r#"{"message":{"content":"/compact","role":"user"},"type":"user"}"#]
        );
        let fx = client(&mut r, r#"{"t":"model","m":"opus"}"#);
        assert!(to_claude(&fx)[0].contains(r#""subtype":"set_model""#));
        let fx = client(&mut r, r#"{"t":"effort","e":null}"#);
        assert!(to_claude(&fx)[0].contains(r#""effortLevel":null"#));
        let fx = client(&mut r, r#"{"t":"teleport"}"#);
        assert!(sent(&fx)[0].contains("not something the relay knows"));
    }

    fn to_line_of(v: &Value) -> String {
        json::to_line(v)
    }

    #[test]
    fn a_container_that_refuses_to_open_says_why_and_the_client_is_home_again() {
        let mut r = relay();
        r.handle(
            snap("claude-sessions", vec![row("claude-3", "a")]),
            Instant::now(),
        );
        client(&mut r, r#"{"t":"hi","v":1}"#);
        let fx = client(&mut r, r#"{"t":"new","c":"claude-sessions"}"#);
        let Some(Effect::Open { epoch, args, .. }) = fx.into_iter().last() else {
            panic!("no open");
        };
        assert_eq!(args, ["remote", "new"]);
        let fx = r.handle(
            Input::Conv {
                epoch,
                line: r#"{"error":"no room for another session here"}"#.into(),
            },
            Instant::now(),
        );
        let lines = sent(&fx);
        assert_eq!(
            lines[0],
            r#"{"t":"err","x":"no room for another session here"}"#
        );
        assert!(lines[1].contains(r#""t":"snap""#), "home again: {lines:?}");
        assert!(fx.iter().any(|e| matches!(e, Effect::CloseConv { .. })));
        // Its command's end, arriving after, is for a conversation no longer open.
        assert!(
            r.handle(
                Input::ConvGone {
                    epoch,
                    why: String::new()
                },
                Instant::now()
            )
            .is_empty()
        );
        assert!(
            sent(&client(
                &mut r,
                r#"{"t":"open","c":"nowhere","k":{"slot":"x"}}"#
            ))[0]
                .contains("no such container")
        );
    }
}
