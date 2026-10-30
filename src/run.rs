//! The menu's loop: read, decide, draw — and draw only what changed.
//!
//! **The menu is the hub's** (2026-10-26): its rows are what each container's agent last sent
//! (`hub.rs`, `agent.rs`), and an action is sent to the container its row is in. With one
//! container — `local`, when nothing else is configured — that is exactly the old single
//! container's menu, its agent a child process instead of a container away.
//!
//! **An idle menu writes only its load line, and that only when it changes.** Until 2026-10-27
//! an idle menu wrote nothing at all; the owner then traded that for the load line kept current
//! ("Continuous refresh is fine, that rule can be removed"). What still keeps it quiet:
//!
//! - **Only the lines that differ from what is on the screen are written.** Every wake-up
//!   renders, compares line by line, and usually writes nothing; a spinner turning writes the
//!   line or two it is on, and a fresh reading the load line, if it reads differently.
//! - **A container's agent sends its rows only when they change**, or when the minute turns
//!   for the ages, and the loop wakes on nothing else but a key, a resize, a reconnect due to a
//!   container that is not answering — which changes nothing on screen unless it answers — and
//!   the load line's reading, every [`load_every`].
//! - **The hub's own figures are read on that tick**: the pool's memory, which the home's
//!   header shows too, and the load of the machine the hub is on. A container's arrive with
//!   its rows.

use crate::clock;
use crate::harness::Harness;
use crate::hub::{self, Conn, Event};
use crate::json::Value;
use crate::menu::{self, Action, Menu, Place, Reach};
use crate::proto;
use crate::render;
use crate::statusline;
use crate::term::{self, Input, RawTerminal};
use crate::ui::{Busy, Header, Key, Outcome, Row, Terminal, TooNarrow};
use crate::work::{self, Event as WorkEvent, Spin, Work};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// How long a container that is not answering waits before it is asked again:
/// `CLAUDE_SESSIONS_RETRY_MS`, for the tests, else 15 seconds.
pub fn retry() -> Duration {
    std::env::var("CLAUDE_SESSIONS_RETRY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(15), Duration::from_millis)
}

/// How often the hub reads its own figures for the load line: `CLAUDE_SESSIONS_LOAD_MS`, for
/// the tests, else 5 seconds — the one-minute load average moves slowly, and a line that
/// changed every second would cost a line of output on the metered link every second.
fn load_every() -> Duration {
    std::env::var("CLAUDE_SESSIONS_LOAD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(5), Duration::from_millis)
}

/// Clear the screen, for the first frame and after anything that leaves it in an unknown
/// state — a child that had the terminal, a resize.
const CLEAR: &[u8] = b"\x1b[2J";

/// The home's header: the hub's own name, the pool's memory where infra mounts it
/// (`CLAUDE_SESSIONS_POOL`, a cgroup v2 directory), and the load of the machine the hub is on,
/// which the load line counts first (`statusline::door`).
fn home_header() -> Header {
    Header {
        host: menu::host(),
        memory: statusline::pool(),
        load: statusline::host_load(),
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
    /// How long a container that is not answering waits to be asked again.
    retry_after: Duration,
    events: Sender<Event>,
    connect: Connect,
}

/// How the hub reaches a container: [`hub::connect`], or a stand-in in the tests.
type Connect = fn(&str, Sender<Event>, fn()) -> Arc<Conn>;

impl Hub {
    fn start(names: Vec<String>, events: Sender<Event>) -> Hub {
        Hub::with(names, events, hub::connect, retry())
    }

    fn with(
        names: Vec<String>,
        events: Sender<Event>,
        connect: Connect,
        retry_after: Duration,
    ) -> Hub {
        let conns = names
            .iter()
            .map(|n| (n.clone(), connect(n, events.clone(), term::wake)))
            .collect();
        Hub {
            names,
            conns,
            rows: HashMap::new(),
            heard: Default::default(),
            places: HashMap::new(),
            retry: HashMap::new(),
            retry_after,
            events,
            connect,
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
                self.retry
                    .insert(name.clone(), Instant::now() + self.retry_after);
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
            let conn = (self.connect)(&name, self.events.clone(), term::wake);
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
    settle(&mut hub, &rx, started + work::SHOW_AFTER);
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
    let result = drive(
        &mut term,
        &mut menu,
        &mut hub,
        &rx,
        loading.then_some(spin),
        load_every(),
    );
    drop(term);
    result
}

/// The terminal as the loop uses it: [`RawTerminal`], or a scripted stand-in in the tests.
trait Console: Terminal {
    fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<Input>;
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    /// The window's size as (cols, rows).
    fn size(&self) -> Option<(u16, u16)>;
}

impl Console for RawTerminal {
    fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<Input> {
        RawTerminal::wait(self, timeout)
    }
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        RawTerminal::write_all(self, bytes)
    }
    fn size(&self) -> Option<(u16, u16)> {
        term::size()
    }
}

/// Take in what the containers say until every one has said something, or `deadline`.
fn settle(hub: &mut Hub, rx: &Receiver<Event>, deadline: Instant) {
    while hub.heard.len() < hub.names.len() {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(ev) => hub.apply(ev),
            Err(_) => break,
        }
    }
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
fn draw(term: &mut dyn Console, menu: &Menu, screen: &mut Screen) -> Result<(), Stop> {
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
    term: &mut dyn Console,
    menu: &mut Menu,
    hub: &mut Hub,
    rx: &Receiver<Event>,
    mut loading: Option<Spin>,
    every: Duration,
) -> Result<(), Stop> {
    let mut screen: Screen = None;
    let mut minute = menu::age_clock(clock::now());
    let mut next_reading = Instant::now() + every;
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
            // Whatever was heard, a container has now said something.
            if loading.take().is_some() {
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

        let until_reading = next_reading.saturating_duration_since(Instant::now());
        let wait = match loading.as_ref() {
            Some(spin) => spin.until_next(Instant::now()).min(CHECK),
            None => hub
                .next_retry()
                .map_or(until_reading, |r| r.min(until_reading)),
        };
        match term.wait(Some(wait)).map_err(io)? {
            Input::Timeout => {
                hub.reconnect_due();
                if Instant::now() >= next_reading {
                    next_reading = Instant::now() + every;
                    menu.set_home(home_header());
                }
            }
            Input::Woken => {}
            Input::Keys(keys) => {
                menu.set_home(home_header());
                for key in keys {
                    if key == Key::Resize {
                        if let Some((w, h)) = term.size() {
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
    term: &mut dyn Console,
    menu: &mut Menu,
    screen: &mut Screen,
    hub: &Hub,
    action: Action,
) -> Result<Done, Stop> {
    let Some(plan) = plan(&action, menu.rows()) else {
        return Ok(Done::Nothing);
    };
    let Some(host) = plan.host.clone() else {
        menu.set_status(Some("that session is no longer listed".into()));
        return Ok(Done::Changed);
    };
    let job = match route(hub.answering(&host), &action) {
        Route::Agent(conn) => hub::job(conn, plan.op.clone()),
        Route::Shell => hub::shell_job(&host),
        Route::Unreachable => {
            menu.set_status(Some(format!("{host} is not answering")));
            return Ok(Done::Changed);
        }
    };
    finish(term, menu, screen, Work::spawn(job), &plan, &host)
}

/// What an action asks of a container: which one, what is sent, the spinner shown meanwhile,
/// and the row to open afterwards should a dialog come of it.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    /// `None` when the row the action is about is no longer listed.
    host: Option<String>,
    op: Value,
    /// The row the spinner stands on, and what it says. No title: the row says which (owner,
    /// 2026-10-04).
    row: Option<usize>,
    what: &'static str,
    /// The row to open once there is room, for the no-room and resume-failed dialogs.
    then: Option<usize>,
    /// The harness a new slot runs, for the no-room dialog's `y`.
    harness: Harness,
}

/// [`Plan`] for `action` over the menu's `rows`; `None` for an action that asks nothing.
fn plan(action: &Action, rows: &[Row]) -> Option<Plan> {
    let host_of = |i: usize| rows.get(i).map(|r| r.host.clone());
    let key_of = |i: usize| rows.get(i).map(|r| proto::key_to(&r.key));
    let resumes = |i: usize| rows.get(i).is_some_and(|r| r.offloaded || r.closed);
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
    let harness = match action {
        Action::New(_, h) | Action::OffloadThenOpen { harness: h, .. } => *h,
        _ => Harness::Claude,
    };
    let (host, op, row, what, then) = match action {
        Action::None | Action::Redraw | Action::Quit => return None,
        Action::Open(i) => (
            host_of(*i),
            op("open", vec![("key", key_of(*i))]),
            Some(*i),
            if resumes(*i) {
                "resuming session"
            } else {
                "opening session"
            },
            Some(*i),
        ),
        // A pi session is its own op (0.10.0): an agent too old to know it says so and starts
        // nothing, where a field on `new` would be ignored and start a claude.
        Action::New(h, harness) => (
            Some(h.clone()),
            op(
                match harness {
                    Harness::Claude => "new",
                    Harness::Pi => "new_pi",
                },
                vec![],
            ),
            None,
            "starting new session",
            None,
        ),
        Action::Shell(h) => (
            Some(h.clone()),
            op("shell", vec![]),
            None,
            "starting shell",
            None,
        ),
        Action::Offload(i) => (
            host_of(*i),
            op("offload", vec![("key", key_of(*i))]),
            Some(*i),
            "offloading session",
            None,
        ),
        // Archiving is the agent's `close`, which an agent of any version understands.
        Action::Archive(i) => (
            host_of(*i),
            op("close", vec![("key", key_of(*i))]),
            Some(*i),
            "archiving session",
            None,
        ),
        // As `new`: a pi slot after the offload is its own op, refused by an older agent.
        Action::OffloadThenOpen {
            victim,
            then,
            harness,
        } => (
            host_of(*victim),
            op(
                match harness {
                    Harness::Claude => "offload_then_open",
                    Harness::Pi => "offload_then_open_pi",
                },
                vec![("victim", key_of(*victim)), ("then", then.and_then(key_of))],
            ),
            Some(*victim),
            "offloading session for room",
            *then,
        ),
    };
    Some(Plan {
        host,
        op,
        row,
        what,
        then,
        harness,
    })
}

/// Where an action's work goes.
#[derive(Debug, PartialEq)]
enum Route<C> {
    /// To the container's agent, which is answering.
    Agent(C),
    /// Straight to ssh: a shell is still worth trying where the agent is not answering.
    Shell,
    /// Nowhere: the container is not answering.
    Unreachable,
}

fn route<C>(answering: Option<C>, action: &Action) -> Route<C> {
    match answering {
        Some(conn) => Route::Agent(conn),
        None if matches!(action, Action::Shell(_)) => Route::Shell,
        None => Route::Unreachable,
    }
}

/// Wait out an action's `work` in `host`, then show what came of it.
fn finish(
    term: &mut dyn Console,
    menu: &mut Menu,
    screen: &mut Screen,
    work: Work,
    plan: &Plan,
    host: &str,
) -> Result<Done, Stop> {
    let Some(outcome) = wait_on(term, menu, screen, work, plan.row, plan.what)? else {
        return Ok(Done::Quit);
    };
    match outcome {
        Outcome::Back(status) => menu.set_status(status),
        Outcome::Refused(why) => menu.set_status(Some(why)),
        Outcome::NoRoom(dialog) => menu.ask_no_room(dialog, plan.then, plan.harness, host),
        Outcome::ResumeFailed(dialog) => menu.ask_resume_failed(dialog, plan.then.unwrap_or(0)),
    }
    Ok(Done::Changed)
}

/// Wait for work to finish, drawing its spinner once it has taken long enough, and handing
/// the terminal over whenever it asks. `None` if the owner quit meanwhile.
///
/// Keys are not acted on while it works, so nothing can be done to a session mid-close —
/// except `q` and Ctrl-C, which quit, and a resize, which redraws.
fn wait_on(
    term: &mut dyn Console,
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
                            if let Some((w, h)) = term.size() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Snapshot;
    use crate::ui::RowKey;
    use std::collections::VecDeque;

    /// A scripted terminal. Each `wait` takes the next input; a scripted `Timeout`, or a wait
    /// past the end of the script, first sleeps out the timeout (at most 600 ms), as a real
    /// terminal would, so time moves on as the loop expects.
    struct Fake {
        script: VecDeque<Input>,
        waits: Vec<Option<Duration>>,
        out: Vec<u8>,
        size: (u16, u16),
        handed: Vec<&'static str>,
    }

    impl Fake {
        fn new(script: Vec<Input>) -> Fake {
            Fake {
                script: script.into(),
                waits: Vec::new(),
                out: Vec::new(),
                size: (80, 24),
                handed: Vec::new(),
            }
        }

        fn clears(&self) -> usize {
            self.out
                .windows(CLEAR.len())
                .filter(|w| *w == CLEAR)
                .count()
        }
    }

    impl Terminal for Fake {
        fn suspend(&mut self) -> std::io::Result<()> {
            self.handed.push("suspend");
            Ok(())
        }
        fn resume(&mut self) -> std::io::Result<()> {
            self.handed.push("resume");
            Ok(())
        }
    }

    impl Console for Fake {
        fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<Input> {
            self.waits.push(timeout);
            assert!(
                self.waits.len() < 10_000,
                "the loop ran far past its script"
            );
            match self.script.pop_front() {
                Some(Input::Timeout) | None => {
                    if let Some(t) = timeout {
                        std::thread::sleep(t.min(Duration::from_millis(600)));
                    }
                    Ok(Input::Timeout)
                }
                Some(input) => Ok(input),
            }
        }
        fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.out.extend_from_slice(bytes);
            Ok(())
        }
        fn size(&self) -> Option<(u16, u16)> {
            Some(self.size)
        }
    }

    fn keys(k: &[Key]) -> Input {
        Input::Keys(k.to_vec())
    }

    fn quit() -> Input {
        keys(&[Key::Char('q')])
    }

    fn row(title: &str, at: u64) -> Row {
        Row {
            host: hub::LOCAL.into(),
            key: RowKey::Slot(title.into()),
            wants_you: false,
            busy: false,
            unread: false,
            attached: false,
            offloaded: false,
            closed: false,
            archived: false,
            title: title.into(),
            age: "1m".into(),
            at,
            nested: false,
        }
    }

    fn header() -> Header {
        Header {
            host: "hub".into(),
            memory: None,
            load: None,
        }
    }

    fn snapshot(rows: Vec<Row>) -> Snapshot {
        Snapshot {
            header: header(),
            workspace: "/workspace".into(),
            rows,
        }
    }

    /// A hub over `names` whose connections are never made, asking again after an hour.
    fn offline(names: &[&str]) -> (Hub, Sender<Event>, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let names = names.iter().map(|n| n.to_string()).collect();
        let hub = Hub::with(
            names,
            tx.clone(),
            |n, _, _| Conn::detached(n),
            Duration::from_secs(3600),
        );
        (hub, tx, rx)
    }

    /// One container's menu, with `rows`.
    fn menu(rows: Vec<Row>) -> Menu {
        Menu::hub(80, 24, header(), vec![Place::new(hub::LOCAL)], rows)
    }

    #[test]
    fn a_hung_up_terminal_is_told_from_a_failure() {
        use std::io::{Error, ErrorKind};
        assert!(terminal_gone(&Error::from(ErrorKind::UnexpectedEof)));
        for gone in [5, 6, 9, 32] {
            assert!(terminal_gone(&Error::from_raw_os_error(gone)), "{gone}");
        }
        assert!(
            !terminal_gone(&Error::from_raw_os_error(2)),
            "ENOENT is a failure"
        );
        assert!(!terminal_gone(&Error::other("anything else")));
    }

    #[test]
    fn the_waits_default_to_fifteen_and_five_seconds() {
        // Only the pty tests set these, on the binary they run.
        if std::env::var_os("CLAUDE_SESSIONS_RETRY_MS").is_none() {
            assert_eq!(retry(), Duration::from_secs(15));
        }
        if std::env::var_os("CLAUDE_SESSIONS_LOAD_MS").is_none() {
            assert_eq!(load_every(), Duration::from_secs(5));
        }
    }

    #[test]
    fn local_is_named_for_this_machine_and_others_for_themselves() {
        let (hub, _, _) = offline(&[hub::LOCAL, "or3-dev"]);
        let places = hub.initial();
        assert_eq!(places[0].header.host, menu::host());
        assert_eq!(places[1].header.host, Place::new("or3-dev").header.host);
        assert_ne!(places[1].header.host, menu::host());
    }

    #[test]
    fn a_container_that_is_down_is_asked_again_after_the_wait_and_not_before() {
        let (mut hub, _, _) = offline(&["a"]);
        assert_eq!(hub.next_retry(), None, "nothing is down");
        assert!(hub.answering("a").is_some());
        hub.apply(Event::Down("a".into(), "no route".into()));
        let next = hub.next_retry().expect("a retry is due");
        assert!(next > Duration::from_secs(3590), "{next:?}");
        assert!(hub.answering("a").is_none(), "not answering meanwhile");
        // Its rows arriving ends the wait.
        hub.apply(Event::Snapshot("a".into(), snapshot(vec![])));
        assert_eq!(hub.next_retry(), None);
        assert!(hub.answering("a").is_some());
    }

    #[test]
    fn the_first_frame_waits_for_every_container_or_the_deadline() {
        let (mut hub, tx, rx) = offline(&["a", "b"]);
        tx.send(Event::Snapshot("a".into(), snapshot(vec![])))
            .unwrap();
        let began = Instant::now();
        settle(&mut hub, &rx, began + Duration::from_millis(300));
        assert_eq!(hub.heard.len(), 1);
        assert!(
            began.elapsed() >= Duration::from_millis(250),
            "{:?}",
            began.elapsed()
        );

        let (mut hub, tx, rx) = offline(&["a", "b"]);
        tx.send(Event::Snapshot("a".into(), snapshot(vec![])))
            .unwrap();
        tx.send(Event::Down("b".into(), "no route".into())).unwrap();
        let began = Instant::now();
        settle(&mut hub, &rx, began + Duration::from_secs(5));
        assert_eq!(hub.heard.len(), 2);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn a_frame_writes_only_the_lines_that_changed() {
        let mut m = menu(vec![row("one", 2), row("two", 1)]);
        let mut t = Fake::new(vec![]);
        // A screen of no lines is drawn from nothing.
        let mut screen: Screen = Some(vec![]);
        draw(&mut t, &m, &mut screen).unwrap();
        assert_eq!(t.clears(), 1);
        // Nothing changed: nothing written.
        let before = t.out.len();
        draw(&mut t, &m, &mut screen).unwrap();
        assert_eq!(t.out.len(), before);
        // One line changed: that line, by a cursor move, and no clear.
        m.set_status(Some("detached · it is still running".into()));
        draw(&mut t, &m, &mut screen).unwrap();
        let wrote = String::from_utf8_lossy(&t.out[before..]).into_owned();
        assert_eq!(t.clears(), 1, "{wrote:?}");
        assert!(
            wrote.starts_with("\x1b[") && wrote.contains("detached"),
            "{wrote:?}"
        );
    }

    #[test]
    fn the_load_line_is_read_again_each_interval_from_when_it_was_last_read() {
        let (mut hub, _, rx) = offline(&[hub::LOCAL]);
        let mut m = menu(vec![row("one", 1)]);
        let mut t = Fake::new(vec![quit()]);
        drive(&mut t, &mut m, &mut hub, &rx, None, Duration::from_secs(5)).unwrap();
        let first = t.waits[0].unwrap();
        assert!(first > Duration::from_secs(4), "{first:?}");

        let every = Duration::from_millis(500);
        let mut t = Fake::new(vec![Input::Timeout, Input::Timeout, quit()]);
        drive(&mut t, &mut m, &mut hub, &rx, None, every).unwrap();
        let second = t.waits[1].unwrap();
        assert!(second > Duration::from_millis(200), "{:?}", t.waits);
    }

    #[test]
    fn the_first_rows_heard_are_sorted_and_later_ones_keep_their_places() {
        let x = row("x", 2);
        let y = Row {
            wants_you: true,
            ..row("y", 1)
        };
        let titles = |m: &Menu| {
            m.all_rows()
                .iter()
                .map(|r| r.title.clone())
                .collect::<Vec<_>>()
        };

        let (mut hub, tx, rx) = offline(&[hub::LOCAL]);
        let mut m = menu(vec![]);
        let snap = snapshot(vec![x.clone(), y.clone()]);
        tx.send(Event::Snapshot(hub::LOCAL.into(), snap.clone()))
            .unwrap();
        let mut t = Fake::new(vec![quit()]);
        drive(&mut t, &mut m, &mut hub, &rx, None, Duration::from_secs(5)).unwrap();
        assert_eq!(titles(&m), ["y", "x"], "sorted: the one waiting leads");

        let (mut hub, tx, rx) = offline(&[hub::LOCAL]);
        let mut m = menu(vec![x.clone(), row("y", 1)]);
        assert_eq!(titles(&m), ["x", "y"], "calibration");
        tx.send(Event::Snapshot(hub::LOCAL.into(), snap)).unwrap();
        let mut t = Fake::new(vec![quit()]);
        drive(&mut t, &mut m, &mut hub, &rx, None, Duration::from_secs(5)).unwrap();
        assert_eq!(
            titles(&m),
            ["x", "y"],
            "rows already shown keep their places"
        );
    }

    #[test]
    fn the_reading_spinner_goes_once_a_container_has_said_something() {
        let (mut hub, tx, rx) = offline(&[hub::LOCAL]);
        let mut m = menu(vec![]);
        m.set_busy(Some(reading(0)));
        tx.send(Event::Snapshot(
            hub::LOCAL.into(),
            snapshot(vec![row("x", 1)]),
        ))
        .unwrap();
        let spin = Spin::after(Instant::now(), work::SHOW_AFTER);
        let mut t = Fake::new(vec![quit()]);
        drive(
            &mut t,
            &mut m,
            &mut hub,
            &rx,
            Some(spin),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(m.view().busy.is_none());
    }

    #[test]
    fn a_resize_takes_the_new_size_and_redraws_from_nothing() {
        let (mut hub, _, rx) = offline(&[hub::LOCAL]);
        let mut m = menu(vec![row("x", 1)]);
        let mut t = Fake::new(vec![keys(&[Key::Resize]), quit()]);
        t.size = (40, 24);
        drive(&mut t, &mut m, &mut hub, &rx, None, Duration::from_secs(5)).unwrap();
        assert_eq!(m.view().width, 40);
        assert_eq!(
            t.clears(),
            2,
            "the first frame, then the frame after the resize"
        );
    }

    fn rows_to_act_on() -> Vec<Row> {
        vec![
            Row {
                closed: true,
                ..row("closed", 3)
            },
            Row {
                offloaded: true,
                ..row("offloaded", 2)
            },
            row("live", 1),
        ]
    }

    #[test]
    fn opening_says_whether_it_resumes_and_opens_that_row_after_a_dialog() {
        let rows = rows_to_act_on();
        let what = |i| plan(&Action::Open(i), &rows).unwrap().what;
        assert_eq!(what(0), "resuming session");
        assert_eq!(what(1), "resuming session");
        assert_eq!(what(2), "opening session");
        let p = plan(&Action::Open(2), &rows).unwrap();
        assert_eq!(
            (p.host.as_deref(), p.row, p.then),
            (Some(hub::LOCAL), Some(2), Some(2))
        );
        let key = |i: usize| crate::json::to_line(&proto::key_to(&rows[i].key));
        assert_eq!(
            crate::json::to_line(&p.op),
            format!(r#"{{"key":{},"op":"open"}}"#, key(2))
        );
        // A row no longer listed has no container to go to.
        assert_eq!(plan(&Action::Open(9), &rows).unwrap().host, None);
    }

    #[test]
    fn offloading_for_room_spins_on_the_victim_and_opens_the_other_after() {
        let rows = rows_to_act_on();
        let action = Action::OffloadThenOpen {
            victim: 2,
            then: Some(1),
            harness: Harness::Claude,
        };
        let p = plan(&action, &rows).unwrap();
        assert_eq!(
            (p.row, p.what, p.then),
            (Some(2), "offloading session for room", Some(1))
        );
        let key = |i: usize| crate::json::to_line(&proto::key_to(&rows[i].key));
        assert_eq!(
            crate::json::to_line(&p.op),
            format!(
                r#"{{"op":"offload_then_open","then":{},"victim":{}}}"#,
                key(1),
                key(2)
            )
        );
        // The rest: what each sends and says, and that nothing is asked of a key that asks
        // nothing.
        let say = |a: Action| {
            let p = plan(&a, &rows).unwrap();
            (crate::json::to_line(&p.op), p.what, p.row, p.then)
        };
        let new = say(Action::New("h".into(), Harness::Claude));
        assert_eq!(
            new,
            (r#"{"op":"new"}"#.into(), "starting new session", None, None)
        );
        let shell = say(Action::Shell("h".into()));
        assert_eq!(
            shell,
            (r#"{"op":"shell"}"#.into(), "starting shell", None, None)
        );
        let (op, what, row, then) = say(Action::Archive(2));
        assert!(
            op.starts_with(r#"{"key":"#) && op.ends_with(r#","op":"close"}"#),
            "{op}"
        );
        assert_eq!((what, row, then), ("archiving session", Some(2), None));
        let (op, what, _, then) = say(Action::Offload(2));
        assert!(
            op.starts_with(r#"{"key":"#) && op.ends_with(r#","op":"offload"}"#),
            "{op}"
        );
        assert_eq!((what, then), ("offloading session", None));
        assert_eq!(plan(&Action::Quit, &rows), None);
    }

    #[test]
    fn only_a_shell_is_tried_where_the_agent_is_not_answering() {
        let shell = Action::Shell("h".into());
        assert_eq!(route(Some(7), &shell), Route::Agent(7));
        assert_eq!(route(Some(7), &Action::Offload(0)), Route::Agent(7));
        assert_eq!(route(None::<u8>, &shell), Route::Shell);
        assert_eq!(route(None::<u8>, &Action::Offload(0)), Route::Unreachable);
    }

    fn open_plan() -> Plan {
        plan(&Action::Open(0), &[row("x", 1)]).unwrap()
    }

    #[test]
    fn work_that_ends_while_it_has_the_terminal_hands_it_back() {
        let mut m = menu(vec![row("x", 1)]);
        let mut t = Fake::new(vec![]);
        let job: work::Job = Box::new(|term| {
            term.suspend().unwrap();
            Outcome::Back(Some("the session ended".into()))
        });
        let done = finish(
            &mut t,
            &mut m,
            &mut None,
            Work::spawn(job),
            &open_plan(),
            "local",
        );
        assert_eq!(done.unwrap(), Done::Changed);
        assert_eq!(t.handed, ["suspend", "resume"]);
        assert_eq!(m.view().status.as_deref(), Some("the session ended"));
    }

    #[test]
    fn q_quits_while_work_runs_and_a_resize_is_taken_meanwhile() {
        let slow = |ms| -> work::Job {
            Box::new(move |_| {
                std::thread::sleep(Duration::from_millis(ms));
                Outcome::Back(None)
            })
        };
        let mut m = menu(vec![row("x", 1)]);
        let mut t = Fake::new(vec![quit()]);
        let done = finish(
            &mut t,
            &mut m,
            &mut None,
            Work::spawn(slow(300)),
            &open_plan(),
            "local",
        );
        assert_eq!(done.unwrap(), Done::Quit);

        let mut t = Fake::new(vec![keys(&[Key::Resize])]);
        t.size = (40, 24);
        let done = finish(
            &mut t,
            &mut m,
            &mut None,
            Work::spawn(slow(300)),
            &open_plan(),
            "local",
        );
        assert_eq!(done.unwrap(), Done::Changed);
        assert_eq!(m.view().width, 40);
    }
}
