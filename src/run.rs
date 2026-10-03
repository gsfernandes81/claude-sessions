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

use crate::clock;
use crate::launch;
use crate::mem;
use crate::menu::{self, Action, Menu};
use crate::render;
use crate::term::{self, Input, RawTerminal};
use crate::ui::{Header, Key, Outcome, TooNarrow};
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
                    if act(term, menu, action) {
                        // A child had the terminal: the alternate screen came back blank.
                        last = None;
                        let rows = menu::gather(clock::now(), menu.workspace());
                        menu.replace_rows(rows, false);
                        menu.set_header(header());
                    }
                }
            }
        }
    }
}

/// Do what a key asked. Returns whether the terminal was handed to something else, so the
/// loop knows to redraw from nothing.
fn act(term: &mut RawTerminal, menu: &mut Menu, action: Action) -> bool {
    let ws = menu.workspace().to_string();
    let rows = menu.rows().to_vec();
    let (outcome, then) = match action {
        Action::None | Action::Redraw | Action::Quit => return false,
        Action::Open(i) => (launch::open(&rows, i, &ws, term), Some(i)),
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
    !matches!(action, Action::Close(_))
}
