//! `claude-remote <ssh name>`: the remote view's client, on the owner's phone (Termux) or in
//! WSL (`docs/remote-view.md`; mockups R1 to R10).
//!
//! **One ssh, kept up.** `ssh -T -C <name> claude-sessions relay`, run again with backoff from
//! 1 to 30 seconds whenever it ends. While it is down the screen stays, scrollback included,
//! typing still works, and an Enter is refused and kept, the footer saying so in words (R8). A
//! link is also taken for dead when something sent gets no answer within five seconds, and a
//! key after two minutes of silence first sends a ping. Back up, it says hello and, with a
//! conversation open, where it got to, so only what it lacks is sent again.
//!
//! **The home is the hub's menu**, drawn by the same code from the same snapshots, with R9's
//! hint line. A session a terminal drives is listed dim, as `term: `, and Enter on it says why
//! it does not open (R10). **The conversation is `convo.rs`'s.**
//!
//! Drawing writes only the rows that changed. Nothing is written while nothing happens, save
//! the spinner while claude works and the seconds while the link is down.

use crate::convo::{self, Applied, Convo, Dialog, Entry, Focus, Link, Ui};
use crate::harness::Harness;
use crate::json::{self, Value};
use crate::menu::{Action, Menu, Place, Reach};
use crate::proto;
use crate::tasks;
use crate::term::{self, Input, RawTerminal};
use crate::ui::{Busy, Header, Key, Row, RowKey};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const ACK_WAIT: Duration = Duration::from_secs(5);
/// How long an open may take before the link is thought dead: starting a holder and reading a
/// long conversation's history into it can take many seconds, and the 5 that any other answer
/// gets cut the link mid-open, which left the conversation opening for ever.
const OPEN_WAIT: Duration = Duration::from_secs(60);
/// How many screens above the view the client keeps loaded ahead of a scroll back.
const AHEAD: usize = 5;
const QUIET_PROBE: Duration = Duration::from_secs(120);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const SPIN_EVERY: Duration = Duration::from_millis(120);
/// What the agent view says to a message typed to the agent.
const AGENT_MESSAGE_REFUSED: &str = "headless claude can't message agents";

/// How often an open shell's details ask for its output again.
const OUTPUT_EVERY: Duration = Duration::from_secs(2);
/// Ctrl+\\, zmx's detach key: out of a conversation to the home, and from the home out.
const DETACH: char = '\x1c';
/// Ctrl+X, then Ctrl+S: Claude Code's send-now chord. Raw mode turns off XON/XOFF, so Ctrl+S
/// arrives as a byte rather than stopping the terminal.
const CTRL_X: char = '\x18';
const CTRL_S: char = '\x13';
/// R10's words, in place of the hint line until the next key.
const TERMINAL_ONLY: &str =
    "Runs in a terminal: attach it from ssh dev, or end it there to open here";

/// What the link's reader thread says.
enum Heard {
    Line(u64, Value),
    Gone(u64, String),
}

struct Wire {
    child: Child,
    stdin: ChildStdin,
}

fn connect(name: &str, epoch: u64, tx: &Sender<Heard>) -> io::Result<Wire> {
    let ssh = std::env::var("CLAUDE_REMOTE_SSH").unwrap_or_else(|_| "ssh".into());
    let relay =
        std::env::var("CLAUDE_REMOTE_RELAY").unwrap_or_else(|_| "claude-sessions relay".into());
    let mut cmd = Command::new(ssh);
    cmd.args(["-T", "-C", "-o", "BatchMode=yes", "--", name])
        .arg(relay)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take();
    let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = stderr {
        let said = said.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                if !l.trim().is_empty() {
                    *said.lock().unwrap_or_else(|p| p.into_inner()) = l.trim().to_string();
                }
            }
        });
    }
    let tx = tx.clone();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = json::parse(l.trim()) {
                if tx.send(Heard::Line(epoch, v)).is_err() {
                    return;
                }
                term::wake();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
        let why = said.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let _ = tx.send(Heard::Gone(epoch, why));
        term::wake();
    });
    Ok(Wire { child, stdin })
}

enum Screen {
    Home,
    /// A conversation: opening until the relay says which slot.
    Conv {
        c: Box<Convo>,
        ui: Box<Ui>,
        opening: bool,
    },
}

struct App {
    name: String,
    wire: Option<Wire>,
    epoch: u64,
    down_since: Option<Instant>,
    retry_at: Option<Instant>,
    backoff: Duration,
    last_heard: Instant,
    awaiting: Option<Instant>,
    hosts: Vec<String>,
    places: HashMap<String, Place>,
    rows: HashMap<String, Vec<Row>>,
    hub: Header,
    menu: Menu,
    menu_fresh: bool,
    screen: Screen,
    models: Option<Vec<(String, String)>>,
    history: Vec<String>,
    history_at: Option<usize>,
    /// The task whose output the tasks view last asked for, and when to ask again: while a
    /// shell's or monitor's details are open, its output keeps coming, as Claude Code's box
    /// keeps scrolling.
    output_for: Option<(String, Instant)>,
    /// What was being typed when Up went back into what was sent: Down past the newest gives
    /// it back, as Claude Code does, so no keypress loses it.
    draft: Option<String>,
    /// What was on each row of the screen, so only what changed is written.
    drawn: Vec<String>,
    /// Bytes the owner should have on the clipboard (OSC 52), written with the next frame.
    clip: Option<String>,
    quit: bool,
    /// When the client started, and whether the relay has said hello since: until it has,
    /// the home is mockup 10's first frame, not an empty container it knows nothing of.
    started: Instant,
    greeted: bool,
    /// Lines a wheel report scrolls (`wheel_step`).
    wheel: usize,
    /// A home action under way: since when, its row, and what the spinner says (mockup 11).
    working: Option<(Instant, usize, String)>,
    /// A bracketed paste arriving: its text so far, until its end (`Key::PasteEnd`).
    pasting: Option<String>,
    /// claude's slash commands with what each does, from `initialize` (the relay's `models`):
    /// asked for when a conversation opens, for its suggestion list.
    catalog: Vec<convo::Command>,
    /// The request that is opening a conversation, until the relay says it is open: sent again
    /// on a new link if the old one drops meanwhile, when that is safe (`dial`).
    open_msg: Option<Value>,
    /// What was sent to a conversation still opening, held until the relay says it is open:
    /// before then the relay can do nothing with it, and a 0.13.5 relay refuses it, which this
    /// client read as the open failing.
    held: Vec<Value>,
    /// A selection dragged over the conversation, in screen cells: where the button went
    /// down, where it is now, and whether it has moved. Highlighted, and copied on release.
    select: Option<Selection>,
}

impl App {
    fn send(&mut self, v: Value) -> bool {
        let Some(w) = self.wire.as_mut() else {
            return false;
        };
        let ok = writeln!(w.stdin, "{}", json::to_line(&v))
            .and_then(|()| w.stdin.flush())
            .is_ok();
        if ok && self.awaiting.is_none() {
            self.awaiting = Some(Instant::now());
        }
        ok
    }

    fn msg(t: &str, rest: Vec<(&str, Value)>) -> Value {
        let mut v = Value::obj();
        v.set("t", Value::string(t));
        for (k, x) in rest {
            v.set(k, x);
        }
        v
    }

    fn up(&self) -> bool {
        self.wire.is_some() && self.down_since.is_none()
    }

    fn link_down(&mut self, now: Instant) {
        if let Some(mut w) = self.wire.take() {
            let _ = w.child.kill();
            let _ = w.child.wait();
        }
        self.awaiting = None;
        // An action under way has no answer coming on this link.
        if self.working.take().is_some() {
            self.menu.set_busy(None);
        }
        if self.down_since.is_none() {
            self.down_since = Some(now);
        }
        self.retry_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        if let Screen::Conv { ui, .. } = &mut self.screen {
            if !matches!(ui.link, Link::Down { .. }) {
                ui.link = Link::Down {
                    since: now,
                    not_sent: false,
                };
            }
            // Its answer went to the relay that just went: the next one never heard it.
            if let Some(Dialog::Btw {
                answer: a @ None, ..
            }) = &mut ui.dialog
            {
                *a = Some(Err(
                    "the link dropped before the answer came: ask again".into()
                ));
            }
        }
    }

    fn dial(&mut self, now: Instant, tx: &Sender<Heard>) {
        self.retry_at = None;
        self.epoch += 1;
        match connect(&self.name, self.epoch, tx) {
            Ok(w) => {
                self.wire = Some(w);
                self.last_heard = now;
                self.send(App::msg("hi", vec![("v", Value::num(1.0))]));
                // Dropped while a conversation opened: the new link's relay knows nothing of it.
                // Opening an existing one again is safe — a second open of a conversation the
                // first started attaches to it — but a new one could start twice, so that goes
                // home, where it is listed if it started.
                if matches!(self.screen, Screen::Conv { opening: true, .. }) {
                    match self.open_msg.clone() {
                        Some(m) if m.get("t").and_then(Value::as_str) == Some("open") => {
                            self.send(m);
                        }
                        _ => {
                            self.open_msg = None;
                            self.held.clear();
                            self.home(Some(
                                "the link dropped while it started: it is listed if it did".into(),
                            ));
                        }
                    }
                    return;
                }
                if let Screen::Conv { c, opening, .. } = &self.screen {
                    if !*opening && !c.slot.is_empty() {
                        let re = App::msg(
                            "re",
                            vec![
                                ("c", Value::string(&c.host)),
                                ("s", Value::string(&c.slot)),
                                ("run", Value::string(&c.run)),
                                ("n", Value::num(c.n as f64)),
                                ("w", Value::num(size().0 as f64)),
                                ("r", Value::num(size().1 as f64)),
                            ],
                        );
                        // The agent in view, which the new link's relay does not know of.
                        let fg = c
                            .viewing
                            .as_ref()
                            .map(|(id, _)| App::msg("fg", vec![("i", Value::string(id))]));
                        self.send(re);
                        if let Some(fg) = fg {
                            self.send(fg);
                        }
                    }
                }
            }
            Err(_) => self.link_down(now),
        }
    }

    // ── what the relay says ──────────────────────────────────────────────────

    fn heard(&mut self, v: Value, now: Instant) {
        self.last_heard = now;
        self.awaiting = None;
        if self.down_since.take().is_some() {
            self.backoff = Duration::from_secs(1);
            if let Screen::Conv { ui, .. } = &mut self.screen {
                ui.link = Link::Up;
            }
        }
        let t = v.get("t").and_then(Value::as_str).unwrap_or("").to_string();
        match t.as_str() {
            "hi" => {
                if !std::mem::replace(&mut self.greeted, true) {
                    self.menu.set_busy(None);
                }
                let hosts: Vec<String> = v
                    .get("hosts")
                    .and_then(Value::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|h| h.as_str().map(str::to_string))
                    .collect();
                if hosts != self.hosts {
                    self.hosts = hosts;
                    self.menu_fresh = true;
                }
            }
            "hub" => {
                if let Some(h) = v.get("h") {
                    self.hub = proto::header_from(h);
                    self.menu.set_home(self.hub.clone());
                }
            }
            "snap" => {
                let host = v.get("c").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(snap) = v.get("s").and_then(|s| proto::snapshot_from(s, &host)) {
                    self.places
                        .insert(host.clone(), crate::hub::place_of(&host, &snap));
                    self.rows.insert(host, snap.rows);
                    self.refresh_menu();
                }
            }
            "down" => {
                let host = v.get("c").and_then(Value::as_str).unwrap_or("").to_string();
                let why = v.get("x").and_then(Value::as_str).unwrap_or("").to_string();
                let mut p = self
                    .places
                    .remove(&host)
                    .unwrap_or_else(|| Place::new(&host));
                p.header.memory = None;
                p.reach = Reach::Down(why);
                self.places.insert(host.clone(), p);
                self.rows.remove(&host);
                self.refresh_menu();
            }
            // A home action's outcome: the spinner stops, and the status line says it.
            "did" => {
                if self.working.take().is_some() {
                    self.menu.set_busy(None);
                }
                let said = v.get("x").and_then(Value::as_str).unwrap_or("").to_string();
                self.menu.set_status((!said.is_empty()).then_some(said));
            }
            "open" => {
                if let Screen::Conv { c, ui, opening } = &mut self.screen {
                    ui.status = None;
                    c.slot = v.get("s").and_then(Value::as_str).unwrap_or("").to_string();
                    c.run = v
                        .get("run")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    *opening = false;
                }
                self.open_msg = None;
                // The command list for the suggestion list, once per client.
                if self.catalog.is_empty() {
                    self.send(App::msg("models", vec![]));
                }
                // And what was typed while it opened.
                for v in std::mem::take(&mut self.held) {
                    self.send(v);
                }
            }
            "reset" => {
                if let Screen::Conv { c, .. } = &mut self.screen {
                    // What was sent and not yet echoed is still on its way.
                    let pending = std::mem::take(&mut c.pending);
                    let shown = std::mem::take(&mut c.shown);
                    **c = Convo::new(&c.host, &c.slot, &c.run);
                    c.pending = pending;
                    c.shown = shown;
                }
            }
            "err" | "end" => {
                let why = v.get("x").and_then(Value::as_str).unwrap_or("").to_string();
                match &mut self.screen {
                    Screen::Conv { opening: true, .. } => self.home(Some(why)),
                    Screen::Conv { c, .. } if t == "end" => {
                        let said = if why.is_empty() {
                            format!("{} ended", c.slot)
                        } else {
                            why
                        };
                        self.home(Some(said));
                    }
                    // A relay from before `/btw` refuses it: the panel says so, not waits.
                    Screen::Conv { ui, .. } => match &mut ui.dialog {
                        Some(Dialog::Btw {
                            answer: a @ None, ..
                        }) => *a = Some(Err(why)),
                        _ => ui.status = Some(why),
                    },
                    Screen::Home => {
                        if self.working.take().is_some() {
                            self.menu.set_busy(None);
                        }
                        self.menu.set_status(Some(why));
                    }
                }
            }
            "models" => {
                let list: Vec<(String, String)> = v
                    .get("m")
                    .and_then(Value::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(|m| {
                        let f =
                            |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        (f("v"), f("d"))
                    })
                    .collect();
                if let Screen::Conv { c, ui, .. } = &mut self.screen {
                    if let Some(Dialog::Model { sel, list: l }) = &mut ui.dialog {
                        *sel = list
                            .iter()
                            .position(|(v, d)| convo::is_current(&c.meta.model, v, d))
                            .unwrap_or(0);
                        *l = Some(list.clone());
                    }
                }
                self.models = Some(list);
                let catalog: Vec<convo::Command> = v
                    .get("c")
                    .and_then(Value::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|c| {
                        let f = |i: usize| {
                            c.as_arr()
                                .and_then(|a| a.get(i))
                                .and_then(Value::as_str)
                                .map(entities)
                        };
                        Some(convo::Command {
                            name: f(0)?,
                            about: f(1).unwrap_or_default(),
                            hint: f(2).unwrap_or_default(),
                        })
                    })
                    .filter(|c| !c.name.is_empty())
                    .collect();
                if !catalog.is_empty() {
                    if let Screen::Conv { ui, .. } = &mut self.screen {
                        ui.catalog = catalog.clone();
                    }
                    self.catalog = catalog;
                }
            }
            "btw" => {
                if let Screen::Conv { ui, .. } = &mut self.screen {
                    if let Some(Dialog::Btw { answer, .. }) = &mut ui.dialog {
                        if answer.is_none() {
                            *answer = Some(match v.get("x").and_then(Value::as_str) {
                                Some(a) => Ok(a.to_string()),
                                None => Err(v
                                    .get("e")
                                    .and_then(Value::as_str)
                                    .unwrap_or("no answer")
                                    .to_string()),
                            });
                        }
                    }
                }
            }
            // `/rc` done: said in the transcript, as Claude Code says it, its link copied to open
            // in the Claude app.
            "rc" => {
                if let Screen::Conv { c, ui, .. } = &mut self.screen {
                    let on = v.get("on").and_then(Value::as_bool) == Some(true);
                    let url = v.get("x").and_then(Value::as_str).unwrap_or("");
                    if on {
                        c.local(Entry::Note(format!("Remote Control active · {url}")));
                        if !url.is_empty() {
                            ui.status = Some(copied(url));
                            self.clip = Some(url.to_string());
                        }
                    } else {
                        c.local(Entry::Note("Remote Control disconnected.".into()));
                        ui.status = None;
                    }
                }
            }
            // A task's output, for its details in the tasks view.
            "tout" => {
                if let Screen::Conv { ui, .. } = &mut self.screen {
                    if let Some(Dialog::Tasks(view)) = &mut ui.dialog {
                        let id = v.get("i").and_then(Value::as_str);
                        if view.open.as_deref() == id {
                            view.output = Some(match v.get("x").and_then(Value::as_str) {
                                Some(x) => Ok(x.to_string()),
                                None => Err(v
                                    .get("e")
                                    .and_then(Value::as_str)
                                    .unwrap_or("no output")
                                    .to_string()),
                            });
                        }
                    }
                }
            }
            // The agent in view: its conversation, whole (`reset`) or as it goes.
            "fgo" => {
                if let Screen::Conv { c, .. } = &mut self.screen {
                    let id = v.get("i").and_then(Value::as_str).unwrap_or("");
                    let started = c
                        .tasks
                        .iter()
                        .find(|t| t.id == id)
                        .and_then(|t| t.started_ms);
                    let (host, slot, run, meta) = (
                        c.host.clone(),
                        c.slot.clone(),
                        c.run.clone(),
                        c.meta.clone(),
                    );
                    if let Some((_, a)) = c.viewing.as_mut().filter(|(i, _)| i == id) {
                        if v.get("reset").and_then(Value::as_bool) == Some(true) {
                            **a = Convo::new(&host, &slot, &run);
                            a.meta = meta;
                        }
                        for op in v.get("o").and_then(Value::as_arr).unwrap_or(&[]) {
                            a.apply(op, now);
                        }
                        // Its spinner counts from when it started, not from when it came
                        // into view.
                        if a.state == "working" {
                            if let Some(t0) = started {
                                let ran = crate::clock::now().saturating_sub(t0);
                                a.working_since = now.checked_sub(Duration::from_millis(ran));
                            }
                        }
                    }
                }
            }
            // The workflow browser's phases and agents.
            "wf" => {
                if let Screen::Conv { ui, .. } = &mut self.screen {
                    if let Some(Dialog::Tasks(view)) = &mut ui.dialog {
                        if view.open.as_deref() == v.get("i").and_then(Value::as_str) {
                            view.wf = Some(tasks::parse_wf(&v));
                        }
                    }
                }
            }
            // `s save` answered.
            "wsaved" => {
                if let Screen::Conv { ui, .. } = &mut self.screen {
                    if let Some(Dialog::Tasks(view)) = &mut ui.dialog {
                        if let Some(f) = view.save.as_mut() {
                            f.said = Some(match v.get("x").and_then(Value::as_str) {
                                Some(p) => Ok(p.to_string()),
                                None => Err(v
                                    .get("e")
                                    .and_then(Value::as_str)
                                    .unwrap_or("not saved")
                                    .to_string()),
                            });
                        }
                    }
                }
            }
            "pong" | "full" => {}
            _ => {
                // The conversation's ops.
                if let Screen::Conv { c, ui, .. } = &mut self.screen {
                    // Scrolled back, the view stays on what it shows while lines arrive under
                    // it; at the bottom it follows them.
                    let (w, h) = term::size().map_or((40, 24), |(w, h)| (w as usize, h as usize));
                    let held = (ui.scroll > 0).then(|| convo::max_scroll(c, ui, w, h));
                    match c.apply(&v, now) {
                        Applied::TurnEnded(d) => c.local(Entry::Worked(format!(
                            "Worked for {} · done {}",
                            took(d),
                            clock_time()
                        ))),
                        Applied::Back(text) => give_back(ui, &text),
                        Applied::Nothing => {}
                    }
                    follow_prompts(c, ui);
                    follow_tasks(c, ui);
                    if let Some(before) = held {
                        ui.scroll = hold_scroll(ui.scroll, before, convo::max_scroll(c, ui, w, h));
                    }
                }
            }
        }
        self.status_figures();
    }

    fn refresh_menu(&mut self) {
        for name in &self.hosts {
            if let Some(p) = self.places.get(name) {
                self.menu.set_place(p.clone());
            }
        }
        let lists: Vec<Vec<Row>> = self
            .hosts
            .iter()
            .map(|n| self.rows.get(n).cloned().unwrap_or_default())
            .collect();
        let refs: Vec<&[Row]> = lists.iter().map(Vec::as_slice).collect();
        let merged: Vec<Row> = crate::hub::merge(&refs)
            .into_iter()
            .map(terminal_mark)
            .collect();
        let first = std::mem::take(&mut self.menu_fresh);
        self.menu.replace_rows(merged, first);
    }

    fn rebuild_menu(&mut self, w: u16, h: u16) {
        let places: Vec<Place> = self
            .hosts
            .iter()
            .map(|n| self.places.get(n).cloned().unwrap_or_else(|| Place::new(n)))
            .collect();
        self.menu = Menu::hub(w, h, self.hub.clone(), places, Vec::new());
        self.menu.remote = true;
        self.menu_fresh = true;
        self.refresh_menu();
    }

    /// The conversation's status line: the figures the door's load line totals — the hub's
    /// own reading, then every container answering — named for the conversation's container,
    /// which is what a session's line says under ssh.
    fn status_figures(&mut self) {
        let Screen::Conv { c, ui, .. } = &mut self.screen else {
            return;
        };
        let answering = self
            .places
            .values()
            .filter(|p| p.reach == Reach::Up)
            .map(|p| &p.header);
        let place = self
            .places
            .get(&c.host)
            .map(|p| p.header.host.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| c.host.clone());
        ui.load = Some(crate::statusline::door(
            self.hub.load.clone(),
            self.hub.memory,
            answering,
            &place,
        ));
    }

    /// What a selection covers on the screen as drawn, to the clipboard, and Claude Code's
    /// words for how it went there.
    fn copy_selection(&mut self, sel: Selection) {
        let (w, h) = term::size().unwrap_or((40, 24));
        let Screen::Conv { c, ui, .. } = &mut self.screen else {
            return;
        };
        let f = convo::draw(c, ui, w as usize, h as usize, Instant::now());
        let rows: Vec<String> = f.lines.iter().map(convo::plain).collect();
        let text = selected_text(&rows, sel);
        if text.trim().is_empty() {
            self.select = None;
            return;
        }
        ui.status = Some(selection_copied(&text));
        self.clip = Some(text);
    }

    /// The open shell's or monitor's output, asked for when its details open and every
    /// `OUTPUT_EVERY` while they stay open.
    fn want_output(&mut self, now: Instant) {
        let Some(id) = self.output_wanted() else {
            self.output_for = None;
            return;
        };
        let due = match &self.output_for {
            Some((was, at)) if *was == id => now >= *at,
            _ => true,
        };
        if due && self.up() {
            self.output_for = Some((id.clone(), now + OUTPUT_EVERY));
            // `wf:<task>[:<agent>]` for a workflow's browser, a task id for a shell's output.
            let m = match id.strip_prefix("wf:") {
                Some(rest) => {
                    let (task, agent) = match rest.split_once(':') {
                        Some((t, a)) => (t, a.parse::<u64>().ok()),
                        None => (rest, None),
                    };
                    let mut fields = vec![("i", Value::string(task))];
                    if let Some(a) = agent {
                        fields.push(("a", Value::num(a as f64)));
                    }
                    App::msg("wf", fields)
                }
                None => App::msg("tout", vec![("i", Value::string(id))]),
            };
            self.send(m);
        }
    }

    /// The task whose output the open tasks view shows, if any.
    fn output_wanted(&self) -> Option<String> {
        let Screen::Conv { c, ui, .. } = &self.screen else {
            return None;
        };
        let Some(Dialog::Tasks(v)) = &ui.dialog else {
            return None;
        };
        let id = v.open.as_ref()?;
        let t = c.tasks.iter().find(|t| &t.id == id)?;
        match t.kind.as_str() {
            "shell" | "monitor" => Some(t.id.clone()),
            // A workflow's browser, with the previews of the agent whose pane is open: a new
            // key, so it is asked for at once.
            "workflow" => {
                let wf = v.wf.clone().unwrap_or_default();
                let agent = (v.pane == tasks::Pane::Agent)
                    .then(|| tasks::wf_agents(&wf, v).get(v.agent).map(|(i, _)| *i))
                    .flatten();
                Some(match agent {
                    Some(a) => format!("wf:{}:{a}", t.id),
                    None => format!("wf:{}", t.id),
                })
            }
            _ => None,
        }
    }

    /// Fewer than `AHEAD` screens above the view, with more before them: the next stretch is
    /// asked for, so a flick back seldom reaches the top of what has come (owner, 2026-11-02).
    fn want_older(&mut self) {
        if !self.up() {
            return;
        }
        let Screen::Conv {
            c,
            ui,
            opening: false,
        } = &mut self.screen
        else {
            return;
        };
        if !c.more || c.asking {
            return;
        }
        let (w, h) = size();
        let top = convo::max_scroll(c, ui, w, h);
        if top.saturating_sub(ui.scroll) >= AHEAD * h {
            return;
        }
        c.asking = true;
        self.send(App::msg("older", vec![]));
    }

    fn home(&mut self, status: Option<String>) {
        self.screen = Screen::Home;
        self.menu.set_status(status);
        self.drawn.clear();
    }

    // ── keys ─────────────────────────────────────────────────────────────────

    fn key(&mut self, k: Key, now: Instant) {
        // A key after a long silence: make sure the link is there before relying on it.
        if self.up() && now.duration_since(self.last_heard) > QUIET_PROBE {
            self.send(App::msg("ping", vec![]));
        }
        // A drag over the conversation selects, and letting go copies, as in Claude Code: its
        // fullscreen mode holds the mouse, so the terminal's own selection cannot.
        let conv = matches!(self.screen, Screen::Conv { .. });
        match k {
            Key::Click { row, col } if conv => {
                self.select = Some(Selection {
                    from: (row, col),
                    to: (row, col),
                    moved: false,
                });
                return;
            }
            Key::Drag { row, col } => {
                if let Some(s) = &mut self.select {
                    s.to = (row, col);
                    s.moved = true;
                }
                return;
            }
            Key::Release { row, col } => {
                match self.select.as_mut() {
                    Some(s) if s.moved && (s.from != (row, col) || s.from != s.to) => {
                        s.to = (row, col);
                        let sel = *s;
                        self.copy_selection(sel);
                    }
                    _ => self.select = None,
                }
                return;
            }
            // A key puts a selection away.
            _ if self.select.is_some() && !matches!(k, Key::PasteStart | Key::PasteEnd) => {
                self.select = None;
            }
            _ => {}
        }
        // A paste is gathered whole first, so nothing in it is read as a key: on the home, a
        // pasted `n` would start a session.
        match (k, &mut self.pasting) {
            (Key::PasteStart, _) => {
                self.pasting = Some(String::new());
                return;
            }
            (Key::PasteEnd, _) => {
                let text = self.pasting.take().unwrap_or_default();
                if let Screen::Conv { ui, .. } = &mut self.screen {
                    if ui.dialog.is_some() {
                        // A dialog's text field takes it typed, on one line.
                        for c in text.chars() {
                            self.conv_key(Key::Char(if c == '\n' { ' ' } else { c }));
                        }
                    } else {
                        let rows = term::size().map_or(24, |(_, h)| h as usize);
                        paste(ui, &text, rows);
                    }
                }
                return;
            }
            (Key::Char(c), Some(text)) => {
                text.push(c);
                return;
            }
            (_, Some(_)) => return,
            _ => {}
        }
        match &mut self.screen {
            Screen::Home => self.home_key(k),
            Screen::Conv { .. } => self.conv_key(k),
        }
    }

    fn home_key(&mut self, k: Key) {
        if k == Key::Char(DETACH) {
            self.quit = true;
            return;
        }
        let action = self.menu.key(k);
        match action {
            Action::Quit => self.quit = true,
            Action::Open(i) => {
                let Some(row) = self.menu.rows().get(i).cloned() else {
                    return;
                };
                if is_terminal(&row) {
                    self.menu.set_status(Some(TERMINAL_ONLY.to_string()));
                    return;
                }
                let open = App::msg(
                    "open",
                    vec![
                        ("c", Value::string(&row.host)),
                        ("k", proto::key_to(&row.key)),
                    ],
                );
                self.open_conv(&row.host, open);
            }
            Action::New(host, Harness::Claude) => {
                let new = App::msg("new", vec![("c", Value::string(&host))]);
                self.open_conv(&host, new);
            }
            // `c`, as at the door: offload a loaded session, archive an offloaded one.
            Action::Offload(i) | Action::Archive(i) => {
                let Some(row) = self.menu.rows().get(i).cloned() else {
                    return;
                };
                if !self.up() {
                    self.menu
                        .set_status(Some("not connected: try again once it is".into()));
                    return;
                }
                let (t, what) = if matches!(action, Action::Offload(_)) {
                    ("offload", "offloading session")
                } else {
                    ("archive", "archiving session")
                };
                self.send(App::msg(
                    t,
                    vec![
                        ("c", Value::string(&row.host)),
                        ("k", proto::key_to(&row.key)),
                    ],
                ));
                self.working = Some((Instant::now(), i, what.to_string()));
            }
            // A shell, a pi session and the no-room question need the door's terminal: the
            // phone's home does not offer them (mockup R9), and their keys do nothing.
            Action::New(_, Harness::Pi) | Action::Shell(_) => {}
            Action::None | Action::Redraw => {}
        }
    }

    fn open_conv(&mut self, host: &str, mut msg: Value) {
        // The screen's size: the relay sends a few screens of the conversation, and the rest
        // as it is scrolled back to.
        let (w, r) = size();
        msg.set("w", Value::num(w as f64));
        msg.set("r", Value::num(r as f64));
        if !self.up() {
            self.menu
                .set_status(Some("not connected: try again once it is".into()));
            return;
        }
        self.send(msg.clone());
        self.open_msg = Some(msg);
        self.held.clear();
        let mut ui = Ui::new();
        ui.status = Some("Opening…".into());
        ui.catalog = self.catalog.clone();
        self.screen = Screen::Conv {
            c: Box::new(Convo::new(host, "", "")),
            ui: Box::new(ui),
            opening: true,
        };
        self.drawn.clear();
    }

    fn conv_key(&mut self, k: Key) {
        let Screen::Conv { c, ui, .. } = &mut self.screen else {
            return;
        };
        ui.status = None;
        ui.toast = None;
        let mut out: Vec<Value> = Vec::new();
        let mut leave = false;
        // Ctrl+X Ctrl+S, Claude Code's send-now: what waits behind the turn goes now. Ctrl+X
        // alone starts the chord; any other key after it is itself.
        let mut chorded = false;
        if ui.dialog.is_none() {
            if std::mem::take(&mut ui.chord) {
                if k == Key::Char(CTRL_S) {
                    chorded = true;
                    if c.pending.iter().any(|p| p.queued) {
                        out.push(App::msg("now", vec![]));
                    }
                }
            } else if k == Key::Char(CTRL_X) {
                ui.chord = true;
                return;
            }
        }
        // The suggestion list, while it shows, has the arrows, Tab, Esc and Enter, as Claude
        // Code's does: Enter takes the chosen command and sends it.
        let before = ui.input.clone();
        let list = if chorded {
            Vec::new()
        } else {
            convo::suggestions(ui)
        };
        let mut suggested = false;
        if !list.is_empty() {
            let sel = ui.suggest.min(list.len() - 1);
            match k {
                Key::Up => {
                    ui.suggest = sel.saturating_sub(1);
                    suggested = true;
                }
                Key::Down => {
                    ui.suggest = (sel + 1).min(list.len() - 1);
                    suggested = true;
                }
                Key::Char('\t') => {
                    ui.input = format!("/{} ", list[sel].name);
                    ui.cursor = ui.input.chars().count();
                    suggested = true;
                }
                Key::Esc => {
                    ui.suggest_off = true;
                    suggested = true;
                }
                // A name typed whole is that command, whatever the list has chosen: `/copy` is
                // `/copy`, not the first command with `copy` in it.
                Key::Enter if !list.iter().any(|c| ui.input == format!("/{}", c.name)) => {
                    ui.input = format!("/{}", list[sel].name);
                    ui.cursor = ui.input.chars().count();
                }
                _ => {}
            }
        }
        // What Down focused, as in Claude Code: on the pill, Enter opens the tasks view —
        // straight into the details when only one task runs; on the agent strip, Enter views
        // that agent, or the conversation again on `main`, and `x` stops one. Any other key
        // puts the focus away and is itself.
        let now_ms = ui.now_ms();
        let viewing = c.viewing.as_ref().map(|(id, _)| id.clone());
        let strip: Vec<convo::Task> = tasks::strip_tasks(&c.tasks, now_ms, viewing.as_deref())
            .into_iter()
            .cloned()
            .collect();
        let has_pill = tasks::pill(&c.tasks).is_some();
        let mut early = false;
        let focus = std::mem::take(&mut ui.focus);
        if focus != Focus::Prompt && ui.dialog.is_none() && !suggested {
            match (focus, k) {
                (Focus::Pill, Key::Enter) => {
                    let mut v = tasks::View {
                        inline: true,
                        notices_at: c.notices,
                        ..tasks::View::default()
                    };
                    let rows = tasks::listed(&c.tasks);
                    if let [(_, t)] = rows.as_slice() {
                        v.direct = true;
                        open_task(&mut v, t);
                    }
                    ui.dialog = Some(Dialog::Tasks(v));
                    early = true;
                }
                (Focus::Pill, Key::Down) => {
                    ui.focus = if strip.is_empty() {
                        Focus::Pill
                    } else {
                        Focus::Strip(0)
                    };
                    early = true;
                }
                (Focus::Strip(i), Key::Down) => {
                    ui.focus = Focus::Strip((i + 1).min(strip.len()));
                    early = true;
                }
                (Focus::Strip(0), Key::Up) => {
                    ui.focus = if has_pill { Focus::Pill } else { Focus::Prompt };
                    early = true;
                }
                (Focus::Strip(i), Key::Up) => {
                    ui.focus = Focus::Strip(i - 1);
                    early = true;
                }
                (Focus::Strip(0), Key::Enter) => {
                    if viewing.is_some() {
                        leave_agent(c, ui);
                        out.push(App::msg("fg", vec![]));
                    }
                    early = true;
                }
                (Focus::Strip(i), Key::Enter) => {
                    if let Some(t) = strip.get(i - 1) {
                        if t.kind == "agent" {
                            view_agent(c, ui, t);
                            out.push(App::msg("fg", vec![("i", Value::string(&t.id))]));
                        } else {
                            // A workflow's is its browser.
                            let mut v = tasks::View {
                                notices_at: c.notices,
                                ..tasks::View::default()
                            };
                            open_task(&mut v, t);
                            ui.dialog = Some(Dialog::Tasks(v));
                        }
                    }
                    early = true;
                }
                (Focus::Strip(i), Key::Char('x')) if i > 0 => {
                    if let Some(t) = strip.get(i - 1).filter(|t| t.running()) {
                        out.push(App::msg("tstop", vec![("i", Value::string(&t.id))]));
                    }
                    ui.focus = Focus::Strip(i);
                    early = true;
                }
                (_, Key::Up | Key::Esc) => early = true,
                _ => {}
            }
        }
        if early {
        } else {
            match ui.dialog.take() {
                // Ctrl+\\ detaches from anywhere in a conversation, a dialog open or not, as zmx's
                // detach key does in ssh dev: the session runs on, and the home comes back.
                _ if k == Key::Char(DETACH) => leave = true,
                _ if chorded || suggested => {}
                Some(Dialog::Btw {
                    question,
                    answer,
                    top,
                }) => match k {
                    Key::Esc | Key::Enter | Key::Char(' ') => {}
                    Key::Up | Key::WheelUp | Key::PageUp => {
                        let step = if k == Key::PageUp { 10 } else { 1 };
                        ui.dialog = Some(Dialog::Btw {
                            question,
                            answer,
                            top: top.saturating_sub(step),
                        });
                    }
                    Key::Down | Key::WheelDown | Key::PageDown => {
                        let step = if k == Key::PageDown { 10 } else { 1 };
                        ui.dialog = Some(Dialog::Btw {
                            question,
                            answer,
                            top: top + step,
                        });
                    }
                    _ => {
                        ui.dialog = Some(Dialog::Btw {
                            question,
                            answer,
                            top,
                        })
                    }
                },
                Some(Dialog::Copy { sel, choices }) => match k {
                    Key::Up => {
                        ui.dialog = Some(Dialog::Copy {
                            sel: sel.saturating_sub(1),
                            choices,
                        })
                    }
                    Key::Down => {
                        let sel = (sel + 1).min(choices.len().saturating_sub(1));
                        ui.dialog = Some(Dialog::Copy { sel, choices });
                    }
                    Key::Enter => {
                        if let Some((_, t)) = choices.into_iter().nth(sel) {
                            ui.status = Some(copied(&t));
                            self.clip = Some(t);
                        }
                    }
                    Key::Esc => {}
                    _ => ui.dialog = Some(Dialog::Copy { sel, choices }),
                },
                Some(Dialog::Tasks(v)) => {
                    let mut fg = None;
                    ui.dialog = tasks_key(c, v, k, &mut out, &mut ui.toast, &mut fg);
                    if let Some(t) = fg {
                        view_agent(c, ui, &t);
                        out.push(App::msg("fg", vec![("i", Value::string(&t.id))]));
                    }
                }
                Some(d) => {
                    ui.dialog = dialog_key(c, d, k, &mut out);
                }
                None => match k {
                    Key::Char('\x03') | Key::Char('\x04') if ui.input.is_empty() => leave = true,
                    Key::Char('\x03') => {
                        ui.input.clear();
                        ui.cursor = 0;
                    }
                    // Ctrl+O, Claude Code's: every tool call drawn with its result, or summed again.
                    Key::Char('\x0f') => c.expanded = !c.expanded,
                    // An agent in view: Esc interrupts it, as Claude Code's does, or leaves the
                    // view of one that has ended.
                    Key::Esc if ui.scroll == 0 && viewing.is_some() => {
                        let id = viewing.clone().unwrap_or_default();
                        let running = c.tasks.iter().any(|t| t.id == id && t.running());
                        if running {
                            out.push(App::msg("tesc", vec![("i", Value::string(&id))]));
                        } else {
                            leave_agent(c, ui);
                            out.push(App::msg("fg", vec![]));
                        }
                    }
                    // Claude Code messages the agent; headless claude refuses to
                    // (`send_task_message`, 2.1.311), so the prompt stays and says so.
                    Key::Enter
                        if viewing.is_some()
                            && !ui.input.trim().is_empty()
                            && !ui.input.starts_with('/') =>
                    {
                        ui.status = Some(AGENT_MESSAGE_REFUSED.into());
                    }
                    Key::Esc => {
                        if ui.scroll > 0 {
                            ui.scroll = 0;
                        } else if c.state != "idle" || !c.pending.is_empty() {
                            // A turn, or a prompt not yet taken in: claude says what came of it,
                            // and a prompt it caught unanswered comes back to the input (`back`).
                            out.push(App::msg("esc", vec![]));
                        }
                    }
                    Key::Enter if backslash_newline(ui) => {}
                    Key::Enter => {
                        let typed = std::mem::take(&mut ui.input);
                        let text = expand_pastes(&typed, &ui.pastes);
                        if !ui.pastes.is_empty() {
                            c.collapsed(&text, &typed);
                        }
                        ui.pastes.clear();
                        ui.cursor = 0;
                        ui.scroll = 0;
                        if !text.trim().is_empty() {
                            self.history.push(text.clone());
                        }
                        self.history_at = None;
                        self.draft = None;
                        match submit(c, ui, text.trim(), &self.models) {
                            Submit::Send(v) => out.extend(v),
                            Submit::Copy(t) => self.clip = Some(t),
                            Submit::Home => leave = true,
                            Submit::Nothing => {}
                        }
                    }
                    // Claude Code's "Press up to edit queued messages": out of the queue, into the
                    // prompt, a line each, above anything typed meanwhile (2.1.311, driven in a pty
                    // on 2026-11-02).
                    Key::Up if c.pending.iter().any(|p| p.queued) => {
                        let (texts, uuids) = c.unqueue();
                        give_back(ui, &texts.join("\n"));
                        out.push(App::msg(
                            "unqueue",
                            vec![(
                                "u",
                                Value::Arr(uuids.into_iter().map(Value::string).collect()),
                            )],
                        ));
                    }
                    // Up and Down move between the prompt's rows first; off its top or bottom row
                    // they step through what was sent, as Claude Code's do.
                    // Down in an empty prompt, not stepping through what was sent: the tasks
                    // pill takes the focus, as in Claude Code.
                    Key::Down
                        if ui.input.is_empty()
                            && self.history_at.is_none()
                            && (has_pill || !strip.is_empty()) =>
                    {
                        ui.focus = if has_pill {
                            Focus::Pill
                        } else {
                            Focus::Strip(0)
                        };
                    }
                    Key::Up | Key::Down => {
                        let room = term::size().map_or(38, |(w, _)| (w as usize).saturating_sub(2));
                        if !row_step(ui, k == Key::Up, room) {
                            history_step(
                                ui,
                                k == Key::Up,
                                &self.history,
                                &mut self.history_at,
                                &mut self.draft,
                            );
                        }
                    }
                    Key::PageUp | Key::WheelUp => {
                        let step = if k == Key::PageUp { 10 } else { self.wheel };
                        let (w, h) = term::size().unwrap_or((40, 24));
                        let most = convo::max_scroll(c, ui, w as usize, h as usize);
                        ui.scroll = (ui.scroll + step).min(most);
                    }
                    Key::PageDown | Key::WheelDown => {
                        let step = if k == Key::PageDown { 10 } else { self.wheel };
                        ui.scroll = ui.scroll.saturating_sub(step);
                    }
                    Key::End if ui.scroll > 0 => ui.scroll = 0,
                    k => edit(ui, k),
                },
            }
        }
        // A changed prompt: the list starts again from its top, and shows again after Esc.
        if let Screen::Conv { ui, .. } = &mut self.screen {
            if ui.input != before {
                ui.suggest = 0;
                ui.suggest_off = false;
            }
        }
        if leave {
            self.send(App::msg("close", vec![]));
            self.home(None);
            return;
        }
        // An Enter while the link is down keeps its text, and says it was not sent.
        if !out.is_empty() && !self.up() {
            if let Screen::Conv { c, ui, .. } = &mut self.screen {
                if let Link::Down { not_sent, .. } = &mut ui.link {
                    *not_sent = true;
                }
                // Nothing went, so nothing is on its way: it is back in the prompt instead.
                if out
                    .iter()
                    .any(|m| m.get("t").and_then(Value::as_str) == Some("say"))
                {
                    c.pending.pop();
                }
                if let Some(last) = self.history.last() {
                    if ui.input.is_empty() {
                        ui.input = last.clone();
                        ui.cursor = ui.input.chars().count();
                    }
                }
            }
            return;
        }
        // A conversation still opening takes nothing yet: what was typed waits, spinning, and
        // goes the moment it is open.
        if matches!(self.screen, Screen::Conv { opening: true, .. }) {
            self.held.extend(out);
            return;
        }
        for v in out {
            self.send(v);
        }
    }
}

/// A prompt or question has arrived, or been answered elsewhere: open or close its dialog.
/// The scroll that keeps the same lines on screen when the bottom of the transcript moved from
/// `before` to `after` lines: as many more lines back as arrived, fewer as went. Never past
/// the top, and not back to following unless the lines it showed are gone.
fn hold_scroll(scroll: usize, before: usize, after: usize) -> usize {
    if after == 0 {
        return 0;
    }
    let moved = scroll as i64 + after as i64 - before as i64;
    moved.clamp(1, after as i64) as usize
}

/// The task list changed under the tasks view: a task whose details are open and that has
/// left the list goes back to the list, as Claude Code's does when a shell finishes, or
/// closes the view when it was opened straight into it; the pill loses its focus once there
/// is no pill.
fn follow_tasks(c: &Convo, ui: &mut Ui) {
    let now_ms = ui.now_ms();
    let viewing = c.viewing.as_ref().map(|(id, _)| id.as_str());
    let n = tasks::strip_tasks(&c.tasks, now_ms, viewing).len();
    ui.focus = match ui.focus {
        Focus::Pill if tasks::pill(&c.tasks).is_none() => Focus::Prompt,
        Focus::Strip(_) if n == 0 => Focus::Prompt,
        Focus::Strip(i) => Focus::Strip(i.min(n)),
        f => f,
    };
    let Some(Dialog::Tasks(v)) = &mut ui.dialog else {
        return;
    };
    let rows = tasks::listed(&c.tasks);
    if let Some(id) = &v.open {
        if !rows.iter().any(|(_, t)| &t.id == id) {
            if v.direct {
                ui.dialog = None;
                return;
            }
            v.open = None;
            v.output = None;
        }
    }
    v.sel = v.sel.min(rows.len().saturating_sub(1));
}

/// A task's details opened: how long it has run, kept as Claude Code keeps it.
fn open_task(v: &mut tasks::View, t: &convo::Task) {
    v.open = Some(t.id.clone());
    v.output = None;
    v.runtime = tasks::runtime(t, crate::clock::now());
}

/// An agent in view in place of the conversation, Claude Code's foregrounded agent: its
/// own conversation comes from the relay (`fgo`).
fn view_agent(c: &mut Convo, ui: &mut Ui, t: &convo::Task) {
    let mut a = Convo::new(&c.host, &c.slot, &c.run);
    a.meta = c.meta.clone();
    c.viewing = Some((t.id.clone(), Box::new(a)));
    ui.scroll = 0;
    ui.focus = Focus::Prompt;
    ui.dialog = None;
}

/// The conversation back in view.
fn leave_agent(c: &mut Convo, ui: &mut Ui) {
    c.viewing = None;
    ui.scroll = 0;
    ui.focus = Focus::Prompt;
}

/// A key in the tasks view; what it leaves open. `toast` takes Claude Code's word on closing
/// a task's details, and `fg` the agent to bring into view.
fn tasks_key(
    c: &Convo,
    mut v: tasks::View,
    k: Key,
    out: &mut Vec<Value>,
    toast: &mut Option<String>,
    fg: &mut Option<convo::Task>,
) -> Option<Dialog> {
    let rows = tasks::listed(&c.tasks);
    let stop = |t: &convo::Task, out: &mut Vec<Value>| {
        if t.running() {
            out.push(App::msg("tstop", vec![("i", Value::string(&t.id))]));
        }
    };
    let open = v
        .open
        .as_ref()
        .and_then(|id| c.tasks.iter().find(|t| &t.id == id))
        .cloned();
    let back = |v: &mut tasks::View| {
        v.open = None;
        v.output = None;
        v.wf = None;
        v.pane = tasks::Pane::Phases;
        v.phase = 0;
        v.agent = 0;
        v.filter.clear();
        v.save = None;
    };
    match (open, k) {
        // The workflow browser.
        (Some(t), k) if t.kind == "workflow" => {
            return workflow_key(&t, v, k, out);
        }
        // Details: back to the list, closed, the task stopped, or the agent in view.
        (Some(_), Key::Left) if !v.direct => back(&mut v),
        (Some(t), Key::Left | Key::Esc | Key::Enter | Key::Char(' ')) => {
            if k == Key::Esc && !v.direct {
                *toast = Some(format!("{} details dismissed", kind_word(&t)));
            }
            return None;
        }
        (Some(t), Key::Char('x')) => {
            stop(&t, out);
            if v.direct {
                return None;
            }
            back(&mut v);
        }
        (Some(t), Key::Char('f')) if t.kind == "agent" && t.running() => {
            *fg = Some(t);
            return None;
        }
        (Some(_), _) => {}
        // The list.
        (None, Key::Up) => v.sel = v.sel.saturating_sub(1),
        (None, Key::Down) => v.sel = (v.sel + 1).min(rows.len().saturating_sub(1)),
        (None, Key::Enter) => {
            if let Some((_, t)) = rows.get(v.sel) {
                // A finished agent opens its conversation, as Claude Code's does.
                if t.kind == "agent" && !t.running() {
                    *fg = Some((*t).clone());
                    return None;
                }
                open_task(&mut v, t);
            }
        }
        (None, Key::Char('f')) => {
            if let Some((_, t)) = rows.get(v.sel).filter(|(_, t)| t.kind == "agent") {
                *fg = Some((*t).clone());
                return None;
            }
        }
        (None, Key::Char('x')) => {
            if let Some((_, t)) = rows.get(v.sel) {
                stop(t, out);
            }
        }
        (None, Key::Esc | Key::Left) => return None,
        (None, _) => {}
    }
    Some(Dialog::Tasks(v))
}

/// What Claude Code calls a task's details: `Shell details`, `Monitor details`.
fn kind_word(t: &convo::Task) -> &'static str {
    match t.kind.as_str() {
        "shell" => "Shell",
        "monitor" => "Monitor",
        "workflow" => "Workflow",
        _ => "Agent",
    }
}

/// A key in the workflow browser: phases, then agents (→ or Enter), then one agent's pane;
/// Esc or ← back a level at a time; `x` stops the workflow, `f` filters the agents, `s`
/// saves its script.
fn workflow_key(
    t: &convo::Task,
    mut v: tasks::View,
    k: Key,
    out: &mut Vec<Value>,
) -> Option<Dialog> {
    use tasks::Pane;
    // `s save`'s form has the keys while it is open.
    if let Some(mut f) = v.save.take() {
        match k {
            Key::Esc => {}
            Key::Char('\t') => {
                f.user = !f.user;
                f.said = None;
                v.save = Some(f);
            }
            Key::Enter if matches!(f.said, Some(Ok(_))) => {}
            Key::Enter => {
                let name = if f.name.trim().is_empty() {
                    t.detail.clone()
                } else {
                    f.name.clone()
                };
                out.push(App::msg(
                    "wsave",
                    vec![
                        ("i", Value::string(&t.id)),
                        ("n", Value::string(name)),
                        ("sc", Value::string(if f.user { "user" } else { "project" })),
                    ],
                ));
                f.said = None;
                v.save = Some(f);
            }
            Key::Char('\x7f') | Key::Char('\x08') => {
                f.name.pop();
                f.said = None;
                v.save = Some(f);
            }
            Key::Char(ch) if !ch.is_control() => {
                f.name.push(ch);
                f.said = None;
                v.save = Some(f);
            }
            _ => v.save = Some(f),
        }
        return Some(Dialog::Tasks(v));
    }
    let wf = v.wf.clone().unwrap_or_default();
    let phases = wf.phases.len().max(1);
    let agents = tasks::wf_agents(&wf, &v).len();
    match (v.pane, k) {
        (_, Key::Char('s')) if t.saveable => {
            v.save = Some(tasks::Save {
                name: t.detail.clone(),
                ..tasks::Save::default()
            });
        }
        (Pane::Phases, Key::Up) => v.phase = v.phase.saturating_sub(1),
        (Pane::Phases, Key::Down) => v.phase = (v.phase + 1).min(phases - 1),
        (Pane::Phases, Key::Right | Key::Enter) => {
            v.pane = Pane::Agents;
            v.agent = 0;
        }
        (Pane::Phases, Key::Char('x')) if t.running() => {
            out.push(App::msg("tstop", vec![("i", Value::string(&t.id))]));
        }
        // Back to the list, as Claude Code's browser goes; closed if the list was not seen.
        (Pane::Phases, Key::Esc | Key::Left) => {
            if v.direct {
                return None;
            }
            v.open = None;
            v.wf = None;
            v.phase = 0;
        }
        (Pane::Agents, Key::Up) => v.agent = v.agent.saturating_sub(1),
        (Pane::Agents, Key::Down) => v.agent = (v.agent + 1).min(agents.saturating_sub(1)),
        (Pane::Agents, Key::Enter) if agents > 0 => v.pane = Pane::Agent,
        (Pane::Agents, Key::Char('f')) => {
            let at = tasks::FILTERS
                .iter()
                .position(|f| *f == v.filter)
                .unwrap_or(0);
            v.filter = tasks::FILTERS[(at + 1) % tasks::FILTERS.len()].to_string();
            v.agent = 0;
        }
        (Pane::Agents, Key::Esc | Key::Left) => v.pane = Pane::Phases,
        (Pane::Agent, Key::Esc | Key::Left | Key::Enter) => v.pane = Pane::Agents,
        _ => {}
    }
    Some(Dialog::Tasks(v))
}

fn follow_prompts(c: &Convo, ui: &mut Ui) {
    let waiting = c.waiting();
    match (&ui.dialog, waiting) {
        (None, Some(Entry::Perm { .. })) => ui.dialog = Some(Dialog::Perm { sel: 0 }),
        (None, Some(Entry::Ask { questions, .. })) => {
            ui.dialog = Some(Dialog::Ask {
                q: 0,
                sel: 0,
                answers: Vec::new(),
                typing: None,
                ticked: vec![false; questions.first().map_or(0, |q| q.options.len())],
            })
        }
        (Some(Dialog::Perm { .. } | Dialog::Ask { .. }), None) => ui.dialog = None,
        _ => {}
    }
}

enum Submit {
    Send(Vec<Value>),
    Copy(String),
    Home,
    Nothing,
}

/// What the owner typed: a slash command the client knows, or a prompt for claude.
fn submit(
    c: &mut Convo,
    ui: &mut Ui,
    text: &str,
    models: &Option<Vec<(String, String)>>,
) -> Submit {
    if text.is_empty() {
        return Submit::Nothing;
    }
    let (cmd, rest) = match text.split_once(' ') {
        Some((a, b)) => (a, b.trim()),
        None => (text, ""),
    };
    let m = App::msg;
    match cmd {
        "/model" if rest.is_empty() => {
            c.local(Entry::Prompt(text.into()));
            let sel = models
                .as_ref()
                .and_then(|l| {
                    l.iter()
                        .position(|(v, d)| convo::is_current(&c.meta.model, v, d))
                })
                .unwrap_or(0);
            ui.dialog = Some(Dialog::Model {
                sel,
                list: models.clone(),
            });
            if models.is_none() {
                return Submit::Send(vec![m("models", vec![])]);
            }
            Submit::Nothing
        }
        "/model" => Submit::Send(vec![m("model", vec![("m", Value::string(rest))])]),
        "/effort" if rest.is_empty() => {
            c.local(Entry::Prompt(text.into()));
            let sel = convo::EFFORTS
                .iter()
                .position(|e| *e == c.meta.effort)
                .unwrap_or(1);
            ui.dialog = Some(Dialog::Effort { sel });
            Submit::Nothing
        }
        "/effort" => {
            let e = if rest == "auto" || rest == "default" {
                Value::Null
            } else {
                Value::string(rest)
            };
            Submit::Send(vec![m("effort", vec![("e", e)])])
        }
        "/auto" => {
            let next = convo::next_mode(&c.meta.mode);
            Submit::Send(vec![m("mode", vec![("m", Value::string(next))])])
        }
        "/rename" if rest.is_empty() => {
            ui.status = Some("/rename takes the new title".into());
            Submit::Nothing
        }
        "/rename" => Submit::Send(vec![m("rename", vec![("x", Value::string(rest))])]),
        "/exit" | "/quit" => Submit::Send(vec![m("exit", vec![])]),
        // Back to the home, the session left running: zmx's detach, as Ctrl+\\ is in ssh dev.
        "/home" | "/disconnect" => Submit::Home,
        "/expand" => {
            c.expanded = !c.expanded;
            Submit::Nothing
        }
        // Claude Code's tasks view: what runs in the background, its output, and `x` to stop.
        "/tasks" | "/bashes" => {
            ui.dialog = Some(Dialog::Tasks(tasks::View {
                notices_at: c.notices,
                ..tasks::View::default()
            }));
            Submit::Nothing
        }
        // Claude Code's `/remote-control`: the session reachable from the Claude app, where a
        // screenshot can be attached; again, it is turned off.
        "/rc" | "/remote-control" => {
            ui.status = Some("Remote Control…".into());
            Submit::Send(vec![m("rc", vec![])])
        }
        "/btw" if rest.is_empty() => {
            ui.status = Some("/btw takes a question".into());
            Submit::Nothing
        }
        // A side question, answered beside the conversation and kept out of it: shown in its
        // own panel until dismissed, as Claude Code's is.
        "/btw" => {
            ui.dialog = Some(Dialog::Btw {
                question: rest.to_string(),
                answer: None,
                top: 0,
            });
            Submit::Send(vec![m("btw", vec![("x", Value::string(rest))])])
        }
        "/copy" => {
            let back = if rest.is_empty() {
                Some(1)
            } else {
                rest.parse::<usize>().ok().filter(|n| *n > 0)
            };
            let Some(back) = back else {
                ui.status = Some("/copy takes how many responses back, 1 the latest".into());
                return Submit::Nothing;
            };
            let all = c.responses();
            let Some(text) = all.len().checked_sub(back).and_then(|i| all.get(i)) else {
                ui.status = Some(if all.is_empty() {
                    "Nothing to copy yet".into()
                } else {
                    format!("Only {} responses to copy from", all.len())
                });
                return Submit::Nothing;
            };
            let choices = convo::copy_choices(text);
            if choices.len() > 1 {
                ui.dialog = Some(Dialog::Copy { sel: 0, choices });
                return Submit::Nothing;
            }
            ui.status = Some(copied(text));
            Submit::Copy(text.clone())
        }
        _ => {
            let uuid = c.sent(text, Instant::now());
            Submit::Send(vec![m(
                "say",
                vec![("x", Value::string(text)), ("u", Value::string(uuid))],
            )])
        }
    }
}

/// What the status line says once something is on the clipboard.
fn copied(text: &str) -> String {
    format!(
        "Copied {} characters to the clipboard",
        text.chars().count()
    )
}

/// A key in the prompt: typing, moving and deleting.
/// The terminal's columns and rows, or the narrow mockups' when it cannot say.
fn size() -> (usize, usize) {
    term::size().map_or((40, 24), |(w, h)| (w as usize, h as usize))
}

/// `\` then Enter is a newline, as in Claude Code: the backslash before the cursor gives way
/// to it, and nothing is sent. Whether there was one.
fn backslash_newline(ui: &mut Ui) -> bool {
    let mut chars: Vec<char> = ui.input.chars().collect();
    let at = ui.cursor.min(chars.len());
    if at == 0 || chars[at - 1] != '\\' {
        return false;
    }
    chars[at - 1] = '\n';
    ui.input = chars.into_iter().collect();
    true
}

/// The cursor a row up or down the prompt as it is wrapped, at the same column or the end of a
/// shorter row. Whether there was a row to go to.
fn row_step(ui: &mut Ui, up: bool, room: usize) -> bool {
    let chars: Vec<char> = ui.input.chars().collect();
    let (rows, (row, col)) = convo::soft_wrap(&chars, ui.cursor, room);
    let to = match (up, row) {
        (true, 0) => return false,
        (true, r) => r - 1,
        (false, r) if r + 1 >= rows.len() => return false,
        (false, r) => r + 1,
    };
    let (a, b) = rows[to];
    // Where `col` falls in that row; not past its end, nor onto the next row's first place
    // when the row was wrapped.
    let wrapped = to + 1 < rows.len() && chars.get(b) != Some(&'\n');
    let last = if wrapped {
        b.saturating_sub(1).max(a)
    } else {
        b
    };
    let mut used = 0;
    let mut at = a;
    while at < last && used + convo::cw(chars[at]) <= col {
        used += convo::cw(chars[at]);
        at += 1;
    }
    ui.cursor = at;
    true
}

/// A step back (Up) or on (Down) through what was sent. Going back from the prompt keeps what
/// was being typed; Down past the newest gives it back, the cursor at its start, as Claude
/// Code does. Up puts the cursor at an entry's end, Down at its start.
fn history_step(
    ui: &mut Ui,
    up: bool,
    history: &[String],
    at: &mut Option<usize>,
    draft: &mut Option<String>,
) {
    let n = history.len();
    let next = match (up, *at) {
        (true, None) if n > 0 => {
            *draft = Some(std::mem::take(&mut ui.input));
            Some(n - 1)
        }
        (true, Some(i)) => Some(i.saturating_sub(1)),
        (false, Some(i)) if i + 1 < n => Some(i + 1),
        (false, Some(_)) => {
            *at = None;
            ui.input = draft.take().unwrap_or_default();
            ui.cursor = 0;
            return;
        }
        // Down with nothing to step on to, or Up with nothing sent: the prompt as it is.
        _ => return,
    };
    *at = next;
    if let Some(i) = next {
        ui.input = history[i].clone();
        ui.cursor = if up { ui.input.chars().count() } else { 0 };
    }
}

/// A selection dragged over the screen, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    from: (u16, u16),
    to: (u16, u16),
    moved: bool,
}

impl Selection {
    /// Its first and last cell, in reading order, both in it.
    fn span(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.from.0 as usize, self.from.1 as usize);
        let b = (self.to.0 as usize, self.to.1 as usize);
        if a <= b { (a, b) } else { (b, a) }
    }

    /// The cells of `row` it covers, `[from, to)`.
    fn cols(&self, row: usize, width: usize) -> Option<(usize, usize)> {
        let ((r0, c0), (r1, c1)) = self.span();
        if row < r0 || row > r1 {
            return None;
        }
        let from = if row == r0 { c0 } else { 0 };
        let to = if row == r1 {
            (c1 + 1).min(width)
        } else {
            width
        };
        (from < to).then_some((from, to))
    }
}

/// Claude Code's selection colour in its dark theme (`selectionBg`).
const SELECTION_BG: u32 = 0x26_4f78;

/// The cells a selection covers, behind its colour.
fn highlight(lines: &mut [convo::Line], sel: Selection) {
    for (row, line) in lines.iter_mut().enumerate() {
        let width: usize = convo::plain(line).chars().map(convo::cw).sum();
        let Some((from, to)) = sel.cols(row, width.max(1)) else {
            continue;
        };
        let mut out: convo::Line = Vec::new();
        let mut at = 0;
        for (text, st) in line.iter() {
            for ch in text.chars() {
                let on = at >= from && at < to;
                let st = if on {
                    convo::St {
                        bg: Some(SELECTION_BG),
                        ..*st
                    }
                } else {
                    *st
                };
                match out.last_mut() {
                    Some((t, s)) if *s == st => t.push(ch),
                    _ => out.push((ch.to_string(), st)),
                }
                at += convo::cw(ch);
            }
        }
        *line = out;
    }
}

/// What a selection covers of rows of text: each row's cells in it, its trailing spaces
/// dropped, a line each.
fn selected_text(rows: &[String], sel: Selection) -> String {
    let ((r0, _), (r1, _)) = sel.span();
    let mut out = Vec::new();
    for (row, text) in rows.iter().enumerate().take(r1 + 1).skip(r0) {
        let width: usize = text.chars().map(convo::cw).sum();
        let Some((from, to)) = sel.cols(row, width) else {
            out.push(String::new());
            continue;
        };
        let mut at = 0;
        let mut piece = String::new();
        for ch in text.chars() {
            if at >= from && at < to {
                piece.push(ch);
            }
            at += convo::cw(ch);
        }
        out.push(piece.trim_end().to_string());
    }
    out.join("\n")
}

/// Claude Code's words for a selection copied: to the clipboard where there is one to set
/// (`termux-clipboard-set`), else sent as OSC 52, which the terminal may not honour.
fn selection_copied(text: &str) -> String {
    let n = text.chars().count();
    let chars = if n == 1 { "char" } else { "chars" };
    if std::env::var_os("TERMUX_VERSION").is_some() {
        format!("copied {n} {chars} to clipboard")
    } else {
        format!(
            "sent {n} {chars} via OSC 52 · if paste fails, hold shift while selecting for native copy"
        )
    }
}

/// The HTML entities claude's command descriptions carry (`&lt;duration&gt;`), as text.
fn entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// A prompt an interrupt caught unanswered, back in the input as Claude Code hands it back:
/// the input itself if empty, else ahead of what has been typed since.
fn give_back(ui: &mut Ui, text: &str) {
    if ui.input.is_empty() {
        ui.input = text.to_string();
    } else {
        ui.input = format!("{text}\n{}", ui.input);
    }
    ui.cursor = ui.input.chars().count();
}

/// The longest a paste goes into the prompt as itself, and how many line breaks: past either
/// it stands in as `[Pasted text #N +L lines]`. Claude Code 2.1.311's numbers, read from its
/// paste handler on 2026-11-02: 800 characters, and two breaks, fewer on a screen under 13 rows.
const PASTE_CHARS: usize = 800;
const PASTE_BREAKS: usize = 2;

/// Claude Code's stand-in for a paste: its number and how many line breaks it holds.
fn paste_label(id: u32, breaks: usize) -> String {
    if breaks == 0 {
        format!("[Pasted text #{id}]")
    } else {
        format!("[Pasted text #{id} +{breaks} lines]")
    }
}

/// A paste into the prompt, as Claude Code takes one: as itself when short, its tabs four
/// spaces; past [`PASTE_CHARS`] or [`PASTE_BREAKS`], as a numbered stand-in.
fn paste(ui: &mut Ui, text: &str, rows: usize) {
    let text = text.replace('\t', "    ");
    let breaks = text.matches('\n').count();
    let most = PASTE_BREAKS.min(rows.saturating_sub(10));
    let insert = if text.chars().count() > PASTE_CHARS || breaks > most {
        let id = ui.pastes.iter().map(|(n, _)| *n).max().unwrap_or(0) + 1;
        ui.pastes.push((id, text.clone()));
        paste_label(id, breaks)
    } else {
        text
    };
    let mut chars: Vec<char> = ui.input.chars().collect();
    let at = ui.cursor.min(chars.len());
    let n = insert.chars().count();
    chars.splice(at..at, insert.chars());
    ui.input = chars.into_iter().collect();
    ui.cursor = at + n;
}

/// The prompt as sent: every stand-in still in it replaced by its paste.
fn expand_pastes(input: &str, pastes: &[(u32, String)]) -> String {
    let mut out = input.to_string();
    for (id, text) in pastes {
        let label = paste_label(*id, text.matches('\n').count());
        out = out.replacen(&label, text, 1);
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn edit(ui: &mut Ui, k: Key) {
    let mut chars: Vec<char> = ui.input.chars().collect();
    let at = ui.cursor.min(chars.len());
    // Backspace right after a paste's stand-in takes the whole of it, and the paste.
    if matches!(k, Key::Char('\x7f') | Key::Char('\x08')) {
        let before: String = chars[..at].iter().collect();
        let hit = ui
            .pastes
            .iter()
            .position(|(id, text)| before.ends_with(&paste_label(*id, text.matches('\n').count())));
        if let Some(i) = hit {
            let (id, text) = ui.pastes.remove(i);
            let n = paste_label(id, text.matches('\n').count()).chars().count();
            chars.drain(at - n..at);
            ui.cursor = at - n;
            ui.input = chars.into_iter().collect();
            return;
        }
    }
    match k {
        Key::Char('\x7f') | Key::Char('\x08') if at > 0 => {
            chars.remove(at - 1);
            ui.cursor = at - 1;
        }
        Key::Delete if at < chars.len() => {
            chars.remove(at);
        }
        Key::Left => ui.cursor = at.saturating_sub(1),
        // A word at a time, as Claude Code's prompt: back to the start of a word, on to the
        // end of one, a word being letters, digits and `_`.
        Key::WordLeft => {
            let mut i = at;
            while i > 0 && !is_word(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word(chars[i - 1]) {
                i -= 1;
            }
            ui.cursor = i;
        }
        Key::WordRight => {
            let mut i = at;
            while i < chars.len() && !is_word(chars[i]) {
                i += 1;
            }
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            ui.cursor = i;
        }
        Key::Right => ui.cursor = (at + 1).min(chars.len()),
        Key::Home | Key::Char('\x01') => ui.cursor = 0,
        Key::End | Key::Char('\x05') => ui.cursor = chars.len(),
        Key::Char('\x15') => {
            chars.drain(..at);
            ui.cursor = 0;
        }
        Key::Char(ch) if !ch.is_control() => {
            chars.insert(at, ch);
            ui.cursor = at + 1;
        }
        _ => {}
    }
    ui.input = chars.into_iter().collect();
}

/// A key while a dialog is open; what it leaves open.
fn dialog_key(c: &Convo, d: Dialog, k: Key, out: &mut Vec<Value>) -> Option<Dialog> {
    let m = App::msg;
    let digit = |k: Key| match k {
        Key::Char(ch) if ch.is_ascii_digit() && ch != '0' => Some(ch as usize - '1' as usize),
        _ => None,
    };
    match d {
        Dialog::Perm { sel } => {
            let Some(Entry::Perm {
                request_id, always, ..
            }) = c.waiting()
            else {
                return None;
            };
            let opts = convo::perm_options(always);
            let answer = |i: usize| -> &'static str {
                match (i, opts.len()) {
                    (0, _) => "y",
                    (1, 3) => "a",
                    _ => "n",
                }
            };
            let pick = match k {
                Key::Up => {
                    return Some(Dialog::Perm {
                        sel: sel.saturating_sub(1),
                    });
                }
                Key::Down => {
                    return Some(Dialog::Perm {
                        sel: (sel + 1).min(opts.len() - 1),
                    });
                }
                Key::Enter => Some(sel),
                Key::Esc => Some(opts.len() - 1),
                k => digit(k).filter(|i| *i < opts.len()),
            };
            match pick {
                Some(i) => {
                    out.push(m(
                        "perm",
                        vec![
                            ("r", Value::string(request_id)),
                            ("a", Value::string(answer(i))),
                        ],
                    ));
                    None
                }
                None => Some(Dialog::Perm { sel }),
            }
        }
        Dialog::Ask {
            q,
            sel,
            mut answers,
            typing,
            mut ticked,
        } => {
            let Some(Entry::Ask {
                request_id,
                questions,
                ..
            }) = c.waiting()
            else {
                return None;
            };
            let question = questions.get(q)?;
            let opts = convo::ask_options(question);
            let n = opts.len();
            let decline = |out: &mut Vec<Value>| {
                out.push(m(
                    "ans",
                    vec![
                        ("r", Value::string(request_id)),
                        (
                            "x",
                            Value::string("The user wants to talk about this before answering."),
                        ),
                    ],
                ));
            };
            // Typing the answer for "Type something."
            if let Some(mut t) = typing {
                match k {
                    Key::Esc => {
                        return Some(Dialog::Ask {
                            q,
                            sel,
                            answers,
                            typing: None,
                            ticked,
                        });
                    }
                    Key::Enter => {
                        answers.push((question.question.clone(), t));
                        return next_question(request_id, questions, q, answers, out);
                    }
                    Key::Char('\x7f') | Key::Char('\x08') => {
                        t.pop();
                    }
                    Key::Char(ch) if !ch.is_control() => t.push(ch),
                    _ => {}
                }
                return Some(Dialog::Ask {
                    q,
                    sel,
                    answers,
                    typing: Some(t),
                    ticked,
                });
            }
            let chosen = match k {
                Key::Up => {
                    return Some(Dialog::Ask {
                        q,
                        sel: sel.saturating_sub(1),
                        answers,
                        typing: None,
                        ticked,
                    });
                }
                Key::Down => {
                    return Some(Dialog::Ask {
                        q,
                        sel: (sel + 1).min(n),
                        answers,
                        typing: None,
                        ticked,
                    });
                }
                Key::Esc => {
                    decline(out);
                    return None;
                }
                Key::Enter => Some(sel),
                k => digit(k).filter(|i| *i <= n),
            };
            let Some(i) = chosen else {
                return Some(Dialog::Ask {
                    q,
                    sel,
                    answers,
                    typing: None,
                    ticked,
                });
            };
            let own = question.options.len();
            if i == n {
                // Chat about this.
                decline(out);
                return None;
            }
            if i == own {
                return Some(Dialog::Ask {
                    q,
                    sel: i,
                    answers,
                    typing: Some(String::new()),
                    ticked,
                });
            }
            if question.multi && i < own {
                if let Some(t) = ticked.get_mut(i) {
                    *t = !*t;
                }
                return Some(Dialog::Ask {
                    q,
                    sel: i,
                    answers,
                    typing: None,
                    ticked,
                });
            }
            let answer = if question.multi {
                question
                    .options
                    .iter()
                    .zip(&ticked)
                    .filter(|(_, t)| **t)
                    .map(|(o, _)| o.0.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                question.options[i].0.clone()
            };
            answers.push((question.question.clone(), answer));
            next_question(request_id, questions, q, answers, out)
        }
        Dialog::Model { sel, list } => {
            let len = list.as_ref().map_or(0, Vec::len);
            match k {
                Key::Up => Some(Dialog::Model {
                    sel: sel.saturating_sub(1),
                    list,
                }),
                Key::Down => Some(Dialog::Model {
                    sel: (sel + 1).min(len.saturating_sub(1)),
                    list,
                }),
                Key::Esc => None,
                Key::Enter => {
                    if let Some((v, _)) = list.as_ref().and_then(|l| l.get(sel)) {
                        out.push(m("model", vec![("m", Value::string(v))]));
                    }
                    None
                }
                _ => Some(Dialog::Model { sel, list }),
            }
        }
        Dialog::Effort { sel } => match k {
            Key::Left => Some(Dialog::Effort {
                sel: sel.saturating_sub(1),
            }),
            Key::Right => Some(Dialog::Effort {
                sel: (sel + 1).min(convo::EFFORTS.len() - 1),
            }),
            Key::Esc => None,
            Key::Enter => {
                out.push(m("effort", vec![("e", Value::string(convo::EFFORTS[sel]))]));
                None
            }
            _ => Some(Dialog::Effort { sel }),
        },
        // Taken by `conv_key`, which can open an agent's view from it.
        d @ Dialog::Tasks(_) => Some(d),
        // Taken by `conv_key` before it gets here, since what they do is the client's.
        d @ (Dialog::Copy { .. } | Dialog::Btw { .. }) => Some(d),
    }
}

/// The next of several questions, or every answer sent once the last is answered.
fn next_question(
    request_id: &str,
    questions: &[convo::Question],
    q: usize,
    answers: Vec<(String, String)>,
    out: &mut Vec<Value>,
) -> Option<Dialog> {
    if q + 1 < questions.len() {
        let ticked = vec![false; questions[q + 1].options.len()];
        return Some(Dialog::Ask {
            q: q + 1,
            sel: 0,
            answers,
            typing: None,
            ticked,
        });
    }
    let mut a = Value::obj();
    for (question, answer) in answers {
        a.set(&question, Value::string(answer));
    }
    out.push(App::msg(
        "ans",
        vec![("r", Value::string(request_id)), ("a", a)],
    ));
    None
}

/// Lines a wheel report scrolls. Termux sends one report for every line a finger travels,
/// so a report is a line there: three ran a swipe three times as far as the finger went.
/// Windows Terminal sends one a notch, and a notch is three lines in Claude Code; it sets
/// `WT_SESSION`, and passes it into WSL. Anything else is taken as Termux, the phone, since
/// a step too small is slow while a step too big jumps (owner, 2026-11-01).
fn wheel_step(env: impl Fn(&str) -> Option<String>) -> usize {
    let set = |k| env(k).is_some_and(|v| !v.is_empty());
    if set("WT_SESSION") && !set("TERMUX_VERSION") {
        3
    } else {
        1
    }
}

/// A running slot that no holder drives is a terminal's: listed dim, as `term: ` (R9).
fn is_terminal(r: &Row) -> bool {
    let running = !r.offloaded && !r.closed && !r.archived;
    let session = matches!(r.key, RowKey::Slot(_) | RowKey::Socket(_));
    session && running && !r.remote
}

fn terminal_mark(mut r: Row) -> Row {
    if is_terminal(&r) {
        r.title = format!("term: {}", r.title);
        r.attached = true;
    }
    r
}

/// `RAM 0.9G/2G`, as R1's footer has it.
fn took(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// The local time as Claude Code writes it, `1:12 AM`, from `date`, which knows the phone's
/// zone where this binary cannot.
fn clock_time() -> String {
    let out = Command::new("date").arg("+%I:%M %p").output();
    match out {
        Ok(o) if o.status.success() => {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            t.strip_prefix('0').map(str::to_string).unwrap_or(t)
        }
        _ => String::new(),
    }
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `/copy`: the terminal's clipboard by OSC 52, and Termux's own when it is there.
fn copy_seq(text: &str) -> String {
    if std::env::var_os("TERMUX_VERSION").is_some() {
        if let Ok(mut child) = Command::new("termux-clipboard-set")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut i) = child.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn sgr(st: &convo::St) -> String {
    let mut codes: Vec<String> = Vec::new();
    for (on, code) in [
        (st.bold, "1"),
        (st.italic, "3"),
        (st.underline, "4"),
        (st.strike, "9"),
    ] {
        if on {
            codes.push(code.into());
        }
    }
    if let Some(c) = st.fg {
        codes.push(format!("38;2;{};{};{}", c >> 16, (c >> 8) & 0xff, c & 0xff));
    }
    if let Some(c) = st.bg {
        codes.push(format!("48;2;{};{};{}", c >> 16, (c >> 8) & 0xff, c & 0xff));
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn ansi(l: &convo::Line) -> String {
    let mut out = String::from("\x1b[2K");
    for (text, st) in l {
        let s = sgr(st);
        if s.is_empty() {
            out.push_str(text);
        } else {
            out.push_str(&s);
            out.push_str(text);
            out.push_str("\x1b[0m");
        }
    }
    out
}

impl App {
    /// The rows of the screen now, as bytes per row, and where the cursor goes.
    fn frame(&mut self, w: u16, h: u16, now: Instant) -> (Vec<String>, Option<(usize, usize)>) {
        match &mut self.screen {
            Screen::Home => {
                self.menu.resize(w, h);
                if !self.greeted {
                    let n = now.saturating_duration_since(self.started).as_millis()
                        / SPIN_EVERY.as_millis();
                    self.menu.set_busy(Some(connecting(n as usize)));
                }
                if let Some((since, row, what)) = &self.working {
                    let n =
                        now.saturating_duration_since(*since).as_millis() / SPIN_EVERY.as_millis();
                    self.menu.set_busy(Some(Busy {
                        frame: n as usize,
                        row: Some(*row),
                        what: what.clone(),
                        loading: false,
                    }));
                }
                // A link that went down after the hello: said on the load line, turning.
                self.menu.reconnecting = self.down_since.filter(|_| self.greeted).map(|since| {
                    let gone = now.saturating_duration_since(since);
                    let frame = (gone.as_millis() / SPIN_EVERY.as_millis()) as usize;
                    format!(
                        "{} Reconnecting… ({}s)",
                        crate::ui::SPINNER[frame % crate::ui::SPINNER.len()],
                        gone.as_secs()
                    )
                });
                match crate::render::render(&self.menu.view()) {
                    Ok(f) => ((0..f.lines.len()).map(|i| f.ansi_line(i)).collect(), None),
                    Err(_) => (vec!["\x1b[2Ktoo narrow".into()], None),
                }
            }
            Screen::Conv { c, ui, .. } => {
                let mut f = convo::draw(c, ui, w as usize, h as usize, now);
                if let Some(sel) = self.select.filter(|s| s.moved) {
                    highlight(&mut f.lines, sel);
                }
                (f.lines.iter().map(ansi).collect(), f.cursor)
            }
        }
    }

    fn paint(&mut self, term: &mut RawTerminal, now: Instant) -> io::Result<()> {
        let (w, h) = term::size().unwrap_or((40, 24));
        let (rows, cursor) = self.frame(w, h, now);
        let mut out = String::new();
        if let Some(t) = self.clip.take() {
            out.push_str(&copy_seq(&t));
        }
        if self.drawn.len() != rows.len() {
            self.drawn = vec![String::from("\u{0}"); rows.len()];
        }
        for (i, r) in rows.iter().enumerate() {
            if self.drawn[i] != *r {
                out.push_str(&format!("\x1b[{};1H{r}", i + 1));
                self.drawn[i] = r.clone();
            }
        }
        match cursor {
            Some((r, c)) => out.push_str(&format!("\x1b[{};{}H\x1b[?25h", r + 1, c + 1)),
            None => out.push_str("\x1b[?25l"),
        }
        if !out.is_empty() {
            term.write_all(out.as_bytes())?;
        }
        Ok(())
    }

    /// How long something sent may go unanswered before the link is thought dead: longer while
    /// a conversation opens.
    fn ack_wait(&self) -> Duration {
        if matches!(self.screen, Screen::Conv { opening: true, .. }) {
            OPEN_WAIT
        } else {
            ACK_WAIT
        }
    }

    /// When the screen next changes by itself: the spinner, the seconds while down, a retry.
    fn next_wake(&self, now: Instant) -> Option<Duration> {
        let mut at: Option<Instant> = self.retry_at;
        let mut soon = |t: Instant| at = Some(at.map_or(t, |a| a.min(t)));
        if let Screen::Conv { c, ui, .. } = &self.screen {
            if matches!(c.state.as_str(), "working" | "compacting") && ui.dialog.is_none() {
                soon(now + SPIN_EVERY);
            }
            if matches!(ui.link, Link::Down { .. }) {
                soon(now + SPIN_EVERY);
            }
            if !c.pending.is_empty() {
                soon(now + SPIN_EVERY);
            }
        }
        if matches!(self.screen, Screen::Home)
            && (!self.greeted || self.down_since.is_some() || self.working.is_some())
        {
            soon(now + SPIN_EVERY);
        }
        if let Some(a) = self.awaiting {
            soon(a + self.ack_wait());
        }
        if self.output_wanted().is_some() {
            soon(self.output_for.as_ref().map_or(now, |(_, at)| *at));
        }
        at.map(|a| a.saturating_duration_since(now))
    }
}

/// `claude-remote <ssh name>`.
/// Mockup 10's spinner and word, before the relay's hello.
fn connecting(frame: usize) -> Busy {
    Busy {
        frame,
        row: None,
        what: "connecting".to_string(),
        loading: true,
    }
}

impl App {
    /// A client for the ssh name `name`, its home empty until the relay says hello, a wheel
    /// report scrolling `wheel` lines.
    fn new(name: &str, w: u16, h: u16, wheel: usize) -> App {
        let hub = Header {
            host: name.to_string(),
            memory: None,
            load: None,
        };
        let mut app = App {
            name: name.to_string(),
            wire: None,
            epoch: 0,
            down_since: None,
            retry_at: None,
            backoff: Duration::from_secs(1),
            last_heard: Instant::now(),
            awaiting: None,
            hosts: Vec::new(),
            places: HashMap::new(),
            rows: HashMap::new(),
            hub: hub.clone(),
            menu: Menu::hub(w, h, hub, Vec::new(), Vec::new()),
            menu_fresh: true,
            screen: Screen::Home,
            models: None,
            history: Vec::new(),
            history_at: None,
            output_for: None,
            draft: None,
            drawn: Vec::new(),
            clip: None,
            quit: false,
            started: Instant::now(),
            greeted: false,
            wheel,
            working: None,
            pasting: None,
            catalog: Vec::new(),
            select: None,
            held: Vec::new(),
            open_msg: None,
        };
        app.menu.remote = true;
        app
    }
}

pub fn run(name: &str) -> Result<(), String> {
    let (w, h) = term::size().ok_or("not a terminal")?;
    let (tx, rx): (Sender<Heard>, Receiver<Heard>) = mpsc::channel();
    let mut app = App::new(name, w, h, wheel_step(|k| std::env::var(k).ok()));
    let mut term = RawTerminal::enter().map_err(|e| format!("cannot set up the terminal: {e}"))?;
    // And bracketed paste, so a paste arrives as one (`Key::PasteStart`), as in Claude Code.
    // And motion while a button is held (`?1002`), for a selection dragged over the
    // conversation; the menu's own terminal never asks for it.
    term.write_all(b"\x1b[2J\x1b[?2004h\x1b[?1002h")
        .map_err(|e| e.to_string())?;
    app.dial(Instant::now(), &tx);
    let mut hosts_seen: Vec<String> = Vec::new();
    loop {
        let now = Instant::now();
        if app.hosts != hosts_seen {
            hosts_seen = app.hosts.clone();
            let (w, h) = term::size().unwrap_or((40, 24));
            app.rebuild_menu(w, h);
        }
        app.want_older();
        app.want_output(now);
        if app.paint(&mut term, now).is_err() {
            break;
        }
        if app.quit {
            break;
        }
        let input = term.wait(app.next_wake(now));
        let now = Instant::now();
        match input {
            Ok(Input::Keys(keys)) => {
                for k in keys {
                    if k == Key::Resize {
                        app.drawn.clear();
                        let _ = term.write_all(b"\x1b[2J");
                        continue;
                    }
                    app.key(k, now);
                }
            }
            Ok(Input::Timeout) | Ok(Input::Woken) => {}
            Err(_) => break,
        }
        while let Ok(h) = rx.try_recv() {
            match h {
                Heard::Line(e, v) if e == app.epoch => app.heard(v, now),
                Heard::Gone(e, why) if e == app.epoch => {
                    app.link_down(now);
                    // ssh's own last word, when it said one: why the link is down.
                    if !why.is_empty() {
                        match &mut app.screen {
                            Screen::Home => app.menu.set_status(Some(why)),
                            Screen::Conv { ui, .. } => ui.status = Some(why),
                        }
                    }
                }
                _ => {}
            }
        }
        if app.awaiting.is_some_and(|a| now >= a + app.ack_wait()) {
            app.link_down(now);
        }
        if app.retry_at.is_some_and(|r| now >= r) {
            app.dial(now, &tx);
        }
    }
    if let Some(mut w) = app.wire.take() {
        let _ = w.child.kill();
        let _ = w.child.wait();
    }
    let _ = term.write_all(b"\x1b[?1002l\x1b[?2004l");
    drop(term);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn copy_is_copy_though_claudes_list_has_only_ux_copy() {
        // headless claude lists `design:ux-copy` and no `copy`: the phone's own comes first,
        // and a name typed whole is kept on Enter (`conv_key`).
        let mut ui = Ui::new();
        ui.catalog = vec![convo::Command {
            name: "design:ux-copy".into(),
            about: "Write or review UX copy".into(),
            hint: String::new(),
        }];
        ui.input = "/copy".into();
        let names: Vec<String> = convo::suggestions(&ui)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["copy", "design:ux-copy"]);
        for cmd in [
            "model", "effort", "rename", "copy", "exit", "btw", "expand", "home",
        ] {
            ui.input = format!("/{cmd}");
            assert!(
                convo::suggestions(&ui).iter().any(|c| c.name == cmd),
                "/{cmd} is in the list"
            );
        }
    }

    #[test]
    fn a_drag_selects_what_it_covers_and_lights_it() {
        let rows: Vec<String> = [
            "● the quick brown fox",
            "  jumps over",
            "",
            "  the lazy dog  ",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let sel = |from: (u16, u16), to: (u16, u16)| Selection {
            from,
            to,
            moved: true,
        };
        assert_eq!(selected_text(&rows, sel((0, 2), (0, 10))), "the quick");
        // Backwards is the same; across rows, each row from its start; trailing spaces go.
        assert_eq!(
            selected_text(&rows, sel((3, 9), (0, 12))),
            "brown fox\n  jumps over\n\n  the lazy"
        );
        let mut lines: Vec<convo::Line> = rows
            .iter()
            .map(|r| vec![(r.clone(), convo::PLAIN)])
            .collect();
        highlight(&mut lines, sel((0, 2), (0, 4)));
        assert_eq!(lines[0][0], ("● ".to_string(), convo::PLAIN));
        assert_eq!(lines[0][1].0, "the");
        assert_eq!(lines[0][1].1.bg, Some(SELECTION_BG));
        assert_eq!(lines[0][2].1.bg, None);
        assert_eq!(lines[1], vec![("  jumps over".to_string(), convo::PLAIN)]);
        assert!(selection_copied("abc").contains("3 chars"));
        assert!(selection_copied("a").contains("1 char "));
    }

    #[test]
    fn scrolled_back_the_view_holds_while_lines_arrive_and_follows_at_the_bottom() {
        let mut c = Convo::new("h", "s", "r");
        let lines: Vec<String> = (1..=60).map(|n| n.to_string()).collect();
        let add = format!(
            r#"{{"t":"add","id":2,"k":"text","x":"{}"}}"#,
            lines.join("\\n")
        );
        for o in [r#"{"t":"add","id":1,"k":"user","x":"count"}"#, add.as_str()] {
            c.apply(&parse(o).unwrap(), Instant::now());
        }
        let mut ui = Ui::new();
        ui.scroll = 10;
        let shown = |c: &Convo, ui: &Ui| convo::draw(c, ui, 40, 24, Instant::now()).lines;
        let before_view = shown(&c, &ui);
        let before = convo::max_scroll(&c, &ui, 40, 24);
        c.apply(
            &parse(r#"{"t":"add","id":3,"k":"text","x":"more\nand more\nand more"}"#).unwrap(),
            Instant::now(),
        );
        ui.scroll = hold_scroll(ui.scroll, before, convo::max_scroll(&c, &ui, 40, 24));
        assert_eq!(
            shown(&c, &ui),
            before_view,
            "the same lines, though four arrived"
        );
        // At the bottom it follows.
        let mut ui = Ui::new();
        let bottom = shown(&c, &ui);
        c.apply(
            &parse(r#"{"t":"add","id":4,"k":"text","x":"newest"}"#).unwrap(),
            Instant::now(),
        );
        ui.scroll = 0;
        assert_ne!(shown(&c, &ui), bottom);
        assert_eq!(
            hold_scroll(3, 10, 0),
            0,
            "nothing left to scroll: following"
        );
    }

    #[test]
    fn ctrl_or_alt_with_an_arrow_goes_a_word_as_claude_codes_prompt_does() {
        let mut d = term::Decoder::new();
        assert_eq!(
            d.feed(b"\x1b[1;5D\x1b[1;5C\x1b[1;3D\x1b[1;2D\x1b[D"),
            [
                Key::WordLeft,
                Key::WordRight,
                Key::WordLeft,
                Key::Left,
                Key::Left
            ]
        );
        // As Claude Code 2.1.311 moved on `alpha beta  gamma` on 2026-11-02.
        let mut ui = Ui::new();
        ui.input = "alpha beta  gamma".into();
        ui.cursor = 17;
        let at = |ui: &mut Ui, k: Key| {
            edit(ui, k);
            ui.cursor
        };
        assert_eq!(at(&mut ui, Key::WordLeft), 12);
        assert_eq!(at(&mut ui, Key::WordLeft), 6);
        assert_eq!(at(&mut ui, Key::WordLeft), 0);
        assert_eq!(at(&mut ui, Key::WordLeft), 0);
        assert_eq!(at(&mut ui, Key::WordRight), 5);
        assert_eq!(at(&mut ui, Key::WordRight), 10);
        assert_eq!(at(&mut ui, Key::WordRight), 17);
        assert_eq!(at(&mut ui, Key::WordRight), 17);
    }

    #[test]
    fn up_and_down_keep_what_was_being_typed_as_claude_code_does() {
        let history = vec!["Say one".to_string(), "Say two".to_string()];
        let (mut at, mut draft) = (None, None);
        let mut ui = Ui::new();
        ui.input = "my draft".into();
        ui.cursor = 8;
        let mut key = |ui: &mut Ui, up: bool| {
            if !row_step(ui, up, 38) {
                history_step(ui, up, &history, &mut at, &mut draft);
            }
            (ui.input.clone(), ui.cursor)
        };
        // As Claude Code 2.1.311 did on 2026-11-02, a one-line draft, the cursor at its end.
        assert_eq!(
            key(&mut ui, true),
            ("Say two".into(), 7),
            "Up: the newest, cursor at end"
        );
        assert_eq!(key(&mut ui, true), ("Say one".into(), 7));
        assert_eq!(
            key(&mut ui, true),
            ("Say one".into(), 7),
            "the oldest stays"
        );
        assert_eq!(
            key(&mut ui, false),
            ("Say two".into(), 0),
            "Down: cursor at the start"
        );
        assert_eq!(
            key(&mut ui, false),
            ("my draft".into(), 0),
            "past the newest: the draft"
        );
        assert_eq!(
            key(&mut ui, false),
            ("my draft".into(), 0),
            "and Down again keeps it"
        );
        // A draft of three rows: Up climbs them first, then goes back; Down returns it whole.
        ui.input = "a\nbb\nccc".into();
        ui.cursor = ui.input.chars().count();
        assert_eq!(key(&mut ui, true).1, 4, "row two, at the end of `bb`");
        assert_eq!(key(&mut ui, true).1, 1, "row one, at the end of `a`");
        assert_eq!(key(&mut ui, true).0, "Say two", "then what was sent");
        assert_eq!(key(&mut ui, false), ("a\nbb\nccc".into(), 0));
        assert_eq!(
            key(&mut ui, false).1,
            2,
            "Down moves through its rows: row two"
        );
        // Nothing sent yet: Up leaves the prompt as it is.
        let (mut at, mut draft) = (None, None);
        let mut ui = Ui::new();
        ui.input = "keep me".into();
        history_step(&mut ui, true, &[], &mut at, &mut draft);
        history_step(&mut ui, false, &[], &mut at, &mut draft);
        assert_eq!(ui.input, "keep me");
    }

    #[test]
    fn a_paste_goes_in_as_itself_or_as_claude_codes_stand_in() {
        let mut ui = Ui::new();
        // Two line breaks or fewer, as typed: Claude Code's three-line paste stays inline.
        paste(&mut ui, "first\nsecond\nthird", 32);
        assert_eq!(ui.input, "first\nsecond\nthird");
        assert!(ui.pastes.is_empty());
        // Three breaks, or past 800 characters, a stand-in at the cursor.
        let mut ui = Ui::new();
        ui.input = "look: ".into();
        ui.cursor = 6;
        paste(&mut ui, "a\nb\nc\nd", 32);
        assert_eq!(ui.input, "look: [Pasted text #1 +3 lines]");
        paste(&mut ui, &"x".repeat(801), 32);
        assert_eq!(ui.input, "look: [Pasted text #1 +3 lines][Pasted text #2]");
        // On a short screen fewer breaks are too many: one at 11 rows, none at 10.
        let mut short = Ui::new();
        paste(&mut short, "a\nb", 11);
        assert_eq!(short.input, "a\nb");
        let mut shorter = Ui::new();
        paste(&mut shorter, "a\nb", 10);
        assert_eq!(shorter.input, "[Pasted text #1 +1 lines]");
        // Sent, each is itself again.
        assert_eq!(
            expand_pastes(&ui.input, &ui.pastes),
            format!("look: a\nb\nc\nd{}", "x".repeat(801))
        );
        // Backspace after one takes it whole, and the paste.
        edit(&mut ui, Key::Char('\x7f'));
        assert_eq!(ui.input, "look: [Pasted text #1 +3 lines]");
        assert_eq!(ui.pastes.len(), 1);
        edit(&mut ui, Key::Char('\x7f'));
        assert_eq!(ui.input, "look: ");
        edit(&mut ui, Key::Char('\x7f'));
        assert_eq!(ui.input, "look:", "then a character at a time");
        // Tabs are four spaces, as Claude Code pastes them.
        let mut ui = Ui::new();
        paste(&mut ui, "a\tb", 32);
        assert_eq!(ui.input, "a    b");
    }

    #[test]
    fn a_backslash_then_enter_is_a_newline_and_sends_nothing() {
        let mut ui = Ui::new();
        ui.input = "one\\".into();
        ui.cursor = 4;
        assert!(backslash_newline(&mut ui));
        assert_eq!((ui.input.as_str(), ui.cursor), ("one\n", 4));
        for k in "two".chars() {
            edit(&mut ui, Key::Char(k));
        }
        assert_eq!(ui.input, "one\ntwo");
        assert!(!backslash_newline(&mut ui), "no backslash: Enter sends");
        // Only the one right before the cursor counts.
        ui.input = "a\\b".into();
        ui.cursor = 3;
        assert!(!backslash_newline(&mut ui));
    }

    #[test]
    fn the_phones_home_offers_c_and_not_what_needs_the_doors_terminal() {
        let hub = Header {
            host: "zero".into(),
            memory: None,
            load: None,
        };
        let mut place = Place::new("claude-sessions");
        place.reach = Reach::Up;
        let rows = vec![row(RowKey::Slot("claude-3".into()), true, false)];
        let mut m = Menu::hub(40, 24, hub, vec![place], rows);
        m.remote = true;
        let lines = |m: &Menu| {
            let f = crate::render::render(&m.view()).unwrap();
            f.plain().lines().map(str::to_string).collect::<Vec<_>>()
        };
        let home = lines(&m);
        let r9 = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/mockups.md"))
            .unwrap();
        let at = r9.find("### Mockup R9 ").unwrap();
        let want = r9[at..]
            .lines()
            .find(|l| l.starts_with("Enter open"))
            .unwrap();
        assert!(home.iter().any(|l| l == want), "R9's hint line: {home:?}");
        m.key(Key::Char('?'));
        let keys = lines(&m);
        assert!(keys.iter().any(|l| l.starts_with("c ")), "{keys:?}");
        assert!(
            !keys
                .iter()
                .any(|l| l.starts_with("p ") || l.starts_with("s ")),
            "no shell, no pi: {keys:?}"
        );
    }

    #[test]
    fn a_dropped_link_on_the_home_is_said_on_its_load_line() {
        let hub = Header {
            host: "zero".into(),
            memory: Some((1 << 30, 3 << 30)),
            load: None,
        };
        let draw = |w: u16, down: Option<&str>| {
            let mut m = Menu::hub(w, 24, hub.clone(), Vec::new(), Vec::new());
            m.remote = true;
            m.reconnecting = down.map(str::to_string);
            let f = crate::render::render(&m.view()).unwrap();
            f.plain().lines().last().unwrap_or("").to_string()
        };
        assert_eq!(draw(80, None), "RAM: 1.0G / 3.0G, Load: ?, zero");
        assert_eq!(
            draw(80, Some("⠋ Reconnecting… (8s)")),
            "RAM: 1.0G / 3.0G, Load: ?, zero · ⠋ Reconnecting… (8s)"
        );
        assert_eq!(
            draw(40, Some("⠋ Reconnecting… (8s)")),
            "⠋ Reconnecting… (8s)"
        );
    }

    #[test]
    fn before_the_relays_hello_the_home_says_connecting_not_empty() {
        let hub = Header {
            host: "zero".into(),
            memory: None,
            load: None,
        };
        let mut m = Menu::hub(40, 24, hub, Vec::new(), Vec::new());
        m.remote = true;
        let plain = |m: &Menu| {
            let f = crate::render::render(&m.view()).unwrap();
            f.plain().lines().map(str::to_string).collect::<Vec<_>>()
        };
        assert!(
            plain(&m).iter().any(|l| l.contains("No claude session")),
            "what it drew before: {:?}",
            plain(&m)
        );
        m.set_busy(Some(connecting(0)));
        let got = plain(&m);
        assert!(got.iter().any(|l| l == "⠋ connecting"), "{got:?}");
        assert!(
            !got.iter().any(|l| l.contains("No claude session")),
            "{got:?}"
        );
    }

    #[test]
    fn a_wheel_report_is_three_lines_under_windows_terminal_and_one_elsewhere() {
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                set.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(wheel_step(env(&[])), 1, "nothing said: the phone");
        assert_eq!(wheel_step(env(&[("TERMUX_VERSION", "0.118.1")])), 1);
        assert_eq!(wheel_step(env(&[("WT_SESSION", "8c0c…")])), 3);
        assert_eq!(wheel_step(env(&[("WT_SESSION", "")])), 1, "set but empty");
        assert_eq!(
            wheel_step(env(&[("WT_SESSION", "x"), ("TERMUX_VERSION", "0.118.1")])),
            1,
            "Termux wins: its swipes are a line a report"
        );
    }

    fn row(key: RowKey, remote: bool, offloaded: bool) -> Row {
        Row {
            host: "claude-sessions".into(),
            key,
            wants_you: false,
            busy: false,
            unread: false,
            attached: false,
            offloaded,
            closed: false,
            archived: false,
            title: "immich upgrade".into(),
            age: "3m".into(),
            at: 1,
            nested: false,
            remote,
        }
    }

    #[test]
    fn a_running_session_no_holder_drives_is_marked_as_a_terminals_and_dim() {
        let t = terminal_mark(row(RowKey::Slot("pi-1".into()), false, false));
        assert_eq!(t.title, "term: immich upgrade");
        assert!(t.attached, "dim, as the menu draws an attached row");
        let r = terminal_mark(row(RowKey::Slot("claude-3".into()), true, false));
        assert_eq!(
            r.title, "immich upgrade",
            "the holder's is the phone's to open"
        );
        let off = terminal_mark(row(RowKey::Slot("claude-4".into()), false, true));
        assert_eq!(
            off.title, "immich upgrade",
            "offloaded: it resumes under the holder"
        );
        let conv = RowKey::Conversation {
            id: "c".into(),
            cwd: "/w".into(),
            harness: crate::harness::Harness::Claude,
        };
        assert!(!is_terminal(&row(conv, false, false)));
    }

    fn convo_with(ops: &[&str]) -> Convo {
        let mut c = Convo::new("h", "claude-3", "r");
        for o in ops {
            c.apply(&parse(o).unwrap(), Instant::now());
        }
        c
    }

    fn line(v: &Value) -> String {
        json::to_line(v)
    }

    #[test]
    fn a_permission_prompt_opens_its_dialog_and_each_way_out_answers_it() {
        let c = convo_with(&[
            r#"{"t":"add","id":1,"k":"perm","r":"p1","n":"Bash","a":"touch x","d":"","al":"Yes, and don't ask again for touch commands in this project"}"#,
        ]);
        let mut ui = Ui::new();
        follow_prompts(&c, &mut ui);
        assert_eq!(ui.dialog, Some(Dialog::Perm { sel: 0 }));
        for (k, want) in [
            (Key::Enter, "y"),
            (Key::Char('2'), "a"),
            (Key::Char('3'), "n"),
            (Key::Esc, "n"),
        ] {
            let mut out = Vec::new();
            assert_eq!(dialog_key(&c, Dialog::Perm { sel: 0 }, k, &mut out), None);
            assert_eq!(
                line(&out[0]),
                format!(r#"{{"a":"{want}","r":"p1","t":"perm"}}"#),
                "{k:?}"
            );
        }
        let mut out = Vec::new();
        assert_eq!(
            dialog_key(&c, Dialog::Perm { sel: 0 }, Key::Down, &mut out),
            Some(Dialog::Perm { sel: 1 })
        );
        assert!(out.is_empty());
        // Answered elsewhere: the dialog goes.
        let mut c = c;
        c.apply(
            &parse(r#"{"t":"done","id":1,"x":"allowed"}"#).unwrap(),
            Instant::now(),
        );
        follow_prompts(&c, &mut ui);
        assert_eq!(ui.dialog, None);
    }

    #[test]
    fn questions_are_answered_in_turn_typed_ticked_or_declined() {
        let c = convo_with(&[
            r#"{"t":"add","id":1,"k":"ask","r":"q1","q":[{"q":"Colour?","h":"C","m":false,"o":[["Red",""],["Blue",""]]},{"q":"Sizes?","h":"S","m":true,"o":[["S",""],["M",""],["L",""]]}]}"#,
        ]);
        let mut ui = Ui::new();
        follow_prompts(&c, &mut ui);
        let mut d = ui.dialog.clone().unwrap();
        let mut out = Vec::new();
        // Blue for the first; S and L ticked, then Submit, for the second.
        for k in [
            Key::Char('2'),
            Key::Char('1'),
            Key::Char('3'),
            Key::Char('5'),
        ] {
            d = dialog_key(&c, d, k, &mut out).unwrap_or(Dialog::Effort { sel: 0 });
        }
        assert_eq!(
            line(&out[0]),
            r#"{"a":{"Colour?":"Blue","Sizes?":"S, L"},"r":"q1","t":"ans"}"#
        );
        // "Type something." takes what is typed.
        let mut out = Vec::new();
        let mut d = ui.dialog.clone().unwrap();
        for k in [
            Key::Char('3'),
            Key::Char('t'),
            Key::Char('e'),
            Key::Char('a'),
            Key::Enter,
        ] {
            d = dialog_key(&c, d, k, &mut out).unwrap();
        }
        assert!(matches!(d, Dialog::Ask { q: 1, .. }));
        // Esc declines the lot.
        let mut out = Vec::new();
        assert_eq!(
            dialog_key(&c, ui.dialog.clone().unwrap(), Key::Esc, &mut out),
            None
        );
        assert!(line(&out[0]).contains(r#""t":"ans""#) && !line(&out[0]).contains(r#""a":"#));
    }

    #[test]
    fn slash_commands_are_the_clients_and_anything_else_is_claudes() {
        let mut c = convo_with(&[
            r#"{"t":"meta","sid":"s","m":"claude-opus-5-5","pm":"default","ti":"","e":"","cwd":"","ver":""}"#,
        ]);
        let mut ui = Ui::new();
        let sent = |s: Submit| match s {
            Submit::Send(v) => v.iter().map(line).collect::<Vec<_>>(),
            _ => vec![],
        };
        assert_eq!(
            sent(submit(&mut c, &mut ui, "/auto", &None)),
            [r#"{"m":"acceptEdits","t":"mode"}"#]
        );
        assert_eq!(
            sent(submit(&mut c, &mut ui, "/model", &None)),
            [r#"{"t":"models"}"#],
            "the list is asked for once"
        );
        assert!(matches!(ui.dialog, Some(Dialog::Model { list: None, .. })));
        assert_eq!(
            sent(submit(&mut c, &mut ui, "/effort high", &None)),
            [r#"{"e":"high","t":"effort"}"#]
        );
        assert_eq!(
            sent(submit(&mut c, &mut ui, "/rename fix the door", &None)),
            [r#"{"t":"rename","x":"fix the door"}"#]
        );
        // claude's own, with the uuid the phone keeps it pending by.
        let said = sent(submit(&mut c, &mut ui, "/compact", &None));
        let uuid = &c.pending.last().unwrap().uuid;
        assert_eq!(
            said,
            [format!(r#"{{"t":"say","u":"{uuid}","x":"/compact"}}"#)]
        );
        assert!(matches!(
            submit(&mut c, &mut ui, "/home", &None),
            Submit::Home
        ));
        assert!(matches!(
            submit(&mut c, &mut ui, "/copy", &None),
            Submit::Nothing
        ));
        c.apply(
            &parse(r#"{"t":"add","id":1,"k":"text","x":"hello"}"#).unwrap(),
            Instant::now(),
        );
        assert!(matches!(submit(&mut c, &mut ui, "/copy", &None), Submit::Copy(t) if t == "hello"));
    }

    /// A client with a conversation open on `ops`, as if the relay had sent them.
    fn app_in(ops: &[&str]) -> App {
        let mut app = App::new("dev", 40, 24, 1);
        app.screen = Screen::Conv {
            c: Box::new(convo_with(ops)),
            ui: Box::new(Ui::new()),
            opening: false,
        };
        app
    }

    fn type_in(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.key(Key::Char(ch), Instant::now());
        }
        app.key(Key::Enter, Instant::now());
    }

    const TASKS: &str = r#"{"t":"tasks","l":[{"i":"b1","k":"shell","d":"Ticks","c":"for i in 1 2; do echo $i; done","st":"running","t0":1000},{"i":"m1","k":"monitor","d":"Watch it","c":"tail -f x","st":"running"}]}"#;

    #[test]
    fn down_on_an_empty_prompt_focuses_the_pill_and_enter_opens_the_tasks_view() {
        let mut app = app_in(&[TASKS]);
        app.key(Key::Down, Instant::now());
        let Screen::Conv { ui, .. } = &app.screen else {
            panic!()
        };
        assert_eq!(ui.focus, Focus::Pill, "Down focused the pill");
        app.key(Key::Enter, Instant::now());
        let Some(Dialog::Tasks(v)) = dialog(&app) else {
            panic!("{:?}", dialog(&app))
        };
        assert!(v.inline && v.open.is_none(), "two tasks: the list, inline");
        // With one task left, Enter goes straight to its details, and ← closes them.
        let one =
            r#"{"t":"tasks","l":[{"i":"b1","k":"shell","d":"Ticks","st":"running","t0":1000}]}"#;
        let mut app = app_in(&[one]);
        app.key(Key::Down, Instant::now());
        app.key(Key::Enter, Instant::now());
        let Some(Dialog::Tasks(v)) = dialog(&app) else {
            panic!()
        };
        assert_eq!(v.open.as_deref(), Some("b1"));
        assert!(v.direct);
        app.key(Key::Left, Instant::now());
        assert!(dialog(&app).is_none());
        // A typed key after Down is itself, and the focus goes.
        let mut app = app_in(&[one]);
        app.key(Key::Down, Instant::now());
        app.key(Key::Char('h'), Instant::now());
        let Screen::Conv { ui, .. } = &app.screen else {
            panic!()
        };
        assert_eq!(ui.focus, Focus::Prompt);
        assert_eq!(ui.input, "h");
    }

    #[test]
    fn the_tasks_view_opens_details_stops_and_follows_the_list() {
        let mut app = app_in(&[TASKS]);
        type_in(&mut app, "/tasks");
        assert!(matches!(dialog(&app), Some(Dialog::Tasks(v)) if !v.inline));
        let Screen::Conv { c, .. } = &app.screen else {
            panic!()
        };
        let v = tasks::View::default();
        let mut out = Vec::new();
        // x on a row stops it, and the list stays open.
        let d = tasks_key(c, v.clone(), Key::Char('x'), &mut out, &mut None, &mut None);
        assert_eq!(
            out.iter().map(json::to_line).collect::<Vec<_>>(),
            [r#"{"i":"b1","t":"tstop"}"#]
        );
        assert!(matches!(d, Some(Dialog::Tasks(_))));
        // Down and Enter open the monitor's details; ← goes back to the list.
        let mut out = Vec::new();
        let Some(Dialog::Tasks(v)) = tasks_key(c, v, Key::Down, &mut out, &mut None, &mut None)
        else {
            panic!()
        };
        let Some(Dialog::Tasks(v)) = tasks_key(c, v, Key::Enter, &mut out, &mut None, &mut None)
        else {
            panic!()
        };
        assert_eq!(v.open.as_deref(), Some("m1"));
        let Some(Dialog::Tasks(back)) =
            tasks_key(c, v.clone(), Key::Left, &mut out, &mut None, &mut None)
        else {
            panic!()
        };
        assert_eq!(back.open, None);
        assert_eq!(back.sel, 1, "the cursor stays on its row");
        // x in the details stops it and goes back; Esc closes.
        let Some(Dialog::Tasks(after)) =
            tasks_key(c, v.clone(), Key::Char('x'), &mut out, &mut None, &mut None)
        else {
            panic!()
        };
        assert_eq!(after.open, None);
        assert_eq!(
            out.iter().map(json::to_line).collect::<Vec<_>>(),
            [r#"{"i":"m1","t":"tstop"}"#]
        );
        assert!(
            tasks_key(
                c,
                v.clone(),
                Key::Esc,
                &mut Vec::new(),
                &mut None,
                &mut None
            )
            .is_none()
        );
        // Its output arrives for it, and its leaving the list takes the view back to the list.
        if let Screen::Conv { ui, .. } = &mut app.screen {
            ui.dialog = Some(Dialog::Tasks(v));
        }
        app.heard(
            parse(r#"{"t":"tout","i":"m1","x":"a\nb\n","n":4}"#).unwrap(),
            Instant::now(),
        );
        assert!(
            matches!(dialog(&app), Some(Dialog::Tasks(v)) if v.output == Some(Ok("a\nb\n".into())))
        );
        assert_eq!(app.output_wanted().as_deref(), Some("m1"));
        app.heard(
            parse(r#"{"t":"tasks","l":[{"i":"b1","k":"shell","d":"Ticks","st":"running"}]}"#)
                .unwrap(),
            Instant::now(),
        );
        assert!(matches!(dialog(&app), Some(Dialog::Tasks(v)) if v.open.is_none()));
        assert_eq!(app.output_wanted(), None);
    }

    const AGENTS: &str = r#"{"t":"tasks","l":[{"i":"ag1","k":"agent","d":"slow reader","c":"general-purpose","st":"running","t0":1000},{"i":"wf1","k":"workflow","d":"Two agents","c":"two-readers","st":"running","an":2,"sv":true,"t0":1000}]}"#;

    fn ui_of(app: &App) -> &Ui {
        let Screen::Conv { ui, .. } = &app.screen else {
            panic!()
        };
        ui
    }

    fn viewing(app: &App) -> Option<String> {
        let Screen::Conv { c, .. } = &app.screen else {
            panic!()
        };
        c.viewing.as_ref().map(|(id, _)| id.clone())
    }

    #[test]
    fn down_walks_the_agent_strip_and_enter_views_an_agent_then_main_again() {
        let mut app = app_in(&[AGENTS]);
        let key = |app: &mut App, k: Key| app.key(k, Instant::now());
        // No shells, so no pill: Down goes straight to `main`.
        key(&mut app, Key::Down);
        assert_eq!(ui_of(&app).focus, Focus::Strip(0));
        key(&mut app, Key::Down);
        key(&mut app, Key::Down);
        key(&mut app, Key::Down);
        assert_eq!(
            ui_of(&app).focus,
            Focus::Strip(2),
            "it stops at the last row"
        );
        key(&mut app, Key::Up);
        assert_eq!(ui_of(&app).focus, Focus::Strip(1));
        key(&mut app, Key::Enter);
        assert_eq!(viewing(&app).as_deref(), Some("ag1"));
        assert_eq!(ui_of(&app).focus, Focus::Prompt);
        // Its conversation arrives for it.
        app.heard(
            parse(r#"{"t":"fgo","i":"ag1","reset":true,"o":[{"t":"add","id":1,"k":"user","x":"Read it."},{"t":"st","s":"working"}]}"#).unwrap(),
            Instant::now(),
        );
        let Screen::Conv { c, .. } = &app.screen else {
            panic!()
        };
        let a = &c.viewing.as_ref().unwrap().1;
        assert_eq!(a.entries, [Entry::Prompt("Read it.".into())]);
        assert_eq!(a.state, "working");
        // A message to it says why it cannot go; a slash command still works.
        type_in(&mut app, "hello");
        assert_eq!(ui_of(&app).status.as_deref(), Some(AGENT_MESSAGE_REFUSED));
        assert_eq!(ui_of(&app).input, "hello", "kept");
        // Down then Enter on `main`: the conversation again.
        if let Screen::Conv { ui, .. } = &mut app.screen {
            ui.input.clear();
            ui.cursor = 0;
        }
        key(&mut app, Key::Down);
        key(&mut app, Key::Enter);
        assert_eq!(viewing(&app), None);
        // Enter on the workflow's row opens its browser.
        key(&mut app, Key::Down);
        key(&mut app, Key::Down);
        key(&mut app, Key::Down);
        key(&mut app, Key::Enter);
        assert!(matches!(dialog(&app), Some(Dialog::Tasks(v)) if v.open.as_deref() == Some("wf1")));
        assert_eq!(app.output_wanted().as_deref(), Some("wf:wf1"));
    }

    #[test]
    fn the_tasks_view_foregrounds_agents_and_its_browser_saves_a_workflow() {
        let app = app_in(&[AGENTS]);
        let Screen::Conv { c, .. } = &app.screen else {
            panic!()
        };
        let (mut toast, mut fg) = (None, None);
        let mut out = Vec::new();
        // `f` on the running agent's row brings it into view.
        let d = tasks_key(
            c,
            tasks::View::default(),
            Key::Char('f'),
            &mut out,
            &mut toast,
            &mut fg,
        );
        assert!(d.is_none());
        assert_eq!(fg.as_ref().map(|t| t.id.as_str()), Some("ag1"));
        // Esc from a task's details says they were dismissed.
        let mut v = tasks::View::default();
        open_task(&mut v, &c.tasks[0]);
        assert!(tasks_key(c, v, Key::Esc, &mut out, &mut toast, &mut None).is_none());
        assert_eq!(toast.as_deref(), Some("Agent details dismissed"));
        // The browser: → to the agents, f filters, ← back, s opens the save form.
        let mut v = tasks::View::default();
        open_task(&mut v, &c.tasks[1]);
        let step = |v: tasks::View, k: Key, out: &mut Vec<Value>| match tasks_key(
            c, v, k, out, &mut None, &mut None,
        ) {
            Some(Dialog::Tasks(v)) => v,
            d => panic!("{d:?}"),
        };
        let v = step(v, Key::Right, &mut out);
        assert_eq!(v.pane, tasks::Pane::Agents);
        let v = step(v, Key::Char('f'), &mut out);
        assert_eq!(v.filter, "running");
        let v = step(v, Key::Left, &mut out);
        assert_eq!(v.pane, tasks::Pane::Phases);
        let v = step(v, Key::Char('s'), &mut out);
        assert_eq!(
            v.save.as_ref().map(|f| f.name.as_str()),
            Some("two-readers")
        );
        let v = step(v, Key::Char('\t'), &mut out);
        let mut out = Vec::new();
        let v = step(v, Key::Enter, &mut out);
        assert_eq!(
            out.iter().map(json::to_line).collect::<Vec<_>>(),
            [r#"{"i":"wf1","n":"two-readers","sc":"user","t":"wsave"}"#]
        );
        // x in the phases stops the workflow; Esc leaves the form, then the browser.
        let v = step(v, Key::Esc, &mut out);
        assert!(v.save.is_none());
        let mut out = Vec::new();
        let v = step(v, Key::Char('x'), &mut out);
        assert_eq!(
            out.iter().map(json::to_line).collect::<Vec<_>>(),
            [r#"{"i":"wf1","t":"tstop"}"#]
        );
        let v = step(v, Key::Esc, &mut out);
        assert_eq!(v.open, None, "back to the list");
    }

    #[test]
    fn esc_in_an_agents_view_interrupts_it_and_leaves_one_that_ended() {
        let mut app = app_in(&[AGENTS]);
        if let Screen::Conv { c, ui, .. } = &mut app.screen {
            let t = c.tasks[0].clone();
            view_agent(c, ui, &t);
        }
        // Nothing to read what was sent here; the view stays, as Claude Code's does.
        app.key(Key::Esc, Instant::now());
        assert_eq!(viewing(&app).as_deref(), Some("ag1"));
        app.heard(
            parse(
                r#"{"t":"tasks","l":[{"i":"ag1","k":"agent","d":"slow reader","st":"stopped"}]}"#,
            )
            .unwrap(),
            Instant::now(),
        );
        app.key(Key::Esc, Instant::now());
        assert_eq!(viewing(&app), None);
    }

    fn dialog(app: &App) -> Option<&Dialog> {
        match &app.screen {
            Screen::Conv { ui, .. } => ui.dialog.as_ref(),
            Screen::Home => None,
        }
    }

    #[test]
    fn ctrl_backslash_and_disconnect_leave_for_the_home_and_from_the_home_quit() {
        let mut app = app_in(&[]);
        type_in(&mut app, "/effort");
        assert!(matches!(dialog(&app), Some(Dialog::Effort { .. })));
        app.key(Key::Char(DETACH), Instant::now());
        assert!(
            matches!(app.screen, Screen::Home),
            "out, the dialog open or not"
        );
        assert!(!app.quit);
        app.key(Key::Char(DETACH), Instant::now());
        assert!(app.quit, "and from the home, out of the client");

        let mut app = app_in(&[]);
        type_in(&mut app, "/disconnect");
        assert!(matches!(app.screen, Screen::Home));
    }

    #[test]
    fn ctrl_o_and_expand_switch_between_tool_calls_summed_and_shown() {
        let mut app = app_in(&[]);
        let expanded = |app: &App| match &app.screen {
            Screen::Conv { c, .. } => c.expanded,
            Screen::Home => panic!("left"),
        };
        app.key(Key::Char('\x0f'), Instant::now());
        assert!(expanded(&app));
        type_in(&mut app, "/expand");
        assert!(!expanded(&app));
    }

    #[test]
    fn copy_takes_a_whole_response_or_one_back_and_offers_its_code_blocks() {
        let mut app = app_in(&[
            r#"{"t":"add","id":1,"k":"user","x":"one"}"#,
            r#"{"t":"add","id":2,"k":"text","x":"first"}"#,
            r#"{"t":"add","id":3,"k":"tool","n":"Bash","a":"ls"}"#,
            r#"{"t":"add","id":4,"k":"text","x":"still first"}"#,
            r#"{"t":"add","id":5,"k":"user","x":"two"}"#,
            r#"{"t":"add","id":6,"k":"text","x":"Run:\n\n```sh\nmake test\n```\n\nthen look."}"#,
        ]);
        type_in(&mut app, "/copy 2");
        assert_eq!(
            app.clip.take().as_deref(),
            Some("first\n\nstill first"),
            "a response is a turn's text, tool calls left out"
        );
        type_in(&mut app, "/copy 3");
        assert!(app.clip.is_none(), "there is no third");
        type_in(&mut app, "/copy");
        let Some(Dialog::Copy { choices, .. }) = dialog(&app) else {
            panic!("a response with code offers a choice");
        };
        let labels: Vec<&str> = choices.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Full response", "sh: make test"]);
        app.key(Key::Down, Instant::now());
        app.key(Key::Enter, Instant::now());
        assert_eq!(app.clip.as_deref(), Some("make test"));
        assert!(dialog(&app).is_none());
    }

    #[test]
    fn btw_asks_beside_the_conversation_and_shows_the_answer_until_dismissed() {
        let mut app = app_in(&[r#"{"t":"add","id":1,"k":"text","x":"hello"}"#]);
        let c_before = match &app.screen {
            Screen::Conv { c, .. } => c.entries.len(),
            Screen::Home => 0,
        };
        type_in(&mut app, "/btw what was that?");
        assert!(matches!(
            dialog(&app),
            Some(Dialog::Btw { answer: None, question, .. }) if question == "what was that?"
        ));
        app.heard(
            parse(r#"{"t":"btw","x":"A greeting."}"#).unwrap(),
            Instant::now(),
        );
        assert!(matches!(
            dialog(&app),
            Some(Dialog::Btw { answer: Some(Ok(a)), .. }) if a == "A greeting."
        ));
        app.key(Key::Char('x'), Instant::now());
        assert!(
            dialog(&app).is_some(),
            "only Space, Enter or Esc dismiss it"
        );
        app.key(Key::Esc, Instant::now());
        type_in(&mut app, "/btw again?");
        app.heard(
            parse(r#"{"t":"err","x":"\"btw\" is not something the relay knows"}"#).unwrap(),
            Instant::now(),
        );
        assert!(
            matches!(
                dialog(&app),
                Some(Dialog::Btw {
                    answer: Some(Err(_)),
                    ..
                })
            ),
            "an older relay's refusal ends the wait"
        );
        app.key(Key::Esc, Instant::now());
        assert!(dialog(&app).is_none());
        let c_after = match &app.screen {
            Screen::Conv { c, .. } => c.entries.len(),
            Screen::Home => 0,
        };
        assert_eq!(c_before, c_after, "nothing of it joins the transcript");
    }

    #[test]
    fn up_takes_a_queued_prompt_back_above_what_is_being_typed() {
        // Claude Code 2.1.311, driven in a pty: "then one about lakes" queued, "draft here"
        // typed, Up gives both, the queued one first.
        let mut app = app_in(&[r#"{"t":"st","s":"working"}"#]);
        if let Screen::Conv { c, .. } = &mut app.screen {
            c.sent("then one about lakes", Instant::now());
        }
        for ch in "draft here".chars() {
            app.key(Key::Char(ch), Instant::now());
        }
        app.key(Key::Up, Instant::now());
        let Screen::Conv { c, ui, .. } = &app.screen else {
            panic!("left the conversation");
        };
        assert_eq!(ui.input, "then one about lakes\ndraft here");
        assert!(c.pending.is_empty(), "withdrawn from the queue");
    }

    #[test]
    fn a_side_question_whose_link_drops_says_so_rather_than_waiting_for_ever() {
        let mut app = app_in(&[r#"{"t":"add","id":1,"k":"text","x":"hello"}"#]);
        type_in(&mut app, "/btw what was that?");
        app.link_down(Instant::now());
        assert!(matches!(
            dialog(&app),
            Some(Dialog::Btw { answer: Some(Err(e)), .. }) if e.contains("ask again")
        ));
    }

    #[test]
    fn the_prompt_edits_as_a_line_editor_does() {
        let mut ui = Ui::new();
        for k in "helo".chars().map(Key::Char) {
            edit(&mut ui, k);
        }
        edit(&mut ui, Key::Left);
        edit(&mut ui, Key::Char('l'));
        assert_eq!((ui.input.as_str(), ui.cursor), ("hello", 4));
        edit(&mut ui, Key::End);
        edit(&mut ui, Key::Char('\x7f'));
        edit(&mut ui, Key::Home);
        edit(&mut ui, Key::Delete);
        assert_eq!(ui.input, "ell");
        edit(&mut ui, Key::Char('é'));
        assert_eq!(ui.input, "éell");
    }

    #[test]
    fn copying_is_osc_52_in_base64() {
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(took(Duration::from_secs(41)), "41s");
        assert_eq!(took(Duration::from_secs(125)), "2m 5s");
    }
}
