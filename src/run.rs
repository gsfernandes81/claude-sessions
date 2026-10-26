//! The menu's loop: read, decide, draw — and draw only what changed.
//!
//! **The menu is the hub's** (2026-10-26): its rows are what each container's agent last sent
//! (`hub.rs`, `agent.rs`), and an action is sent to the container its row is in. With one
//! container — `local`, when nothing else is configured — that is exactly the old single
//! container's menu, its agent a child process instead of a container away.
//!
//! **An idle menu emits zero bytes** (`CLAUDE.md`; the link is metered). Three things keep it
//! so, and each is load-bearing:
//!
//! - **Only the lines that differ from what is on the screen are written.** Every wake-up
//!   renders, compares line by line, and usually writes nothing; a spinner turning writes the
//!   line or two it is on.
//! - **A container's agent sends its rows only when they change**, or when the minute turns
//!   for the ages, and the loop wakes on nothing else but a key, a resize and a reconnect due
//!   to a container that is not answering — which changes nothing on screen unless it answers.
//! - **The home's memory figure is refreshed only with something else.** The pool's memory
//!   moves by a megabyte every few seconds on a busy box; refreshed on its own it would redraw
//!   the screen that often. It is brought up to date when a key is pressed and when the minute
//!   turns — the one tick the ages are allowed. A container's own figure arrives with its rows.

use crate::clock;
use crate::hub::{self, Conn, Event};
use crate::json::Value;
use crate::mem;
use crate::menu::{self, Action, Menu, Place, Reach};
use crate::proto;
use crate::render;
use crate::term::{self, Input, RawTerminal};
use crate::ui::{Busy, Header, Key, Outcome, Row, Terminal, TooNarrow};
use crate::work::{self, Event as WorkEvent, Spin, Work};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// How long a container that is not answering waits before it is asked again:
/// `CLAUDE_SESSIONS_RETRY_MS`, for the tests, else 15 seconds.
fn retry() -> Duration {
    std::env::var("CLAUDE_SESSIONS_RETRY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(15), Duration::from_millis)
}

/// Clear the screen, for the first frame and after anything that leaves it in an unknown
/// state — a child that had the terminal, a resize.
const CLEAR: &[u8] = b"\x1b[2J";

/// The home's header: the hub's own name, and the pool's memory where infra mounts it
/// (`CLAUDE_SESSIONS_POOL`, a cgroup v2 directory).
fn home_header() -> Header {
    let memory = std::env::var("CLAUDE_SESSIONS_POOL")
        .ok()
        .filter(|d| !d.is_empty())
        .map(|d| mem::read_at(&d))
        .and_then(|m| m.limit.zip(m.used()).map(|(limit, used)| (used, limit)));
    Header {
        host: menu::host(),
        memory,
    }
}

fn too_narrow(t: &TooNarrow) -> String {
    format!(
        "the terminal is {} columns wide; the menu needs at least {}",
        t.width, t.need
    )
}

/// Every container, and what the menu has heard from each.
struct Hub {
    names: Vec<String>,
    conns: HashMap<String, Arc<Conn>>,
    rows: HashMap<String, Vec<Row>>,
    /// Containers that have said something — rows, or that they are not answering.
    heard: std::collections::HashSet<String>,
    /// Each container's place on the menu, as it last said.
    places: HashMap<String, Place>,
    /// When each container that is not answering is to be asked again.
    retry: HashMap<String, Instant>,
    events: Sender<Event>,
}

impl Hub {
    fn start(names: Vec<String>, events: Sender<Event>) -> Hub {
        let conns = names
            .iter()
            .map(|n| (n.clone(), hub::connect(n, events.clone(), term::wake)))
            .collect();
        Hub {
            names,
            conns,
            rows: HashMap::new(),
            heard: Default::default(),
            places: HashMap::new(),
            retry: HashMap::new(),
            events,
        }
    }

    fn initial(&self) -> Vec<Place> {
        self.names
            .iter()
            .map(|n| {
                let mut p = Place::new(n);
                // `local` is this machine, and the header names it as the old menu did.
                if n == hub::LOCAL {
                    p.header.host = menu::host();
                }
                p
            })
            .collect()
    }

    /// Take in what a container said: its rows, and its place on the menu.
    fn apply(&mut self, ev: Event) {
        match ev {
            Event::Snapshot(name, snap) => {
                self.heard.insert(name.clone());
                self.retry.remove(&name);
                self.places
                    .insert(name.clone(), hub::place_of(&name, &snap));
                self.rows.insert(name, snap.rows);
            }
            Event::Down(name, why) => {
                self.heard.insert(name.clone());
                self.retry.insert(name.clone(), Instant::now() + retry());
                self.rows.remove(&name);
                let place = self
                    .places
                    .entry(name.clone())
                    .or_insert_with(|| Place::new(&name));
                place.header.memory = None;
                place.reach = Reach::Down(why);
            }
        }
    }

    /// Every container's place as last heard, onto the menu.
    fn sync(&self, menu: &mut Menu) {
        for name in &self.names {
            if let Some(p) = self.places.get(name) {
                menu.set_place(p.clone());
            }
        }
    }

    fn merged(&self) -> Vec<Row> {
        let lists: Vec<&[Row]> = self
            .names
            .iter()
            .map(|n| self.rows.get(n).map_or(&[][..], Vec::as_slice))
            .collect();
        hub::merge(&lists)
    }

    /// Ask again every container whose time has come. Not shown as connecting: a container
    /// that still does not answer must cost the screen nothing.
    fn reconnect_due(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .retry
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(n, _)| n.clone())
            .collect();
        for name in due {
            self.retry.remove(&name);
            let conn = hub::connect(&name, self.events.clone(), term::wake);
            self.conns.insert(name, conn);
        }
    }

    fn next_retry(&self) -> Option<Duration> {
        self.retry
            .values()
            .min()
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    fn answering(&self, name: &str) -> Option<Arc<Conn>> {
        if self.retry.contains_key(name) {
            return None;
        }
        self.conns.get(name).cloned()
    }
}

/// Run the menu until the owner quits. `Err` carries why it could not run — the door prints
/// it and falls through to a login shell.
///
/// **The first frame does not wait long.** Given a quarter of a second every container has
/// usually answered and the first frame is the list. With one container that has not, the
/// frame is drawn with a spinner where the list will go (mockup 10), and `n`, `s` and `?`
/// work while it is read; at the home, a container not yet heard from says `connecting`.
pub fn run() -> Result<(), Stop> {
    let (width, height) = term::size().ok_or_else(|| Stop::Failed("not a terminal".into()))?;
    let (tx, rx) = mpsc::channel();
    let mut hub = Hub::start(hub::hosts(), tx);
    let started = Instant::now();
    while hub.heard.len() < hub.names.len() {
        let left = work::SHOW_AFTER.saturating_sub(started.elapsed());
        match rx.recv_timeout(left) {
            Ok(ev) => hub.apply(ev),
            Err(_) => break,
        }
    }
    let single = hub.names.len() == 1;
    let loading = single && hub.heard.is_empty();
    let mut menu = Menu::hub(width, height, home_header(), hub.initial(), hub.merged());
    hub.sync(&mut menu);
    let mut spin = Spin::after(started, work::SHOW_AFTER);
    if loading {
        let frame = spin.tick(Instant::now()).unwrap_or(0);
        menu.set_busy(Some(reading(frame)));
    }
    // Refuse before touching the terminal: a door that falls through to a shell should find
    // the terminal exactly as the login left it.
    if let Err(t) = render::render(&menu.view()) {
        return Err(Stop::Failed(too_narrow(&t)));
    }
    let mut term = RawTerminal::enter().map_err(|e| match io(e) {
        Stop::Failed(why) => Stop::Failed(format!("cannot set up the terminal: {why}")),
        gone => gone,
    })?;
    let result = drive(&mut term, &mut menu, &mut hub, &rx, loading.then_some(spin));
    drop(term);
    result
}

/// Why the menu stopped, when it did not stop because the owner quit.
#[derive(Debug)]
pub enum Stop {
    /// Something went wrong, and the door should say what before it falls back to a shell.
    Failed(String),
    /// The terminal is gone — an ssh link dropped under the menu, most often on a phone. There
    /// is nobody to tell and nothing to write to: issue #6 was a report of exactly this
    /// written to the dead terminal, which panicked. The menu ends quietly instead.
    TerminalGone,
}

/// A terminal error as the menu treats it: gone, or a failure worth reporting.
fn io(e: std::io::Error) -> Stop {
    if terminal_gone(&e) {
        Stop::TerminalGone
    } else {
        Stop::Failed(e.to_string())
    }
}

/// What a terminal that has hung up answers with: `term.rs`'s end-of-file for a poll that
/// reports the hangup, and from a read or a write `EIO` (5) — what Linux gives on a pty whose
/// other side has closed — `ENXIO` (6), `EBADF` (9) or `EPIPE` (32). The numbers are Linux's
/// own and the same on both targets.
fn terminal_gone(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::UnexpectedEof
        || matches!(e.raw_os_error(), Some(5 | 6 | 9 | 32))
}

fn reading(frame: usize) -> Busy {
    Busy {
        frame,
        row: None,
        what: "reading sessions".to_string(),
        loading: true,
    }
}

/// What has been written to the terminal, line by line, so that only lines that changed are
/// written again. `None` means the screen is in an unknown state and is drawn from nothing.
type Screen = Option<Vec<String>>;

/// Draw the menu, writing only what differs from what is on the screen. Nothing at all when
/// nothing changed — the idle menu's zero bytes — and one line when one line did, such as
/// the spinner turning.
fn draw(term: &mut RawTerminal, menu: &Menu, screen: &mut Screen) -> Result<(), Stop> {
    let frame = render::render(&menu.view()).map_err(|t| Stop::Failed(too_narrow(&t)))?;
    let lines: Vec<String> = (0..frame.lines.len()).map(|i| frame.ansi_line(i)).collect();
    let out = match screen.as_ref() {
        Some(prev) if prev.len() == lines.len() => {
            let mut out = String::new();
            for (i, line) in lines.iter().enumerate() {
                if prev[i] != *line {
                    out.push_str(&format!("\x1b[{};1H", i + 1));
                    out.push_str(line);
                }
            }
            out
        }
        _ => {
            let mut out = String::from_utf8_lossy(CLEAR).into_owned();
            out.push_str(&frame.ansi());
            out
        }
    };
    if !out.is_empty() {
        term.write_all(out.as_bytes()).map_err(io)?;
    }
    *screen = Some(lines);
    Ok(())
}

/// How often the loop looks at running work while it waits on the terminal: how late a
/// worker's request for the terminal can be answered.
const CHECK: Duration = Duration::from_millis(20);

fn drive(
    term: &mut RawTerminal,
    menu: &mut Menu,
    hub: &mut Hub,
    rx: &Receiver<Event>,
    mut loading: Option<Spin>,
) -> Result<(), Stop> {
    let mut screen: Screen = None;
    let mut minute = menu::age_clock(clock::now());
    // The first rows to arrive are sorted into their groups, as the first reading always
    // was; after that, rows keep their places.
    let mut first = menu.all_rows().is_empty();
    loop {
        let mut heard = false;
        while let Ok(ev) = rx.try_recv() {
            hub.apply(ev);
            heard = true;
        }
        if heard {
            hub.sync(menu);
            let rows = hub.merged();
            let sort = first && !rows.is_empty();
            menu.replace_rows(rows, sort);
            if sort {
                first = false;
            }
            if loading.is_some() && !hub.heard.is_empty() {
                loading = None;
                menu.set_busy(None);
            }
            let now = menu::age_clock(clock::now());
            if now != minute {
                minute = now;
                menu.set_home(home_header());
            }
        }
        if let Some(spin) = loading.as_mut() {
            if let Some(frame) = spin.tick(Instant::now()) {
                menu.set_busy(Some(reading(frame)));
            }
        }
        draw(term, menu, &mut screen)?;

        let wait = match loading.as_ref() {
            Some(spin) => Some(spin.until_next(Instant::now()).min(CHECK)),
            None => hub.next_retry(),
        };
        match term.wait(wait).map_err(io)? {
            Input::Timeout => hub.reconnect_due(),
            Input::Woken => {}
            Input::Keys(keys) => {
                menu.set_home(home_header());
                for key in keys {
                    if key == Key::Resize {
                        if let Some((w, h)) = term::size() {
                            menu.resize(w, h);
                        }
                        screen = None;
                    }
                    let action = menu.key(key);
                    if action == Action::Quit {
                        return Ok(());
                    }
                    if act(term, menu, &mut screen, hub, action)? == Done::Quit {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// What doing an action did, for the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    Nothing,
    Changed,
    /// The owner quit while it was being worked on.
    Quit,
}

/// Do what a key asked, in the container it is about, with a spinner if it takes long enough
/// to need one (mockup 11). The container's rows after it arrive as its snapshot, sent before
/// it says the action is done.
fn act(
    term: &mut RawTerminal,
    menu: &mut Menu,
    screen: &mut Screen,
    hub: &Hub,
    action: Action,
) -> Result<Done, Stop> {
    let rows = menu.rows().to_vec();
    let host_of = |i: usize| rows.get(i).map(|r| r.host.clone());
    let key_of = |i: usize| rows.get(i).map(|r| proto::key_to(&r.key));
    // What the spinner says, and the row it stands on. No title: the row says which (owner,
    // 2026-10-04).
    let resumes = |i: usize| rows.get(i).is_some_and(|r| r.offloaded || r.closed);
    let (row, what): (Option<usize>, &str) = match &action {
        Action::None | Action::Redraw | Action::Quit => return Ok(Done::Nothing),
        Action::Open(i) if resumes(*i) => (Some(*i), "resuming session"),
        Action::Open(i) => (Some(*i), "opening session"),
        Action::New(_) => (None, "starting new session"),
        Action::Shell(_) => (None, "starting shell"),
        Action::Close(i) => (Some(*i), "closing session"),
        Action::OffloadThenOpen { victim, .. } => (Some(*victim), "offloading session for room"),
    };
    let then = match &action {
        Action::Open(i) => Some(*i),
        Action::OffloadThenOpen { then, .. } => *then,
        _ => None,
    };
    let op = |name: &str, fields: Vec<(&str, Option<Value>)>| {
        let mut v = Value::obj();
        v.set("op", Value::string(name));
        for (k, val) in fields {
            if let Some(val) = val {
                v.set(k, val);
            }
        }
        v
    };
    let (host, op) = match &action {
        Action::None | Action::Redraw | Action::Quit => return Ok(Done::Nothing),
        Action::Open(i) => (host_of(*i), op("open", vec![("key", key_of(*i))])),
        Action::New(h) => (Some(h.clone()), op("new", vec![])),
        Action::Shell(h) => (Some(h.clone()), op("shell", vec![])),
        Action::Close(i) => (host_of(*i), op("close", vec![("key", key_of(*i))])),
        Action::OffloadThenOpen { victim, then } => (
            host_of(*victim),
            op(
                "offload_then_open",
                vec![("victim", key_of(*victim)), ("then", then.and_then(key_of))],
            ),
        ),
    };
    let Some(host) = host else {
        menu.set_status(Some("that session is no longer listed".into()));
        return Ok(Done::Changed);
    };
    let job = match hub.answering(&host) {
        Some(conn) => hub::job(conn, op),
        // A shell is still worth trying where the agent is not answering: ssh may be.
        None if matches!(action, Action::Shell(_)) => hub::shell_job(&host),
        None => {
            menu.set_status(Some(format!("{host} is not answering")));
            return Ok(Done::Changed);
        }
    };
    let Some(outcome) = wait_on(term, menu, screen, Work::spawn(job), row, what)? else {
        return Ok(Done::Quit);
    };
    match outcome {
        Outcome::Back(status) => menu.set_status(status),
        Outcome::Refused(why) => menu.set_status(Some(why)),
        Outcome::NoRoom(dialog) => menu.ask_no_room(dialog, then, &host),
        Outcome::ResumeFailed(dialog) => menu.ask_resume_failed(dialog, then.unwrap_or(0)),
    }
    Ok(Done::Changed)
}

/// Wait for work to finish, drawing its spinner once it has taken long enough, and handing
/// the terminal over whenever it asks. `None` if the owner quit meanwhile.
///
/// Keys are not acted on while it works, so nothing can be done to a session mid-close —
/// except `q` and Ctrl-C, which quit, and a resize, which redraws.
fn wait_on(
    term: &mut RawTerminal,
    menu: &mut Menu,
    screen: &mut Screen,
    work: Work,
    row: Option<usize>,
    what: &str,
) -> Result<Option<Outcome>, Stop> {
    let mut spin = Spin::after(Instant::now(), work::SHOW_AFTER);
    loop {
        let now = Instant::now();
        let wait = spin.until_next(now).min(CHECK);
        match work.next(wait) {
            Some(WorkEvent::Done(outcome)) => {
                menu.set_busy(None);
                return Ok(Some(outcome));
            }
            Some(WorkEvent::Suspend) => {
                // The child gets a terminal with nothing of ours on it, and we touch the
                // terminal again only when it is handed back.
                menu.set_busy(None);
                let suspended = term.suspend();
                work.ack();
                suspended.map_err(io)?;
                loop {
                    match work.next(Duration::from_secs(3600)) {
                        Some(WorkEvent::Resume) => break,
                        Some(WorkEvent::Done(outcome)) => {
                            let _ = term.resume();
                            *screen = None;
                            return Ok(Some(outcome));
                        }
                        _ => {}
                    }
                }
                let resumed = term.resume();
                work.ack();
                resumed.map_err(io)?;
                // The alternate screen came back blank; and what follows — settling, reading
                // the record — gets its own quarter second before a spinner shows.
                *screen = None;
                draw(term, menu, screen)?;
                spin = Spin::after(Instant::now(), work::SHOW_AFTER);
                continue;
            }
            Some(WorkEvent::Resume) => work.ack(),
            None => {}
        }
        if let Some(frame) = spin.tick(Instant::now()) {
            menu.set_busy(Some(Busy {
                frame,
                row,
                what: what.to_string(),
                loading: false,
            }));
            draw(term, menu, screen)?;
        }
        match term.wait(Some(Duration::ZERO)).map_err(io)? {
            Input::Keys(keys) => {
                for key in keys {
                    match key {
                        Key::Char('q') | Key::Char('\u{3}') => return Ok(None),
                        Key::Resize => {
                            if let Some((w, h)) = term::size() {
                                menu.resize(w, h);
                            }
                            *screen = None;
                            if spin.frame().is_some() {
                                draw(term, menu, screen)?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Input::Timeout | Input::Woken => {}
        }
    }
}
