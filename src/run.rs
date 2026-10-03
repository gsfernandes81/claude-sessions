//! The menu's loop: read, decide, draw — and draw only what changed.
//!
//! **An idle menu emits zero bytes** (`CLAUDE.md`; the link is metered). Three things keep it
//! so, and each is load-bearing:
//!
//! - **A frame is written only when it differs from the last one written.** Every wake-up
//!   renders, compares, and usually writes nothing.
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
use crate::ui::{Header, Key, Outcome, RowKey, TooNarrow};
use std::time::{Duration, Instant};

/// How often the registry and abduco are looked at while nothing else happens.
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
pub fn run() -> Result<(), String> {
    let (width, height) = term::size().ok_or("not a terminal")?;
    let workspace = menu::workspace();
    let rows = menu::gather(clock::now(), &workspace);
    let mut menu = Menu::new(width, height, header(), workspace, rows);
    // Refuse before touching the terminal: a door that falls through to a shell should find
    // the terminal exactly as the login left it.
    if let Err(t) = render::render(&menu.view()) {
        return Err(too_narrow(&t));
    }
    let mut term = RawTerminal::enter().map_err(|e| format!("cannot set up the terminal: {e}"))?;
    let result = drive(&mut term, &mut menu);
    drop(term);
    result
}

fn drive(term: &mut RawTerminal, menu: &mut Menu) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let mut last: Option<String> = None;
    let mut minute = menu::age_clock(clock::now());
    let mut polled = Instant::now();
    loop {
        match render::render(&menu.view()) {
            Ok(frame) => {
                let out = frame.ansi();
                if last.as_deref() != Some(out.as_str()) {
                    if last.is_none() {
                        term.write_all(CLEAR).map_err(io)?;
                    }
                    term.write_all(out.as_bytes()).map_err(io)?;
                    last = Some(out);
                }
            }
            Err(t) => return Err(too_narrow(&t)),
        }

        let wait = POLL.saturating_sub(polled.elapsed());
        match term.wait(Some(wait)).map_err(io)? {
            Input::Timeout => {
                polled = Instant::now();
                let now = clock::now();
                let before = menu.rows().to_vec();
                let rows = menu::gather(now, menu.workspace());
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
                        last = None;
                    }
                    let action = menu.key(key);
                    if action == Action::Quit {
                        return Ok(());
                    }
                    let done = act(term, menu, action);
                    if done == Done::HandedOver {
                        // A child had the terminal: the alternate screen came back blank.
                        last = None;
                    }
                    if done != Done::Nothing {
                        let rows = menu::gather(clock::now(), menu.workspace());
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
    /// The registry or the archive changed: read the rows again.
    Changed,
    /// The terminal was handed to something else: read the rows again and redraw from
    /// nothing.
    HandedOver,
}

/// Do what a key asked.
fn act(term: &mut RawTerminal, menu: &mut Menu, action: Action) -> Done {
    let ws = menu.workspace().to_string();
    let rows = menu.rows().to_vec();
    let id = |i: usize| match rows.get(i).map(|r| &r.key) {
        Some(RowKey::Conversation { id, .. }) => Some(id.clone()),
        _ => None,
    };
    let (outcome, then) = match action {
        Action::None | Action::Redraw | Action::Quit => return Done::Nothing,
        Action::Archive(i) | Action::Unarchive(i) => {
            let archiving = matches!(action, Action::Archive(_));
            let Some(id) = id(i) else {
                return Done::Nothing;
            };
            let mark = if archiving {
                Mark::Archived
            } else {
                Mark::Kept
            };
            menu.set_status(Some(match archive::set(&id, mark) {
                Ok(()) if archiving => "archived · c under Archived undoes it".into(),
                Ok(()) => "back under Closed".into(),
                Err(e) => format!("could not change the archive: {e}"),
            }));
            return Done::Changed;
        }
        Action::Open(i) => {
            // Resumed, it leaves the archive; marked kept first, so a resume that fails does
            // not leave it folded away again by its age.
            if rows.get(i).is_some_and(|r| r.archived) {
                if let Some(id) = id(i) {
                    let _ = archive::set(&id, Mark::Kept);
                }
            }
            (launch::open(&rows, i, &ws, term), Some(i))
        }
        Action::New => (launch::new_session(&rows, &ws, term), None),
        Action::Shell => (launch::shell(&ws, term), None),
        Action::Close(i) => (launch::close(&rows, i), None),
        Action::OffloadThenOpen { victim, then } => (
            launch::offload_then_open(&rows, victim, then, &ws, term),
            then,
        ),
    };
    match outcome {
        Outcome::Back(status) => menu.set_status(status),
        Outcome::Refused(why) => menu.set_status(Some(why)),
        Outcome::NoRoom(dialog) => menu.ask_no_room(dialog, then),
        Outcome::ResumeFailed(dialog) => menu.ask_resume_failed(dialog, then.unwrap_or(0)),
    }
    // Close never hands the terminal over; everything else might have.
    if matches!(action, Action::Close(_)) {
        Done::Changed
    } else {
        Done::HandedOver
    }
}
