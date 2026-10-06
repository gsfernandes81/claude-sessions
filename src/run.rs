//! The menu's loop: read, decide, draw — and draw only what changed.
//!
//! **An idle menu emits zero bytes** (`CLAUDE.md`; the link is metered). Three things keep it
//! so, and each is load-bearing:
//!
//! - **Only the lines that differ from what is on the screen are written.** Every wake-up
//!   renders, compares line by line, and usually writes nothing; a spinner turning writes the
//!   line or two it is on.
//! - **The registry is re-read every [`POLL`]**, which costs a few `stat`s and reads and no
//!   bytes on the link unless a row actually changed.
//! - **The header's memory figure is refreshed only with something else.** Memory moves by a
//!   megabyte every few seconds on a busy box; refreshed on its own it would redraw the screen
//!   that often. It is brought up to date when rows change, when a key is pressed, and when
//!   the minute turns — the one tick the ages are allowed.

use crate::archive::{self, Mark};
use crate::clock;
use crate::launch;
use crate::mem;
use crate::menu::{self, Action, Menu};
use crate::render;
use crate::term::{self, Input, RawTerminal};
use crate::ui::{Busy, Header, Key, Outcome, RowKey, Terminal, TooNarrow};
use crate::work::{self, Event, Spin, Work};
use crate::zmx;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How often the registry is looked at while nothing else happens. zmx is looked at less
/// often: see `zmx::Watch`.
const POLL: Duration = Duration::from_secs(2);

/// Clear the screen, for the first frame and after anything that leaves it in an unknown
/// state — a child that had the terminal, a resize.
const CLEAR: &[u8] = b"\x1b[2J";

fn header() -> Header {
    let m = mem::read();
    Header {
        host: menu::host(),
        memory: m.limit.zip(m.current).map(|(limit, cur)| (cur, limit)),
    }
}

fn too_narrow(t: &TooNarrow) -> String {
    format!(
        "the terminal is {} columns wide; the menu needs at least {}",
        t.width, t.need
    )
}

/// Run the menu until the owner quits. `Err` carries why it could not run — the door prints
/// it and falls through to a login shell.
///
/// **The first frame does not wait for the list.** The list is read on its own thread; given
/// a quarter of a second it is usually done and the first frame is the list. If not, the
/// frame is drawn with a spinner where the list will go (mockup 10), and `n`, `s` and `?`
/// work while it is read.
pub fn run() -> Result<(), Stop> {
    let (width, height) = term::size().ok_or_else(|| Stop::Failed("not a terminal".into()))?;
    let workspace = menu::workspace();
    let first = {
        let (tx, rx) = mpsc::channel();
        let ws = workspace.clone();
        std::thread::spawn(move || {
            let sessions = zmx::sessions().unwrap_or_default();
            let _ = tx.send(menu::gather(clock::now(), &ws, &sessions));
        });
        rx
    };
    let started = Instant::now();
    let rows = first.recv_timeout(work::SHOW_AFTER).ok();
    let loading = rows.is_none();
    let mut menu = Menu::new(width, height, header(), workspace, rows.unwrap_or_default());
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
    let result = drive(&mut term, &mut menu, loading.then_some((first, spin)));
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
    mut first: Option<(Receiver<Vec<crate::ui::Row>>, Spin)>,
) -> Result<(), Stop> {
    let mut screen: Screen = None;
    let mut minute = menu::age_clock(clock::now());
    let mut polled = Instant::now();
    let mut zmx = zmx::Watch::default();
    loop {
        // The first reading, if it was not ready for the first frame.
        if let Some((rx, spin)) = first.as_mut() {
            match rx.try_recv() {
                Ok(rows) => {
                    menu.set_busy(None);
                    menu.replace_rows(rows, true);
                    first = None;
                    polled = Instant::now();
                }
                Err(_) => {
                    if let Some(frame) = spin.tick(Instant::now()) {
                        menu.set_busy(Some(reading(frame)));
                    }
                }
            }
        }
        draw(term, menu, &mut screen)?;

        let now = Instant::now();
        let wait = match first.as_ref() {
            Some((_, spin)) => spin.until_next(now).min(CHECK),
            None => POLL.saturating_sub(polled.elapsed()),
        };
        match term.wait(Some(wait)).map_err(io)? {
            Input::Timeout => {
                if first.is_some() || polled.elapsed() < POLL {
                    continue;
                }
                polled = Instant::now();
                let now = clock::now();
                let before = menu.rows().to_vec();
                let rows = menu::gather(now, menu.workspace(), zmx.sessions(false));
                menu.replace_rows(rows, false);
                let turned = menu::age_clock(now) != minute;
                if turned || menu.rows() != before.as_slice() {
                    minute = menu::age_clock(now);
                    menu.set_header(header());
                }
            }
            Input::Keys(keys) => {
                menu.set_header(header());
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
                    let done = act(term, menu, &mut screen, action)?;
                    if done == Done::Quit {
                        return Ok(());
                    }
                    if done != Done::Nothing && first.is_none() {
                        let rows = menu::gather(clock::now(), menu.workspace(), zmx.sessions(true));
                        menu.replace_rows(rows, false);
                        menu.set_header(header());
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
    /// The registry or the archive may have changed: read the rows again.
    Changed,
    /// The owner quit while it was being worked on.
    Quit,
}

/// Do what a key asked, on a worker thread, with a spinner if it takes long enough to need
/// one (mockup 11).
fn act(
    term: &mut RawTerminal,
    menu: &mut Menu,
    screen: &mut Screen,
    action: Action,
) -> Result<Done, Stop> {
    let ws = menu.workspace().to_string();
    let rows = menu.rows().to_vec();
    let id = |i: usize| match rows.get(i).map(|r| &r.key) {
        Some(RowKey::Conversation { id, .. }) => Some(id.clone()),
        _ => None,
    };
    // What the spinner says, and the row it stands on. No title: the row says which (owner,
    // 2026-10-04).
    let resumes = |i: usize| rows.get(i).is_some_and(|r| r.offloaded || r.closed);
    let (row, what): (Option<usize>, &str) = match action {
        Action::None | Action::Redraw | Action::Quit => return Ok(Done::Nothing),
        Action::Open(i) if resumes(i) => (Some(i), "resuming session"),
        Action::Open(i) => (Some(i), "opening session"),
        Action::New => (None, "starting new session"),
        Action::Shell => (None, "starting shell"),
        Action::Close(i) => (Some(i), "closing session"),
        Action::OffloadThenOpen { victim, .. } => (Some(victim), "offloading session for room"),
        Action::Archive(i) => (Some(i), "archiving session"),
        Action::Unarchive(i) => (Some(i), "unarchiving session"),
    };
    let then = match action {
        Action::Open(i) => Some(i),
        Action::OffloadThenOpen { then, .. } => then,
        _ => None,
    };
    let job: work::Job = match action {
        Action::None | Action::Redraw | Action::Quit => return Ok(Done::Nothing),
        Action::Archive(i) | Action::Unarchive(i) => {
            let archiving = matches!(action, Action::Archive(_));
            let Some(id) = id(i) else {
                return Ok(Done::Nothing);
            };
            Box::new(move |_: &mut dyn Terminal| {
                let now = clock::now();
                let mark = if archiving {
                    Mark::Archived(now)
                } else {
                    Mark::Kept(now)
                };
                Outcome::Back(Some(match archive::set(&id, mark) {
                    Ok(()) if archiving => "archived · c under Archived undoes it".into(),
                    Ok(()) => "back under Closed".into(),
                    Err(e) => format!("could not change the archive: {e}"),
                }))
            })
        }
        Action::Open(i) => {
            // Resumed, it leaves the archive. Marked kept first, so a resume that fails does
            // not leave it folded straight back by its age; a kept mark lasts 30 days, so a
            // launch that was refused costs no more than that.
            let unarchive = rows
                .get(i)
                .is_some_and(|r| r.archived)
                .then(|| id(i))
                .flatten();
            Box::new(move |t: &mut dyn Terminal| {
                if let Some(id) = unarchive {
                    let _ = archive::set(&id, Mark::Kept(clock::now()));
                }
                launch::open(&rows, i, &ws, t)
            })
        }
        Action::New => Box::new(move |t: &mut dyn Terminal| launch::new_session(&rows, &ws, t)),
        Action::Shell => Box::new(move |t: &mut dyn Terminal| launch::shell(&ws, t)),
        Action::Close(i) => Box::new(move |_: &mut dyn Terminal| launch::close(&rows, i)),
        Action::OffloadThenOpen { victim, then } => Box::new(move |t: &mut dyn Terminal| {
            launch::offload_then_open(&rows, victim, then, &ws, t)
        }),
    };
    let Some(outcome) = wait_on(term, menu, screen, Work::spawn(job), row, what)? else {
        return Ok(Done::Quit);
    };
    match outcome {
        Outcome::Back(status) => menu.set_status(status),
        Outcome::Refused(why) => menu.set_status(Some(why)),
        Outcome::NoRoom(dialog) => menu.ask_no_room(dialog, then),
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
            Some(Event::Done(outcome)) => {
                menu.set_busy(None);
                return Ok(Some(outcome));
            }
            Some(Event::Suspend) => {
                // The child gets a terminal with nothing of ours on it, and we touch the
                // terminal again only when it is handed back.
                menu.set_busy(None);
                let suspended = term.suspend();
                work.ack();
                suspended.map_err(io)?;
                loop {
                    match work.next(Duration::from_secs(3600)) {
                        Some(Event::Resume) => break,
                        Some(Event::Done(outcome)) => {
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
            Some(Event::Resume) => work.ack(),
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
            Input::Timeout => {}
        }
    }
}
