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
use crate::reduce::{self, Item, Op, Reducer};
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
    /// A side question's answer, or why there is none, for the conversation it was asked in.
    Side {
        epoch: u64,
        answer: Result<String, String>,
    },
    /// A home action's outcome: whether it was done, and the words for the status line.
    HomeDone {
        ok: bool,
        said: String,
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
    /// `op` for `host`'s agent, as the hub's menu sends it (`hub::job`): offloading or
    /// archiving a session from the phone's home. Its outcome comes back as `HomeDone`.
    Home { host: String, op: Value },
    /// Run `claude-sessions <args>` in `host` with `input` on its stdin, its one line of
    /// answer to come back as `Side { epoch }`: `/btw`, beside the conversation's own command.
    Side {
        epoch: u64,
        host: String,
        args: Vec<String>,
        input: String,
    },
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
    /// A `/rc` sent to claude: its id and whether it turns Remote Control on; and whether it
    /// is on, as claude last said (`bridge_state`, or the answer to one).
    rc_req: Option<(String, bool)>,
    rc_on: bool,
    /// `get_task_output` requests sent for the client's tasks view: request id to task id.
    output_reqs: HashMap<String, String>,
    /// What the client sent before the conversation was open (`action`), in its order.
    held: Vec<(String, Value)>,
    pending: Vec<Op>,
    pending_n: u64,
    /// Each subagent's own conversation, by the tool-use id of its call, kept as `shown` is:
    /// for Claude Code's view of one agent, which the client opens with `fg`.
    agents: HashMap<String, Shown>,
    /// And whether each is working, which is no item's.
    agent_states: HashMap<String, Op>,
    /// The agent the client views, by that id and its task id, and its ops not yet sent.
    fg: Option<(String, String)>,
    fg_pending: Vec<Op>,
    pending_since: Option<Instant>,
    /// The client's width and rows, when it loads lazily: it is sent the last few screens of
    /// the conversation, and older ones as it scrolls back (`docs/remote-view.md` § *Lazy
    /// history*). `None` for a client that wants everything at once.
    size: Option<(usize, usize)>,
    /// Every item so far, each with its ops, oldest first; the client has `shown.items[from..]`.
    shown: Shown,
    from: usize,
    /// The newest state and meta ops, for a client sent only the end of the conversation.
    state_op: Option<Op>,
    meta_op: Option<Op>,
    /// The background task list as last sent, for a client that connects.
    tasks_op: Option<Op>,
    /// Whether the end of the log as it stood has been sent.
    started: bool,
    /// The history the holder serves backwards, before the log.
    history: History,
}

/// A resumed conversation's history as the holder serves it (`holder::older`).
#[derive(Default)]
struct History {
    /// The holder has some to serve.
    can: bool,
    /// Where the next stretch ends, `None` for the end of what the holder started with.
    before: Option<u64>,
    /// All of it is in.
    done: bool,
    /// A stretch is asked for and not yet in; the client waits on it.
    asking: bool,
    waiting: bool,
    /// The number the next stretch's items start at: well above any the log will reach.
    next: u64,
    /// Tool calls' whole results from stretches read, for `full`.
    results: HashMap<u64, String>,
}

/// The first number a stretch of history's items take.
const HISTORY_IDS: u64 = 1 << 40;

/// How many screens the client is sent at once: on opening, and each time it has fewer than
/// its five screens above the view (`client.rs`'s `AHEAD`).
const SCREENS: usize = 5;

/// How many transcript lines to ask the holder for at a time (whole turns, so often more).
const HISTORY_LINES: u64 = 40;

/// Items and the ops that made each, kept so a client can be sent any stretch of them later.
/// A text item's streamed pieces are folded into its `Add`, and a result or progress op
/// replaces the one before it, so what is kept is the item as it now stands.
#[derive(Default)]
struct Shown {
    items: Vec<(u64, Vec<Op>)>,
    at: HashMap<u64, usize>,
}

fn op_id(op: &Op) -> Option<u64> {
    match op {
        Op::Add { id, .. }
        | Op::Append { id, .. }
        | Op::SetText { id, .. }
        | Op::Result { id, .. }
        | Op::Resolved { id, .. }
        | Op::Progress { id, .. } => Some(*id),
        Op::State(_) | Op::Meta(_) | Op::Back { .. } | Op::Tasks(_) => None,
    }
}

impl Shown {
    /// Keep `op`; where its item is, `None` for an op of no item.
    fn record(&mut self, op: &Op) -> Option<usize> {
        let id = op_id(op)?;
        if let Op::Add { .. } = op {
            self.items.push((id, vec![op.clone()]));
            self.at.insert(id, self.items.len() - 1);
            return Some(self.items.len() - 1);
        }
        let i = *self.at.get(&id)?;
        let ops = &mut self.items[i].1;
        match op {
            Op::Append { text, .. } => match ops.first_mut() {
                Some(Op::Add {
                    item: Item::Text(t),
                    ..
                }) => t.push_str(text),
                _ => ops.push(op.clone()),
            },
            Op::SetText { text, .. } => match ops.first_mut() {
                Some(Op::Add {
                    item: Item::Text(t),
                    ..
                }) => *t = text.clone(),
                _ => ops.push(op.clone()),
            },
            Op::Result { .. } => {
                ops.retain(|o| !matches!(o, Op::Result { .. }));
                ops.push(op.clone());
            }
            Op::Progress { .. } => {
                ops.retain(|o| !matches!(o, Op::Progress { .. }));
                ops.push(op.clone());
            }
            _ => ops.push(op.clone()),
        }
        Some(i)
    }

    /// Older items, in front of these.
    fn prepend(&mut self, older: Shown) {
        let mut items = older.items;
        items.append(&mut self.items);
        self.items = items;
        self.at = self
            .items
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();
    }

    /// Where a stretch ending before `end` starts, to fill about `rows` rows at `w` columns.
    fn back_from(&self, end: usize, rows: usize, w: usize) -> usize {
        let mut from = end;
        let mut got = 0;
        while from > 0 && got < rows {
            from -= 1;
            got += item_rows(&self.items[from].1, w);
        }
        from
    }

    fn ops(&self, range: std::ops::Range<usize>) -> Vec<Op> {
        self.items[range]
            .iter()
            .flat_map(|(_, ops)| ops.iter().cloned())
            .collect()
    }
}

/// About how many rows an item takes at `w` columns: enough to load by, not to draw by.
fn item_rows(ops: &[Op], w: usize) -> usize {
    let room = w.saturating_sub(4).max(10);
    let text_rows = |t: &str| -> usize {
        t.split('\n')
            .map(|l| l.chars().count().div_ceil(room).max(1))
            .sum::<usize>()
    };
    let mut rows = match ops.first() {
        Some(Op::Add { item, .. }) => match item {
            Item::Prompt(t) | Item::Text(t) | Item::Note(t) | Item::Notice(t) => 1 + text_rows(t),
            Item::Tool { .. } => 1,
            Item::Agent { .. } => 3,
            Item::Permission { .. } | Item::Ask { .. } => 4,
            Item::Compacted { .. } => 2,
            // Collapsed, as it is drawn until Ctrl+O.
            Item::Message(_) => 2,
        },
        _ => 1,
    };
    for op in &ops[1..] {
        rows += match op {
            Op::Result { preview, .. } => preview.len().min(3),
            Op::Progress { lines, .. } => lines.len(),
            _ => 0,
        };
    }
    rows
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
            Input::HomeDone { ok, said } => out.push(Effect::Send(msg(
                "did",
                vec![("ok", Value::Bool(ok)), ("x", Value::string(said))],
            ))),
            // Everything the client sends is answered at once: it takes a link that says nothing
            // for 5 seconds after a send for dead, and a side question, a rename or an offload
            // can take longer than that to say anything of its own.
            Input::Client(v) => {
                self.client(&v, &mut out);
                if !out.iter().any(|e| matches!(e, Effect::Send(_))) {
                    out.insert(0, Effect::Send(msg("pong", vec![])));
                }
            }
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
            // Only to the conversation it was asked in: one left meanwhile has no one to tell.
            Input::Side { epoch, answer } => {
                if self.conv.as_ref().is_some_and(|c| c.epoch == epoch) {
                    let field = match answer {
                        Ok(a) => ("x", Value::string(a)),
                        Err(e) => ("e", Value::string(e)),
                    };
                    out.push(Effect::Send(msg("btw", vec![field])));
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
            // The home's `c`: what the hub's menu does with it, through the same agent — its
            // `offload`, and `close`, which archives (`run::plan`).
            "offload" | "archive" => {
                let host = s(v, "c").unwrap_or_default();
                let key = v.get("k").and_then(proto::key_from);
                match key {
                    Some(key) if self.hosts.contains(&host) => {
                        let what = if t == "offload" { "offload" } else { "close" };
                        let mut op = Value::obj();
                        op.set("op", Value::string(what));
                        op.set("key", proto::key_to(&key));
                        out.push(Effect::Home { host, op });
                    }
                    _ => out.push(err("no such session")),
                }
            }
            // A probe after a long silence (`docs/remote-view.md` § *Decisions*): any answer
            // says the link is alive.
            "ping" => out.push(Effect::Send(msg("pong", vec![]))),
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
            rc_req: None,
            output_reqs: HashMap::new(),
            rc_on: false,
            held: Vec::new(),
            pending: Vec::new(),
            agents: HashMap::new(),
            agent_states: HashMap::new(),
            fg: None,
            fg_pending: Vec::new(),
            pending_n: 0,
            pending_since: None,
            size: match (
                v.get("w").and_then(Value::as_u64),
                v.get("r").and_then(Value::as_u64),
            ) {
                (Some(w), Some(r)) if w > 0 && r > 0 => Some((w as usize, r as usize)),
                _ => None,
            },
            shown: Shown::default(),
            from: 0,
            state_op: None,
            meta_op: None,
            tasks_op: None,
            started: false,
            history: History {
                next: HISTORY_IDS,
                ..History::default()
            },
        });
        // Answered at once: starting a holder and reading a long history into it takes
        // seconds, and a client that hears nothing for 5 thinks the link dead.
        out.push(Effect::Send(msg("opening", vec![])));
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
            c.history.can = v.get("older").and_then(Value::as_bool) == Some(true);
            match c.had.take() {
                // A lazy client is sent the end again, whatever it had: what it held of the
                // middle is not known here.
                Some(_) if c.size.is_some() => out.push(Effect::Send(msg("reset", vec![]))),
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
            // Nothing logged yet: the end of the log is here already.
            if c.size.is_some() && c.live_from <= 1 {
                start(c, out);
            }
            let held = std::mem::take(&mut c.held);
            for (t, v) in held {
                self.action(&t, &v, out);
            }
            return;
        }
        let Some((n, body)) = line.split_once('\t') else {
            return;
        };
        if n == "H" {
            if let Ok(v) = json::parse(body.trim()) {
                match v.get("type").and_then(Value::as_str) {
                    // A workflow saved, or why not (`holder::save_workflow`).
                    Some("_saved") => {
                        let mut m = msg("wsaved", vec![]);
                        for k in ["path", "error"] {
                            if let Some(x) = s(&v, k) {
                                m.set(if k == "path" { "x" } else { "e" }, Value::string(x));
                            }
                        }
                        out.push(Effect::Send(m));
                    }
                    _ => history_in(c, &v, out),
                }
            }
            return;
        }
        let Ok(n) = n.parse::<u64>() else { return };
        let Ok(v) = json::parse(body.trim()) else {
            return;
        };
        let live = n >= c.live_from;
        let mut fed = c.reducer.feed(&v);
        for (tool, op) in std::mem::take(&mut fed.sub) {
            c.agents.entry(tool.clone()).or_default().record(&op);
            if let Op::State(_) = op {
                c.agent_states.insert(tool.clone(), op.clone());
            }
            if c.started && c.fg.as_ref().is_some_and(|(t, _)| *t == tool) {
                c.fg_pending.push(op);
                c.pending_since.get_or_insert(now);
            }
        }
        if live {
            for line in fed.to_claude {
                out.push(Effect::ToConv { epoch, line });
            }
        }
        // A task's output, for the tasks view: what it said so far, or why there is none.
        let mut for_output = false;
        if let Msg::Response {
            request_id,
            ok,
            body,
        } = sdk::classify(&v)
        {
            if let Some(task) = c.output_reqs.remove(&request_id) {
                let mut m = msg("tout", vec![("i", Value::string(task))]);
                if ok {
                    let all = s(&body, "output").unwrap_or_default();
                    m.set("x", Value::string(output_tail(&all)));
                    if let Some(n) = body.get("total_bytes").and_then(Value::as_u64) {
                        m.set("n", Value::num(n as f64));
                    }
                    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
                        m.set("tr", Value::Bool(true));
                    }
                } else {
                    let why = body.as_str().unwrap_or("claude has no output for it");
                    m.set("e", Value::string(why));
                }
                out.push(Effect::Send(m));
                for_output = true;
            }
        }
        // claude refusing something this relay asked of it — a rename, a model — is said to the
        // client, which would otherwise see nothing happen.
        if live && !for_output {
            if let Msg::Response {
                request_id,
                ok: false,
                body,
            } = sdk::classify(&v)
            {
                if request_id.starts_with(&format!("rv{}-", std::process::id())) {
                    let why = body.as_str().unwrap_or("claude refused it").to_string();
                    out.push(err(&why));
                }
            }
        }
        if v.get("type").and_then(Value::as_str) == Some("system")
            && v.get("subtype").and_then(Value::as_str) == Some("bridge_state")
        {
            c.rc_on = !matches!(
                v.get("state").and_then(Value::as_str),
                Some("disconnected" | "off" | "failed" | "error")
            );
        }
        if let Some((rc_id, on)) = c.rc_req.clone() {
            if let Msg::Response {
                request_id,
                ok,
                body,
            } = sdk::classify(&v)
            {
                if request_id == rc_id {
                    c.rc_req = None;
                    if ok {
                        c.rc_on = on;
                        let mut m = msg("rc", vec![("on", Value::Bool(on))]);
                        if let Some(url) = s(&body, "session_url") {
                            m.set("x", Value::string(url));
                        }
                        out.push(Effect::Send(m));
                    }
                }
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
        if c.size.is_some() {
            // Lazy: everything is kept; the client is sent the end of the log as it stood once
            // it is in, and from then on what changes in what it has.
            for op in fed.ops {
                let at = c.shown.record(&op);
                match &op {
                    Op::State(_) => c.state_op = Some(op.clone()),
                    Op::Meta(_) => c.meta_op = Some(op.clone()),
                    Op::Tasks(_) => c.tasks_op = Some(op.clone()),
                    _ => {}
                }
                if c.started && at.is_none_or(|i| i >= c.from) {
                    c.pending.push(op);
                }
            }
            c.pending_n = n;
            if !c.started {
                if n + 1 >= c.live_from {
                    start(c, out);
                }
                return;
            }
        } else if n > c.skip_to {
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
        // Still opening: held, and done the moment it is open, rather than refused. A refusal
        // here once lost every prompt typed while a long conversation opened.
        if c.run.is_none() {
            c.held.push((t.to_string(), v.clone()));
            return;
        }
        // Scrolled near the top of what it has: the stretch before it.
        if t == "older" {
            older(c, out);
            return;
        }
        let epoch = c.epoch;
        let mut to = |line: Value| out.push(Effect::ToConv { epoch, line });
        match t {
            // With the phone's uuid for it where it sent one, so Up can withdraw it by that.
            "say" => {
                let uuid = s(v, "u")
                    .filter(|u| u.len() == 36)
                    .unwrap_or_else(crate::launch::fresh_session_id);
                to(sdk::prompt(&s(v, "x").unwrap_or_default(), &uuid))
            }
            // Up on the phone: prompts waiting behind the turn leave claude's queue.
            "unqueue" => {
                let uuids = v.get("u").and_then(Value::as_arr).unwrap_or(&[]);
                for (n, u) in uuids.iter().filter_map(Value::as_str).enumerate() {
                    to(sdk::cancel_async_message(&format!("{id}-{n}"), u));
                }
            }
            // Ctrl+X Ctrl+S: the prompts waiting behind the turn go now, as Claude Code's go.
            // A tool call running goes to the background and the turn takes them in (2.1.311's
            // terminal, read 2026-11-02; headless claude's own `send_now` would stop the turn
            // and its call instead). With none running, `send_now` drops the turn quietly, as
            // the terminal does.
            "now" if c.reducer.tool_running() => to(sdk::background_tasks(&id)),
            "now" => to(sdk::interrupt_send_now(&id)),
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
            // A side question goes to its own claude, not this one (`launch::remote_btw`).
            "btw" => {
                let Some(slot) = c.slot.clone() else {
                    out.push(err("the conversation is still opening"));
                    return;
                };
                let mut args = vec!["remote".to_string(), "btw".to_string(), slot];
                let model = c.reducer.meta().model.clone();
                if !model.is_empty() {
                    args.push(model);
                }
                out.push(Effect::Side {
                    epoch,
                    host: c.host.clone(),
                    args,
                    input: s(v, "x").unwrap_or_default(),
                });
            }
            // `/rc`: on, or off when it is on, as Claude Code's toggles.
            "rc" => {
                let on = !c.rc_on;
                c.rc_req = Some((id.clone(), on));
                let title = c.reducer.meta().title.clone();
                to(sdk::remote_control(&id, on, &title));
            }
            // The tasks view: a shell's or monitor's output so far, and `x` to stop one.
            "tout" => {
                let task = s(v, "i").unwrap_or_default();
                // Only so many outstanding: a client that asks and asks must not grow this.
                if c.output_reqs.len() >= 16 {
                    c.output_reqs.clear();
                }
                c.output_reqs.insert(id.clone(), task.clone());
                to(sdk::get_task_output(&id, &task));
            }
            "tstop" => to(sdk::stop_task(&id, &s(v, "i").unwrap_or_default())),
            // Esc in the view of one agent: that agent interrupted, as Claude Code's does.
            "tesc" => to(sdk::interrupt_task(&id, &s(v, "i").unwrap_or_default())),
            // Claude Code's view of one agent: its conversation so far, then as it goes; `fg`
            // without `i` leaves it.
            "fg" => {
                c.fg_pending.clear();
                let Some(task) = s(v, "i") else {
                    c.fg = None;
                    return;
                };
                let tool = c.reducer.tool_of_task(&task).map(str::to_string);
                let ops = tool
                    .as_ref()
                    .and_then(|t| c.agents.get(t))
                    .map(|shown| {
                        let from = shown.items.len().saturating_sub(AGENT_ITEMS);
                        reduce::coalesce(shown.ops(from..shown.items.len()))
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .chain(tool.as_ref().and_then(|t| c.agent_states.get(t)).cloned())
                    .collect::<Vec<Op>>();
                c.fg = tool.map(|t| (t, task.clone()));
                out.push(Effect::Send(msg(
                    "fgo",
                    vec![
                        ("i", Value::string(task)),
                        ("reset", Value::Bool(true)),
                        ("o", Value::Arr(ops.iter().map(Op::to_value).collect())),
                    ],
                )));
            }
            // Claude Code's workflow browser: its phases and agents, one agent's previews.
            "wf" => {
                let task = s(v, "i").unwrap_or_default();
                let agent = v.get("a").and_then(Value::as_u64).map(|a| a as usize);
                match c.reducer.workflow_view(&task, agent) {
                    Some(mut w) => {
                        w.set("t", Value::string("wf"));
                        w.set("i", Value::string(task));
                        out.push(Effect::Send(w));
                    }
                    None => out.push(err("no such workflow")),
                }
            }
            // `s save` in the browser: the holder writes the script where Claude Code would.
            "wsave" => {
                let task = s(v, "i").unwrap_or_default();
                let Some(script) = c.reducer.workflow_script(&task) else {
                    out.push(err("that workflow has no script to save"));
                    return;
                };
                let mut line = Value::obj();
                line.set("type", Value::string("_save_workflow"));
                line.set("name", Value::string(s(v, "n").unwrap_or_default()));
                line.set(
                    "scope",
                    Value::string(s(v, "sc").unwrap_or_else(|| "project".into())),
                );
                line.set("script", Value::string(script));
                to(line);
            }
            "esc" => to(sdk::interrupt(&id)),
            "exit" => to(sdk::end_session(&id)),
            "models" => {
                c.models_req = Some(id.clone());
                to(sdk::initialize(&id));
            }
            "full" => {
                let item = v.get("id").and_then(Value::as_u64).unwrap_or(0);
                let text = c
                    .reducer
                    .full_result(item)
                    .or_else(|| c.history.results.get(&item).map(String::as_str))
                    .unwrap_or("")
                    .to_string();
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
/// The end of the log as it stood is in: a lazy client is sent its last few screens, then
/// whether there is more before them.
fn start(c: &mut Conv, out: &mut Vec<Effect>) {
    let Some((w, rows)) = c.size else { return };
    c.started = true;
    c.from = c.shown.back_from(c.shown.items.len(), rows * SCREENS, w);
    let mut ops: Vec<Op> = c.meta_op.iter().cloned().collect();
    ops.extend(c.shown.ops(c.from..c.shown.items.len()));
    ops.extend(c.state_op.iter().cloned());
    ops.extend(c.tasks_op.iter().cloned());
    c.pending = ops;
    flush(c, out);
    // `flush` says nothing for nothing; the client still needs to hear the log is in.
    if c.shown.items.is_empty() && c.meta_op.is_none() && c.state_op.is_none() {
        out.push(Effect::Send(msg(
            "n",
            vec![("n", Value::num(c.pending_n as f64))],
        )));
    }
    out.push(more(c));
    // Prompts sent, by this client before it reconnected or by another, that claude has not
    // echoed: a queued one is echoed only when the turn takes it in, and until then the
    // client showed the turn working and nothing of the prompt (owner, 2026-11-02).
    let waiting: Vec<Value> = c
        .reducer
        .waiting_prompts()
        .into_iter()
        .map(|(u, x, q)| {
            let mut p = Value::obj();
            p.set("u", Value::string(u));
            p.set("x", Value::string(x));
            p.set("q", Value::Bool(q));
            p
        })
        .collect();
    if !waiting.is_empty() {
        out.push(Effect::Send(msg("wait", vec![("p", Value::Arr(waiting))])));
    }
}

/// How many of an agent's items the view of it is sent when it opens: the last of them.
const AGENT_ITEMS: usize = 200;

/// How much of a task's output reaches the client: the view shows its last nine lines, and
/// claude answers with all of it, which for a long-running shell is megabytes.
const OUTPUT_TAIL: usize = 4096;

/// The end of a task's output, from a line's start, at most `OUTPUT_TAIL` bytes.
fn output_tail(all: &str) -> String {
    if all.len() <= OUTPUT_TAIL {
        return all.to_string();
    }
    let mut from = all.len() - OUTPUT_TAIL;
    while !all.is_char_boundary(from) {
        from += 1;
    }
    let tail = &all[from..];
    match tail.find('\n') {
        Some(nl) if nl + 1 < tail.len() => tail[nl + 1..].to_string(),
        _ => tail.to_string(),
    }
}

/// Whether there is anything before what the client has.
fn more(c: &Conv) -> Effect {
    let m = c.from > 0 || (c.history.can && !c.history.done);
    Effect::Send(msg("more", vec![("m", Value::Bool(m))]))
}

/// The client wants the stretch before what it has: from what is kept, or asked of the holder.
fn older(c: &mut Conv, out: &mut Vec<Effect>) {
    let Some((w, rows)) = c.size else { return };
    if c.from > 0 {
        let from = c.shown.back_from(c.from, rows * SCREENS, w);
        let ops = reduce::coalesce(c.shown.ops(from..c.from));
        c.from = from;
        out.push(Effect::Send(msg(
            "old",
            vec![("o", Value::Arr(ops.iter().map(Op::to_value).collect()))],
        )));
        out.push(more(c));
    } else if c.history.can && !c.history.done {
        c.history.waiting = true;
        if !c.history.asking {
            c.history.asking = true;
            let mut ask = Value::obj();
            ask.set("type", Value::string("_older"));
            if let Some(b) = c.history.before {
                ask.set("before", Value::num(b as f64));
            }
            ask.set("want", Value::num(HISTORY_LINES as f64));
            out.push(Effect::ToConv {
                epoch: c.epoch,
                line: ask,
            });
        }
    } else {
        out.push(more(c));
    }
}

/// A stretch of history from the holder: reduced on its own, put before everything kept, and
/// sent on if the client waits for it.
fn history_in(c: &mut Conv, v: &Value, out: &mut Vec<Effect>) {
    c.history.asking = false;
    let mut r = Reducer::numbered_from(c.history.next);
    let mut stretch = Shown::default();
    let lines = v.get("lines").and_then(Value::as_arr).unwrap_or(&[]);
    for l in lines {
        let mut h = Value::obj();
        h.set("type", Value::string("_history"));
        h.set("line", l.clone());
        for op in r.feed(&h).ops {
            stretch.record(&op);
        }
    }
    for op in r
        .feed(&json::parse(r#"{"type":"_history_end"}"#).unwrap_or(Value::Null))
        .ops
    {
        stretch.record(&op);
    }
    c.history.next = r.next_number();
    c.history.results.extend(r.take_results());
    c.history.before = v.get("before").and_then(Value::as_u64).or(c.history.before);
    c.history.done = v.get("more").and_then(Value::as_bool) != Some(true);
    let got = stretch.items.len();
    c.shown.prepend(stretch);
    c.from += got;
    // Sent on to a client waiting for it; a stretch with nothing to draw asks for the one
    // before it.
    if std::mem::take(&mut c.history.waiting) {
        older(c, out);
    }
}

fn flush(c: &mut Conv, out: &mut Vec<Effect>) {
    c.pending_since = None;
    if !c.fg_pending.is_empty() {
        if let Some((_, task)) = &c.fg {
            let ops = reduce::coalesce(std::mem::take(&mut c.fg_pending));
            out.push(Effect::Send(msg(
                "fgo",
                vec![
                    ("i", Value::string(task)),
                    ("o", Value::Arr(ops.iter().map(Op::to_value).collect())),
                ],
            )));
        }
        c.fg_pending.clear();
    }
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
    // The slash commands too, for the prompt's suggestion list: name, description, argument
    // hint. An older client reads `m` alone.
    let commands = body
        .get("commands")
        .and_then(Value::as_arr)
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            Value::Arr(
                ["name", "description", "argumentHint"]
                    .iter()
                    .map(|k| Value::string(s(c, k).unwrap_or_default()))
                    .collect(),
            )
        })
        .collect();
    msg(
        "models",
        vec![("m", Value::Arr(list)), ("c", Value::Arr(commands))],
    )
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
                Effect::Side {
                    epoch,
                    host,
                    args,
                    input,
                } => side(epoch, &host, &args, input, &tx),
                Effect::Home { host, op } => match conns.get(&host) {
                    Some(conn) => home_job(conn.clone(), op, &tx),
                    None => {
                        let _ = tx.send(Input::HomeDone {
                            ok: false,
                            said: format!("{host} is not one of this hub's containers"),
                        });
                    }
                },
            }
        }
        if stdout.flush().is_err() {
            break;
        }
    }
    finish(running)
}

/// A home action, on a thread of its own so the relay keeps serving meanwhile. Offloading and
/// archiving need no terminal; were the agent to ask for one, the phone has none to lend.
fn home_job(conn: Arc<hub::Conn>, op: Value, tx: &Sender<Input>) {
    struct NoTerminal;
    impl crate::ui::Terminal for NoTerminal {
        fn suspend(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn run(&mut self, _: &mut std::process::Command) -> io::Result<std::process::ExitStatus> {
            Err(io::Error::other("the phone has no terminal to lend"))
        }
    }
    let tx = tx.clone();
    std::thread::spawn(move || {
        let what = op
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let outcome = hub::job(conn, op)(&mut NoTerminal);
        let _ = tx.send(home_done(&what, outcome));
    });
}

/// What the phone's status line says of a home action's outcome.
fn home_done(op: &str, outcome: crate::ui::Outcome) -> Input {
    use crate::ui::Outcome;
    let done = if op == "offload" {
        "offloaded"
    } else {
        "archived"
    };
    match outcome {
        Outcome::Back(said) => Input::HomeDone {
            ok: true,
            said: said.unwrap_or_else(|| done.to_string()),
        },
        Outcome::Refused(why) => Input::HomeDone {
            ok: false,
            said: why,
        },
        Outcome::NoRoom(_) | Outcome::ResumeFailed(_) => Input::HomeDone {
            ok: false,
            said: format!("not {done}"),
        },
    }
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

/// Run a side question's command on its own thread: the question on stdin, then its one
/// JSON line read back, `{"answer"}` or `{"error"}`.
fn side(epoch: u64, host: &str, args: &[String], input: String, tx: &Sender<Input>) {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = hub::command(host, &argv, false);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let tx = tx.clone();
    std::thread::spawn(move || {
        let answer = match cmd.spawn() {
            Err(e) => Err(format!("could not run ssh: {e}")),
            Ok(mut child) => {
                if let Some(mut pipe) = child.stdin.take() {
                    let _ = pipe.write_all(input.as_bytes());
                }
                match child.wait_with_output() {
                    Err(e) => Err(e.to_string()),
                    Ok(o) => {
                        let text = String::from_utf8_lossy(&o.stdout);
                        let v = text
                            .lines()
                            .rev()
                            .find_map(|l| json::parse(l.trim()).ok())
                            .unwrap_or(Value::Null);
                        match (s(&v, "answer"), s(&v, "error")) {
                            (Some(a), _) => Ok(a),
                            (None, Some(e)) => Err(e),
                            (None, None) => Err(String::from_utf8_lossy(&o.stderr)
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("the container did not answer")
                                .trim()
                                .to_string()),
                        }
                    }
                }
            }
        };
        let _ = tx.send(Input::Side { epoch, answer });
    });
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

    #[test]
    fn send_now_backgrounds_a_running_call_and_otherwise_drops_the_turn_as_claude_code_does() {
        let mut r = relay();
        let (epoch, _) = opened(&mut r, 1);
        // Nothing running: claude's send_now, which drops the turn quietly.
        let fx = client(&mut r, r#"{"t":"now"}"#);
        let said = to_claude(&fx);
        assert!(said[0].contains(r#""send_now":true"#), "{said:?}");
        // A call running: Ctrl+B's background_tasks, so the turn takes the prompt in.
        line(
            &mut r,
            epoch,
            1,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 20"}}]}}"#,
        );
        let fx = client(&mut r, r#"{"t":"now"}"#);
        let said = to_claude(&fx);
        assert!(
            said[0].contains(r#""subtype":"background_tasks""#),
            "{said:?}"
        );
        assert!(!said[0].contains("send_now"));
    }

    #[test]
    fn what_is_sent_while_a_conversation_opens_waits_for_it_and_a_refusal_is_said() {
        let mut r = relay();
        let fx = client(
            &mut r,
            r#"{"t":"open","c":"claude-sessions","k":{"slot":"claude-3"}}"#,
        );
        // Answered at once, so a slow open does not look like a dead link.
        assert_eq!(sent(&fx), [r#"{"t":"opening"}"#]);
        let Some(Effect::Open { epoch, .. }) = fx.into_iter().last() else {
            panic!("no open");
        };
        // Typed before the holder answers: held, not refused.
        let fx = client(
            &mut r,
            r#"{"t":"say","x":"held","u":"11111111-2222-4333-8444-555555555555"}"#,
        );
        assert_eq!(
            sent(&fx),
            [r#"{"t":"pong"}"#],
            "only answered, so the link holds"
        );
        let fx = client(&mut r, r#"{"t":"rename","x":"held title"}"#);
        assert_eq!(sent(&fx), [r#"{"t":"pong"}"#]);
        r.handle(
            Input::Conv {
                epoch,
                line: r#"{"slot":"claude-3"}"#.into(),
            },
            Instant::now(),
        );
        let fx = r.handle(
            Input::Conv {
                epoch,
                line: r#"{"next":1,"run":"run-1"}"#.into(),
            },
            Instant::now(),
        );
        let said = to_claude(&fx);
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(
            said[0].contains(r#""content":"held""#),
            "in order: {said:?}"
        );
        // The rename before claude has said its session: no empty session_id to refuse.
        assert!(said[1].contains(r#""subtype":"rename_session""#));
        assert!(!said[1].contains("session_id"), "{said:?}");
        // claude refusing something the relay asked: the client is told why.
        let id = format!("rv{}-9", std::process::id());
        let fx = line(
            &mut r,
            epoch,
            1,
            &format!(
                r#"{{"type":"control_response","response":{{"subtype":"error","request_id":"{id}","error":"session_id is not the current session"}}}}"#
            ),
        );
        assert!(
            sent(&fx)
                .iter()
                .any(|l| l.contains("session_id is not the current session")),
            "{:?}",
            sent(&fx)
        );
    }

    #[test]
    fn up_withdraws_the_phones_queued_prompts_by_the_uuids_it_sent_them_with() {
        let mut r = relay();
        opened(&mut r, 1);
        let uuid = "11111111-2222-4333-8444-555555555555";
        let fx = client(
            &mut r,
            &format!(r#"{{"t":"say","x":"lakes","u":"{uuid}"}}"#),
        );
        assert!(
            to_claude(&fx)[0].contains(&format!(r#""uuid":"{uuid}""#)),
            "{fx:?}"
        );
        let fx = client(&mut r, &format!(r#"{{"t":"unqueue","u":["{uuid}"]}}"#));
        let lines = to_claude(&fx);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains(r#""subtype":"cancel_async_message""#),
            "{lines:?}"
        );
        assert!(lines[0].contains(&format!(r#""message_uuid":"{uuid}""#)));
    }

    #[test]
    fn the_homes_c_goes_to_the_containers_agent_as_the_doors_does() {
        let mut r = relay();
        let fx = client(
            &mut r,
            r#"{"t":"offload","c":"or3-dev","k":{"slot":"claude-3"}}"#,
        );
        match fx.as_slice() {
            [Effect::Send(_), Effect::Home { host, op }] => {
                assert_eq!(host, "or3-dev");
                assert_eq!(
                    json::to_line(op),
                    r#"{"key":{"slot":"claude-3"},"op":"offload"}"#
                );
            }
            other => panic!("{other:?}"),
        }
        // Archiving is the agent's `close`, as `run::plan` sends it.
        let fx = client(
            &mut r,
            r#"{"t":"archive","c":"claude-sessions","k":{"slot":"claude-4"}}"#,
        );
        assert!(
            matches!(fx.as_slice(), [Effect::Send(_), Effect::Home { op, .. }] if op.get("op").and_then(Value::as_str) == Some("close")),
            "{fx:?}"
        );
        // A container the hub does not have is refused, not sent anywhere.
        let fx = client(
            &mut r,
            r#"{"t":"offload","c":"elsewhere","k":{"slot":"claude-1"}}"#,
        );
        assert!(matches!(fx.as_slice(), [Effect::Send(_)]), "{fx:?}");
        // Its outcome, for the status line.
        let fx = r.handle(
            home_done("offload", crate::ui::Outcome::Back(None)),
            Instant::now(),
        );
        assert_eq!(
            fx,
            [Effect::Send(
                parse(r#"{"t":"did","ok":true,"x":"offloaded"}"#).unwrap()
            )]
        );
        let fx = r.handle(
            home_done("close", crate::ui::Outcome::Refused("busy".into())),
            Instant::now(),
        );
        assert_eq!(
            fx,
            [Effect::Send(
                parse(r#"{"t":"did","ok":false,"x":"busy"}"#).unwrap()
            )]
        );
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

    #[test]
    fn a_lazy_client_gets_the_last_screens_then_older_ones_then_the_holders_history() {
        let mut r = relay();
        // 40 columns, 5 rows: five screens is 25 rows, and a two-line reply takes 2.
        let fx = client(
            &mut r,
            r#"{"t":"open","c":"claude-sessions","k":{"slot":"claude-3"},"w":40,"r":5}"#,
        );
        let Some(Effect::Open { epoch, .. }) = fx.into_iter().last() else {
            panic!("no open");
        };
        let conv = |r: &mut Relay, l: &str| {
            r.handle(
                Input::Conv {
                    epoch,
                    line: l.into(),
                },
                Instant::now(),
            )
        };
        conv(&mut r, r#"{"slot":"claude-3"}"#);
        let fx = conv(&mut r, r#"{"next":21,"run":"run-1","older":true}"#);
        assert_eq!(
            sent(&fx)[0],
            r#"{"c":"claude-sessions","run":"run-1","s":"claude-3","t":"open"}"#
        );
        // The log as it stood: twenty replies, nothing sent until the last is in.
        let reply = |n: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{n}","content":[{{"type":"text","text":"reply {n}"}}]}}}}"#
            )
        };
        for n in 1..20 {
            assert!(sent(&line(&mut r, epoch, n, &reply(n))).is_empty());
        }
        let ids = |fx: &[Effect]| -> Vec<u64> {
            let mut ids = Vec::new();
            for m in sent(fx) {
                let v = parse(&m).unwrap();
                let ops = match v.get("o").and_then(Value::as_arr) {
                    Some(o) => o.to_vec(),
                    None => vec![v],
                };
                for o in ops {
                    if o.get("t").and_then(Value::as_str) == Some("add") {
                        ids.push(o.get("id").and_then(Value::as_u64).unwrap());
                    }
                }
            }
            ids
        };
        let fx = line(&mut r, epoch, 20, &reply(20));
        assert_eq!(ids(&fx), (8..=20).collect::<Vec<_>>(), "{:?}", sent(&fx));
        let said = sent(&fx);
        assert_eq!(said[said.len() - 2], r#"{"n":20,"t":"n"}"#);
        assert_eq!(said[said.len() - 1], r#"{"m":true,"t":"more"}"#);
        // Live, a new item goes straight on.
        line(&mut r, epoch, 21, &reply(21));
        let fx = r.tick(Instant::now() + Duration::from_secs(1));
        assert_eq!(ids(&fx), [21]);
        // Scrolled back: the stretch before, from what the relay kept, then the rest of it.
        let fx = client(&mut r, r#"{"t":"older"}"#);
        assert_eq!(ids(&fx), (1..=7).collect::<Vec<_>>());
        assert_eq!(
            sent(&fx).last().unwrap(),
            r#"{"m":true,"t":"more"}"#,
            "the holder has history"
        );
        // Then the holder's history, asked for once however often the client asks.
        let fx = client(&mut r, r#"{"t":"older"}"#);
        assert_eq!(to_claude(&fx), [r#"{"type":"_older","want":40}"#]);
        assert_eq!(sent(&fx), [r#"{"t":"pong"}"#]);
        assert!(to_claude(&client(&mut r, r#"{"t":"older"}"#)).is_empty());
        let fx = conv(
            &mut r,
            r#"H	{"type":"_older","before":0,"more":false,"lines":[{"type":"user","message":{"role":"user","content":"long ago"}},{"type":"assistant","message":{"id":"h1","content":[{"type":"text","text":"an old reply"}]}}]}"#,
        );
        assert_eq!(ids(&fx), [HISTORY_IDS, HISTORY_IDS + 1]);
        let said = sent(&fx);
        assert!(said[0].contains(r#""x":"long ago""#), "{said:?}");
        assert_eq!(said.last().unwrap(), r#"{"m":false,"t":"more"}"#);
        let fx = client(&mut r, r#"{"t":"older"}"#);
        assert_eq!(sent(&fx), [r#"{"m":false,"t":"more"}"#]);
    }

    #[test]
    fn rc_turns_remote_control_on_with_its_link_and_off_again() {
        let mut r = relay();
        let (epoch, _) = opened(&mut r, 1);
        let fx = client(&mut r, r#"{"t":"rc"}"#);
        let said = to_claude(&fx);
        assert!(
            said[0].contains(r#""subtype":"remote_control""#),
            "{said:?}"
        );
        assert!(said[0].contains(r#""enabled":true"#), "{said:?}");
        let id = parse(&said[0])
            .unwrap()
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        // As 2.1.311 answered it on 2026-11-02.
        let fx = line(
            &mut r,
            epoch,
            1,
            &format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{{"session_url":"https://claude.ai/code/session_x","bridge_session_id":"cse_x"}}}}}}"#
            ),
        );
        assert!(
            sent(&fx).contains(
                &r#"{"on":true,"t":"rc","x":"https://claude.ai/code/session_x"}"#.to_string()
            ),
            "{:?}",
            sent(&fx)
        );
        let said = to_claude(&client(&mut r, r#"{"t":"rc"}"#));
        assert!(said[0].contains(r#""enabled":false"#), "{said:?}");
    }

    #[test]
    fn a_client_that_connects_is_told_of_prompts_still_on_their_way() {
        let mut r = relay();
        let fx = client(
            &mut r,
            r#"{"t":"open","c":"claude-sessions","k":{"slot":"claude-3"},"w":40,"r":10}"#,
        );
        let Some(Effect::Open { epoch, .. }) = fx.into_iter().last() else {
            panic!("no open");
        };
        let conv = |r: &mut Relay, l: &str| {
            r.handle(
                Input::Conv {
                    epoch,
                    line: l.into(),
                },
                Instant::now(),
            )
        };
        conv(&mut r, r#"{"slot":"claude-3"}"#);
        conv(&mut r, r#"{"next":3,"run":"run-1"}"#);
        let u = "aaaaaaaa-1111-4111-8111-111111111111";
        line(
            &mut r,
            epoch,
            1,
            &format!(
                r#"{{"type":"_in","line":{}}}"#,
                json::to_line(&sdk::prompt("then this", u))
            ),
        );
        let fx = line(
            &mut r,
            epoch,
            2,
            &format!(r#"{{"type":"command_lifecycle","command_uuid":"{u}","state":"queued"}}"#),
        );
        assert_eq!(
            sent(&fx).last().unwrap(),
            &format!(r#"{{"p":[{{"q":true,"u":"{u}","x":"then this"}}],"t":"wait"}}"#)
        );
    }

    #[test]
    fn a_side_question_runs_beside_the_conversation_on_its_model_and_answers_only_it() {
        let mut r = relay();
        let (epoch, _) = opened(&mut r, 1);
        let init = r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-haiku-5-5","permissionMode":"default"}"#;
        line(&mut r, epoch, 1, init);
        let fx = client(&mut r, r#"{"t":"btw","x":"what was that?"}"#);
        // Answered at once: the side question takes longer than the client waits for a word.
        assert_eq!(sent(&fx), [r#"{"t":"pong"}"#]);
        let Some(Effect::Side {
            epoch: e,
            host,
            args,
            input,
        }) = fx.into_iter().last()
        else {
            panic!("no side question");
        };
        assert_eq!(
            (e, host.as_str(), input.as_str()),
            (epoch, "claude-sessions", "what was that?")
        );
        assert_eq!(args, ["remote", "btw", "claude-3", "claude-haiku-5-5"]);
        let fx = r.handle(
            Input::Side {
                epoch,
                answer: Ok("A greeting.".into()),
            },
            Instant::now(),
        );
        assert_eq!(sent(&fx), [r#"{"t":"btw","x":"A greeting."}"#]);
        let fx = r.handle(
            Input::Side {
                epoch: epoch + 1,
                answer: Err("late".into()),
            },
            Instant::now(),
        );
        assert!(
            sent(&fx).is_empty(),
            "an answer for a conversation left is dropped"
        );
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

        // A prompt goes with a uuid of its own, which claude's lifecycle lines name.
        let fx = client(&mut r, r#"{"t":"say","x":"/compact"}"#);
        let said = to_claude(&fx);
        let line = parse(&said[0]).unwrap();
        assert_eq!(
            json::to_line(line.get("message").unwrap()),
            r#"{"content":"/compact","role":"user"}"#
        );
        assert_eq!(
            line.get("uuid").and_then(Value::as_str).map(str::len),
            Some(36)
        );
        let fx = client(&mut r, r#"{"t":"model","m":"opus"}"#);
        assert!(to_claude(&fx)[0].contains(r#""subtype":"set_model""#));
        let fx = client(&mut r, r#"{"t":"effort","e":null}"#);
        assert!(to_claude(&fx)[0].contains(r#""effortLevel":null"#));
        let fx = client(&mut r, r#"{"t":"teleport"}"#);
        assert!(sent(&fx)[0].contains("not something the relay knows"));
    }

    #[test]
    fn the_tasks_view_asks_claude_for_output_and_stops_a_task() {
        let mut r = relay();
        client(&mut r, r#"{"t":"hi","v":1}"#);
        let fx = client(
            &mut r,
            r#"{"t":"open","c":"claude-sessions","k":{"slot":"claude-3"},"w":40,"r":10}"#,
        );
        let Some(Effect::Open { epoch, .. }) = fx.into_iter().last() else {
            panic!("no open");
        };
        let conv = |r: &mut Relay, l: &str| {
            r.handle(
                Input::Conv {
                    epoch,
                    line: l.into(),
                },
                Instant::now(),
            )
        };
        conv(&mut r, r#"{"slot":"claude-3"}"#);
        conv(&mut r, r#"{"next":2,"run":"run-1"}"#);
        // The list as it stood reaches a client that connects, with the log's end.
        let now = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b1","task_type":"local_bash","description":"Ticks"}]}"#;
        let fx = line(&mut r, epoch, 1, now);
        assert!(
            sent(&fx)
                .iter()
                .any(|l| l.contains(r#""t":"tasks""#) && l.contains(r#""i":"b1""#)),
            "{:?}",
            sent(&fx)
        );
        let fx = client(&mut r, r#"{"t":"tout","i":"b1"}"#);
        let asked = parse(&to_claude(&fx)[0]).unwrap();
        let req = asked.get("request").unwrap();
        assert_eq!(
            json::to_line(req),
            r#"{"subtype":"get_task_output","task_id":"b1"}"#
        );
        let rid = asked
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let answer = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{rid}","response":{{"output":"tick 1\ntick 2\n","total_bytes":14,"truncated":false}}}}}}"#
        );
        let fx = line(&mut r, epoch, 2, &answer);
        assert_eq!(
            sent(&fx),
            [r#"{"i":"b1","n":14,"t":"tout","x":"tick 1\ntick 2\n"}"#]
        );
        // One claude has no output for is said in the view, not as a refusal.
        let fx = client(&mut r, r#"{"t":"tout","i":"a1"}"#);
        let rid = parse(&to_claude(&fx)[0])
            .unwrap()
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let refused = format!(
            r#"{{"type":"control_response","response":{{"subtype":"error","request_id":"{rid}","error":"no shell or Monitor task with that task_id"}}}}"#
        );
        let fx = line(&mut r, epoch, 3, &refused);
        assert_eq!(
            sent(&fx),
            [r#"{"e":"no shell or Monitor task with that task_id","i":"a1","t":"tout"}"#]
        );
        // Only the end of a long output crosses the link.
        let long: String = (0..2000).map(|n| format!("line {n}\n")).collect();
        let tail = output_tail(&long);
        assert!(
            tail.len() <= OUTPUT_TAIL && tail.starts_with("line "),
            "{}",
            &tail[..20]
        );
        assert!(tail.ends_with("line 1999\n"));
        assert_eq!(output_tail("short\n"), "short\n");
        let fx = client(&mut r, r#"{"t":"tstop","i":"b1"}"#);
        assert!(to_claude(&fx)[0].contains(r#""request":{"subtype":"stop_task","task_id":"b1"}"#));
    }

    #[test]
    fn the_view_of_one_agent_gets_its_conversation_then_follows_it() {
        let mut r = relay();
        client(&mut r, r#"{"t":"hi","v":1}"#);
        let fx = client(
            &mut r,
            r#"{"t":"open","c":"claude-sessions","k":{"slot":"claude-3"},"w":40,"r":10}"#,
        );
        let Some(Effect::Open { epoch, .. }) = fx.into_iter().last() else {
            panic!("no open");
        };
        let conv = |r: &mut Relay, l: &str| {
            r.handle(
                Input::Conv {
                    epoch,
                    line: l.into(),
                },
                Instant::now(),
            )
        };
        conv(&mut r, r#"{"slot":"claude-3"}"#);
        conv(&mut r, r#"{"next":3,"run":"run-1"}"#);
        line(
            &mut r,
            epoch,
            1,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_a","name":"Agent","input":{"description":"reader","prompt":"Read it.","run_in_background":true}}]}}"#,
        );
        line(
            &mut r,
            epoch,
            2,
            r#"{"type":"system","subtype":"task_started","task_id":"ag1","tool_use_id":"tu_a","description":"reader","task_type":"local_agent"}"#,
        );
        let fx = client(&mut r, r#"{"t":"fg","i":"ag1"}"#);
        assert_eq!(
            sent(&fx),
            [
                r#"{"i":"ag1","o":[{"id":1,"k":"user","t":"add","x":"Read it."},{"s":"working","t":"st"}],"reset":true,"t":"fgo"}"#
            ]
        );
        // What it does next comes with the next batch, and only while it is viewed.
        line(
            &mut r,
            epoch,
            3,
            r#"{"type":"assistant","parent_tool_use_id":"tu_a","message":{"id":"s1","content":[{"type":"tool_use","id":"tu_r","name":"Read","input":{"file_path":"/x"}}]}}"#,
        );
        let fx = r.tick(Instant::now() + Duration::from_secs(1));
        assert!(
            sent(&fx)
                .iter()
                .any(|l| l.starts_with(r#"{"i":"ag1","o":[{"a":"/x","id":2,"k":"tool""#)),
            "{:?}",
            sent(&fx)
        );
        client(&mut r, r#"{"t":"fg"}"#);
        line(
            &mut r,
            epoch,
            4,
            r#"{"type":"user","parent_tool_use_id":"tu_a","message":{"role":"user","content":[{"tool_use_id":"tu_r","type":"tool_result","content":"ok"}]}}"#,
        );
        let fx = r.tick(Instant::now() + Duration::from_secs(1));
        assert!(
            !sent(&fx).iter().any(|l| l.contains("fgo")),
            "{:?}",
            sent(&fx)
        );
        // Esc there interrupts that agent alone.
        let fx = client(&mut r, r#"{"t":"tesc","i":"ag1"}"#);
        assert!(to_claude(&fx)[0].contains(r#""request":{"subtype":"interrupt","task_id":"ag1"}"#));
        // A save goes to the holder, which answers it under `H`.
        let fx = client(&mut r, r#"{"t":"wsave","i":"ag1","n":"x"}"#);
        assert!(sent(&fx)[0].contains("no script"), "{:?}", sent(&fx));
        let fx = conv(
            &mut r,
            "H\t{\"type\":\"_saved\",\"path\":\"/p/.claude/workflows/x.js\"}\n",
        );
        assert_eq!(
            sent(&fx),
            [r#"{"t":"wsaved","x":"/p/.claude/workflows/x.js"}"#]
        );
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
