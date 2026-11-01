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

use crate::convo::{self, Applied, Convo, Dialog, Entry, Link, Ui};
use crate::json::{self, Value};
use crate::menu::{Action, Menu, Place, Reach};
use crate::proto;
use crate::term::{self, Input, RawTerminal};
use crate::ui::{Busy, Header, Key, Row, RowKey};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const ACK_WAIT: Duration = Duration::from_secs(5);
const QUIET_PROBE: Duration = Duration::from_secs(120);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const SPIN_EVERY: Duration = Duration::from_millis(120);
/// Ctrl+\\, zmx's detach key: out of a conversation to the home, and from the home out.
const DETACH: char = '\x1c';
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
                if let Screen::Conv { c, opening, .. } = &self.screen {
                    if !*opening && !c.slot.is_empty() {
                        let re = App::msg(
                            "re",
                            vec![
                                ("c", Value::string(&c.host)),
                                ("s", Value::string(&c.slot)),
                                ("run", Value::string(&c.run)),
                                ("n", Value::num(c.n as f64)),
                            ],
                        );
                        self.send(re);
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
            }
            "reset" => {
                if let Screen::Conv { c, .. } = &mut self.screen {
                    **c = Convo::new(&c.host, &c.slot, &c.run);
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
                    Screen::Conv { ui, .. } => ui.status = Some(why),
                    Screen::Home => self.menu.set_status(Some(why)),
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
            "pong" | "full" => {}
            _ => {
                // The conversation's ops.
                if let Screen::Conv { c, ui, .. } = &mut self.screen {
                    if let Applied::TurnEnded(d) = c.apply(&v, now) {
                        c.local(Entry::Worked(format!(
                            "Worked for {} · done {}",
                            took(d),
                            clock_time()
                        )));
                    }
                    follow_prompts(c, ui);
                }
            }
        }
        if let Screen::Conv { c, ui, .. } = &mut self.screen {
            if let Some(p) = self.places.get(&c.host) {
                ui.ram = ram(p.header.memory, 40);
            }
        }
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
        match self.menu.key(k) {
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
            Action::New(host, _) => {
                let new = App::msg("new", vec![("c", Value::string(&host))]);
                self.open_conv(&host, new);
            }
            Action::Shell(_)
            | Action::Offload(_)
            | Action::Archive(_)
            | Action::OffloadThenOpen { .. } => {
                self.menu.set_status(Some(
                    "that is for ssh dev; the phone opens and starts".into(),
                ));
            }
            Action::None | Action::Redraw => {}
        }
    }

    fn open_conv(&mut self, host: &str, msg: Value) {
        if !self.up() {
            self.menu
                .set_status(Some("not connected: try again once it is".into()));
            return;
        }
        self.send(msg);
        let mut ui = Ui::new();
        ui.status = Some("Opening…".into());
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
        let mut out: Vec<Value> = Vec::new();
        let mut leave = false;
        match ui.dialog.take() {
            // Ctrl+\\ detaches from anywhere in a conversation, a dialog open or not, as zmx's
            // detach key does in ssh dev: the session runs on, and the home comes back.
            _ if k == Key::Char(DETACH) => leave = true,
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
                Key::Esc => {
                    if ui.scroll > 0 {
                        ui.scroll = 0;
                    } else if c.state != "idle" {
                        out.push(App::msg("esc", vec![]));
                    }
                }
                Key::Enter if backslash_newline(ui) => {}
                Key::Enter => {
                    let text = std::mem::take(&mut ui.input);
                    ui.cursor = 0;
                    ui.scroll = 0;
                    if !text.trim().is_empty() {
                        self.history.push(text.clone());
                    }
                    self.history_at = None;
                    match submit(c, ui, text.trim(), &self.models) {
                        Submit::Send(v) => out.extend(v),
                        Submit::Copy(t) => self.clip = Some(t),
                        Submit::Home => leave = true,
                        Submit::Nothing => {}
                    }
                }
                Key::Up | Key::Down if !self.history.is_empty() => {
                    let n = self.history.len();
                    let at = match (k, self.history_at) {
                        (Key::Up, None) => Some(n - 1),
                        (Key::Up, Some(i)) => Some(i.saturating_sub(1)),
                        (Key::Down, Some(i)) if i + 1 < n => Some(i + 1),
                        _ => None,
                    };
                    self.history_at = at;
                    ui.input = at.map(|i| self.history[i].clone()).unwrap_or_default();
                    ui.cursor = ui.input.chars().count();
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
        if leave {
            self.send(App::msg("close", vec![]));
            self.home(None);
            return;
        }
        // An Enter while the link is down keeps its text, and says it was not sent.
        if !out.is_empty() && !self.up() {
            if let Screen::Conv { ui, .. } = &mut self.screen {
                if let Link::Down { not_sent, .. } = &mut ui.link {
                    *not_sent = true;
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
        for v in out {
            self.send(v);
        }
    }
}

/// A prompt or question has arrived, or been answered elsewhere: open or close its dialog.
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
        _ => Submit::Send(vec![m("say", vec![("x", Value::string(text))])]),
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

fn edit(ui: &mut Ui, k: Key) {
    let mut chars: Vec<char> = ui.input.chars().collect();
    let at = ui.cursor.min(chars.len());
    match k {
        Key::Char('\x7f') | Key::Char('\x08') if at > 0 => {
            chars.remove(at - 1);
            ui.cursor = at - 1;
        }
        Key::Delete if at < chars.len() => {
            chars.remove(at);
        }
        Key::Left => ui.cursor = at.saturating_sub(1),
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
fn ram(memory: Option<(u64, u64)>, _w: usize) -> String {
    let Some((used, limit)) = memory else {
        return String::new();
    };
    let g = |b: u64| b as f64 / (1u64 << 30) as f64;
    let lim = g(limit);
    let lim = if (lim - lim.round()).abs() < 0.05 {
        format!("{}G", lim.round() as u64)
    } else {
        format!("{lim:.1}G")
    };
    format!("RAM {:.1}G/{lim}", g(used))
}

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
                match crate::render::render(&self.menu.view()) {
                    Ok(f) => ((0..f.lines.len()).map(|i| f.ansi_line(i)).collect(), None),
                    Err(_) => (vec!["\x1b[2Ktoo narrow".into()], None),
                }
            }
            Screen::Conv { c, ui, .. } => {
                if let Some(p) = self.places.get(&c.host) {
                    ui.ram = ram(p.header.memory, w as usize);
                }
                let f = convo::draw(c, ui, w as usize, h as usize, now);
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

    /// When the screen next changes by itself: the spinner, the seconds while down, a retry.
    fn next_wake(&self, now: Instant) -> Option<Duration> {
        let mut at: Option<Instant> = self.retry_at;
        let mut soon = |t: Instant| at = Some(at.map_or(t, |a| a.min(t)));
        if let Screen::Conv { c, ui, .. } = &self.screen {
            if matches!(c.state.as_str(), "working" | "compacting") && ui.dialog.is_none() {
                soon(now + SPIN_EVERY);
            }
            if matches!(ui.link, Link::Down { .. }) {
                soon(now + Duration::from_secs(1));
            }
        }
        if matches!(self.screen, Screen::Home) && !self.greeted {
            soon(now + SPIN_EVERY);
        }
        if let Some(a) = self.awaiting {
            soon(a + ACK_WAIT);
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
            drawn: Vec::new(),
            clip: None,
            quit: false,
            started: Instant::now(),
            greeted: false,
            wheel,
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
    term.write_all(b"\x1b[2J").map_err(|e| e.to_string())?;
    app.dial(Instant::now(), &tx);
    let mut hosts_seen: Vec<String> = Vec::new();
    loop {
        let now = Instant::now();
        if app.hosts != hosts_seen {
            hosts_seen = app.hosts.clone();
            let (w, h) = term::size().unwrap_or((40, 24));
            app.rebuild_menu(w, h);
        }
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
        if app.awaiting.is_some_and(|a| now >= a + ACK_WAIT) {
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
    drop(term);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

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
        assert_eq!(
            sent(submit(&mut c, &mut ui, "/compact", &None)),
            [r#"{"t":"say","x":"/compact"}"#]
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
        assert!(dialog(&app).is_none());
        let c_after = match &app.screen {
            Screen::Conv { c, .. } => c.entries.len(),
            Screen::Home => 0,
        };
        assert_eq!(c_before, c_after, "nothing of it joins the transcript");
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
        assert_eq!(ram(Some((966_367_641, 2 << 30)), 40), "RAM 0.9G/2G");
        assert_eq!(took(Duration::from_secs(41)), "41s");
        assert_eq!(took(Duration::from_secs(125)), "2m 5s");
    }
}
