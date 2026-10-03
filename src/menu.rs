//! The menu's state: which rows there are, where the cursor is, what is being asked, and
//! what a key does to all of that. Pure, apart from [`gather`], which reads the registry and
//! abduco — so every key in `docs/design.md` § *The menu* is a test here rather than
//! something to try by hand over ssh.
//!
//! Two behaviours that are decisions, not accidents:
//!
//! **Rows keep their places while the menu is open.** The order is wants-you, then unread,
//! then most recent — once, when the menu starts. After that a row stays where it was and a
//! new row joins at the bottom. Mockup 6 is the evidence: back from slot 2, row 2 now reads
//! `now` and is no longer unread, yet it is still second, above unread row 3. Re-sorting under
//! the cursor would move the row the owner is reaching for.
//!
//! **Ages are measured from a clock floored to the minute.** Each row's age would otherwise
//! roll over at its own second of the minute, and six rows could redraw six times a minute.
//! Floored, every age changes together, once a minute at most — the rule in `CLAUDE.md`, on
//! a metered link.

use crate::abduco;
use crate::clock::Millis;
use crate::fmt;
use crate::live;
use crate::procinfo;
use crate::registry::{self, SlotRecord, State};
use crate::ui::{Dialog, Header, Key, Row, RowKey, Screen, View};

/// What the loop should do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing changed that a frame would show.
    None,
    /// The view changed; draw it.
    Redraw,
    Quit,
    /// Open the row at this index: attach if live, resume if offloaded.
    Open(usize),
    New,
    Shell,
    /// Close the row at this index — the owner has said `y`.
    Close(usize),
    /// Mockup 4's `y`: offload `victim`, then open `then` (or start a new slot when `None`).
    OffloadThenOpen {
        victim: usize,
        then: Option<usize>,
    },
}

/// A dialog, plus what answering it needs that the drawing does not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    Close {
        index: usize,
    },
    NoRoom {
        then: Option<usize>,
        victim: Option<usize>,
    },
    ResumeFailed {
        index: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub width: u16,
    pub height: u16,
    header: Header,
    workspace: String,
    rows: Vec<Row>,
    cursor: usize,
    scroll: usize,
    screen: Screen,
    ask: Option<(Ask, Dialog)>,
    status: Option<String>,
}

impl Menu {
    pub fn new(width: u16, height: u16, header: Header, workspace: String, rows: Vec<Row>) -> Menu {
        let mut m = Menu {
            width,
            height,
            header,
            workspace,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            screen: Screen::List,
            ask: None,
            status: None,
        };
        m.replace_rows(rows, true);
        m
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn set_header(&mut self, header: Header) {
        self.header = header;
    }

    pub fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    /// Take a fresh reading of the rows, keeping the order the owner has been looking at.
    ///
    /// `first` sorts — wants-you, unread, then whatever order `gather` produced, which is
    /// most recent first. Afterwards, rows that are still there keep their places, rows that
    /// are gone go, and new ones join at the bottom. The cursor stays on the row it was on.
    pub fn replace_rows(&mut self, fresh: Vec<Row>, first: bool) {
        let under_cursor = self.rows.get(self.cursor).map(|r| r.key.clone());
        let mut next: Vec<Row> = Vec::with_capacity(fresh.len());
        if first {
            next = fresh;
            // Stable, so `gather`'s recency order survives within each group. Closed rows go
            // last: they are history, reachable but never in the way.
            next.sort_by_key(|r| (r.closed, !r.wants_you, !r.unread));
        } else {
            for old in &self.rows {
                if let Some(f) = fresh.iter().find(|f| f.key == old.key) {
                    next.push(f.clone());
                }
            }
            for f in fresh {
                if !next.iter().any(|n| n.key == f.key) {
                    next.push(f);
                }
            }
        }
        self.rows = next;
        self.cursor = under_cursor
            .and_then(|k| self.rows.iter().position(|r| r.key == k))
            .unwrap_or(self.cursor)
            .min(self.rows.len().saturating_sub(1));
        // A dialog about a row that has gone is a question about nothing.
        if let Some((ask, _)) = &self.ask {
            let index = match ask {
                Ask::Close { index } | Ask::ResumeFailed { index } => Some(*index),
                Ask::NoRoom { .. } => None,
            };
            if index.is_some_and(|i| i >= self.rows.len()) {
                self.ask = None;
            }
        }
        self.keep_cursor_visible();
    }

    /// How many lines the hint takes at this width, counted by the renderer that draws it.
    fn hint_lines(&self) -> usize {
        crate::render::hint_line_count(self.width, self.rows.is_empty())
    }

    /// Rows that fit between the header's rule and the closing rule.
    fn capacity(&self) -> usize {
        let status = self
            .status
            .as_deref()
            .map_or(0, |s| crate::render::status_line_count(self.width, s));
        let fixed = 3 + status + self.hint_lines();
        usize::from(self.height).saturating_sub(fixed).max(1)
    }

    fn keep_cursor_visible(&mut self) {
        let cap = self.capacity();
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + cap {
            self.scroll = self.cursor + 1 - cap;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(cap));
    }

    pub fn view(&self) -> View {
        let (dialog, scroll) = match &self.ask {
            // A dialog draws two rows above itself, from `scroll`. Show the row the question
            // is about first, and the one after it, as mockups 4 and 5 do — clamped so there
            // are two to show when it is the last row.
            Some((ask, dialog)) => {
                let about = match ask {
                    Ask::Close { index } | Ask::ResumeFailed { index } => *index,
                    Ask::NoRoom { victim, then } => victim.or(*then).unwrap_or(self.cursor),
                };
                (
                    Some(dialog.clone()),
                    about.min(self.rows.len().saturating_sub(2)),
                )
            }
            None => (None, self.scroll),
        };
        View {
            width: self.width,
            height: self.height,
            header: self.header.clone(),
            workspace: self.workspace.clone(),
            rows: self.rows.clone(),
            cursor: self.cursor,
            scroll,
            screen: self.screen,
            dialog,
            status: self.status.clone(),
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.keep_cursor_visible();
    }

    /// Ask the owner something a launch came back with (mockups 4 and 5).
    pub fn ask_no_room(&mut self, dialog: Dialog, then: Option<usize>) {
        let victim = match &dialog {
            Dialog::NoRoom {
                offer: Some((row, _, _)),
                ..
            } => Some(row.saturating_sub(1)),
            _ => None,
        };
        self.ask = Some((Ask::NoRoom { then, victim }, dialog));
    }

    pub fn ask_resume_failed(&mut self, dialog: Dialog, index: usize) {
        self.ask = Some((Ask::ResumeFailed { index }, dialog));
    }

    fn move_to(&mut self, index: usize) -> Action {
        if self.rows.is_empty() {
            return Action::None;
        }
        let index = index.min(self.rows.len() - 1);
        if index == self.cursor {
            return Action::None;
        }
        self.cursor = index;
        self.keep_cursor_visible();
        Action::Redraw
    }

    fn ask_close(&mut self, index: usize) -> Action {
        let Some(row) = self.rows.get(index) else {
            return Action::None;
        };
        let dialog = Dialog::Close {
            row: index + 1,
            title: row.title.clone(),
            running: !row.offloaded,
        };
        self.ask = Some((Ask::Close { index }, dialog));
        Action::Redraw
    }

    /// What a key does. Every key the design lists, and nothing else: an unknown key does
    /// nothing at all rather than something surprising.
    pub fn key(&mut self, key: Key) -> Action {
        // A status line is news about the last thing that happened; the next key is the
        // owner moving on.
        let had_status = self.status.take().is_some();
        let act = self.key_inner(key);
        match act {
            Action::None if had_status => Action::Redraw,
            other => other,
        }
    }

    fn key_inner(&mut self, key: Key) -> Action {
        // `q` quits from anywhere and is listed nowhere (owner, 2026-10-01). Ctrl-C does too:
        // raw mode delivers it as a byte rather than a signal, and a menu that ignored the
        // one key everybody presses to get out would feel like a hang.
        if matches!(key, Key::Char('q') | Key::Char('\u{3}')) {
            return Action::Quit;
        }
        if let Some((ask, _)) = self.ask.clone() {
            return self.answer(ask, key);
        }
        if self.screen == Screen::Keys {
            return match key {
                Key::Char('?') | Key::Esc => {
                    self.screen = Screen::List;
                    Action::Redraw
                }
                Key::Resize => Action::Redraw,
                _ => Action::None,
            };
        }
        let page = self.capacity().max(1);
        match key {
            Key::Up | Key::WheelUp => self.move_to(self.cursor.saturating_sub(1)),
            Key::Down | Key::WheelDown => self.move_to(self.cursor + 1),
            Key::PageUp => self.move_to(self.cursor.saturating_sub(page)),
            Key::PageDown => self.move_to(self.cursor + page),
            Key::Home => self.move_to(0),
            Key::End => self.move_to(usize::MAX),
            Key::Click { row, .. } => match self.row_at(row) {
                Some(i) => self.move_to(i),
                None => Action::None,
            },
            Key::Enter if !self.rows.is_empty() => Action::Open(self.cursor),
            Key::Char('n') => Action::New,
            Key::Char('s') => Action::Shell,
            Key::Char('c') if self.rows.get(self.cursor).is_some_and(|r| r.closed) => {
                self.status = Some(format!("{} is already closed", self.cursor + 1));
                Action::Redraw
            }
            Key::Char('c') if !self.rows.is_empty() => self.ask_close(self.cursor),
            Key::Char('?') => {
                self.screen = Screen::Keys;
                Action::Redraw
            }
            Key::Esc => Action::Quit,
            Key::Resize => Action::Redraw,
            _ => Action::None,
        }
    }

    /// Which row a click on screen line `line` landed on: rows start on the third line, after
    /// the header and its rule.
    fn row_at(&self, line: u16) -> Option<usize> {
        let offset = usize::from(line).checked_sub(2)?;
        if offset >= self.capacity() {
            return None;
        }
        let index = self.scroll + offset;
        (index < self.rows.len()).then_some(index)
    }

    fn answer(&mut self, ask: Ask, key: Key) -> Action {
        let dismiss = |m: &mut Menu| {
            m.ask = None;
            Action::Redraw
        };
        match (ask, key) {
            (_, Key::Resize) => Action::Redraw,
            (Ask::Close { index }, Key::Char('y')) => {
                self.ask = None;
                Action::Close(index)
            }
            (Ask::Close { .. }, Key::Char('n') | Key::Esc) => dismiss(self),
            (
                Ask::NoRoom {
                    then,
                    victim: Some(victim),
                },
                Key::Char('y'),
            ) => {
                self.ask = None;
                Action::OffloadThenOpen { victim, then }
            }
            (Ask::NoRoom { .. }, Key::Char('n') | Key::Esc) => dismiss(self),
            (Ask::ResumeFailed { index }, Key::Char('r')) => {
                self.ask = None;
                Action::Open(index)
            }
            (Ask::ResumeFailed { index }, Key::Char('c')) => self.ask_close(index),
            (Ask::ResumeFailed { .. }, Key::Esc) => dismiss(self),
            _ => Action::None,
        }
    }
}

/// The clock the list's ages are measured from: now, floored to the minute. See the module
/// note for why.
pub fn age_clock(now: Millis) -> Millis {
    now - now % 60_000
}

/// Read the rows: every open slot in the registry, plus every abduco session the registry
/// has never heard of. In recency order; `Menu::replace_rows` does the grouping.
pub fn gather(now: Millis) -> Vec<Row> {
    let clock = age_clock(now);
    let recs = registry::all().unwrap_or_default();
    let sockets = abduco::sockets();
    let lives = live::all();
    // Closed slots too (owner, 2026-10-03): listed at the bottom, so a conversation that
    // ran in a slot can be resumed from the menu after it was closed.
    let mut slots: Vec<(&SlotRecord, Row)> = recs
        .iter()
        .map(|r| (r, slot_row(r, now, clock, &sockets, &lives)))
        .collect();
    slots.sort_by_key(|(r, _)| std::cmp::Reverse(r.last_activity_ms));
    let mut rows: Vec<Row> = slots.into_iter().map(|(_, row)| row).collect();
    for sock in sockets {
        if recs.iter().any(|r| r.slot == sock.name) {
            continue;
        }
        // A socket's own age is the only clock a session with no record has.
        let since = std::fs::metadata(&sock.path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as Millis)
            .unwrap_or(clock);
        rows.push(Row {
            key: RowKey::Socket(sock.name.clone()),
            wants_you: false,
            unread: false,
            timer: false,
            attached: sock.attached_bit,
            offloaded: false,
            unregistered: true,
            closed: false,
            title: sock.name.clone(),
            age: fmt::age(clock.saturating_sub(since)),
        });
    }
    rows
}

fn slot_row(
    r: &SlotRecord,
    now: Millis,
    clock: Millis,
    sockets: &[abduco::Socket],
    lives: &[live::LiveSession],
) -> Row {
    let alive = matches!((r.pid, r.proc_start), (Some(p), Some(s)) if procinfo::is_alive(p, s));
    let attached = alive && sockets.iter().any(|s| s.name == r.slot && s.attached_bit);
    let live_title = r
        .pid
        .filter(|_| alive)
        .and_then(|pid| lives.iter().find(|l| l.pid == pid))
        .and_then(|l| l.real_title().map(str::to_string));
    Row {
        key: RowKey::Slot(r.slot.clone()),
        wants_you: r.needs_you,
        unread: r.unread(),
        timer: r.has_pending_timer(now),
        attached,
        // A record whose process has gone is resumable whatever its state says — reconcile
        // has not caught up yet. One with no pid at all is a slot just started, waiting for
        // its SessionStart, and is not.
        offloaded: matches!(r.state, State::Offloaded | State::Offloading)
            || (r.state == State::Live && r.pid.is_some() && !alive),
        unregistered: !r.registered,
        closed: r.state == State::Closed,
        title: live_title
            .or_else(|| r.title.clone())
            .or_else(|| r.first_prompt.clone())
            .unwrap_or_else(|| "(no title yet)".into()),
        age: fmt::age(clock.saturating_sub(r.last_activity_ms)),
    }
}

/// The container's name, as the header shows it: its hostname.
pub fn host() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this container".into())
}

/// Where `n` and `s` start: `CLAUDE_SESSIONS_WORKSPACE`, else `/workspace` where it exists
/// (it does in every dev container this runs in), else home.
pub fn workspace() -> String {
    if let Ok(w) = std::env::var("CLAUDE_SESSIONS_WORKSPACE") {
        if !w.is_empty() {
            return w;
        }
    }
    if std::path::Path::new("/workspace").is_dir() {
        return "/workspace".into();
    }
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> Row {
        Row {
            key: RowKey::Slot(name.into()),
            wants_you: false,
            unread: false,
            timer: false,
            attached: false,
            offloaded: false,
            unregistered: false,
            closed: false,
            title: name.into(),
            age: "1m".into(),
        }
    }

    fn header() -> Header {
        Header {
            host: "infra-dev".into(),
            memory: None,
        }
    }

    fn menu(rows: Vec<Row>) -> Menu {
        Menu::new(40, 24, header(), "/workspace".into(), rows)
    }

    fn names(m: &Menu) -> Vec<String> {
        m.rows().iter().map(|r| r.title.clone()).collect()
    }

    #[test]
    fn the_first_reading_sorts_wants_you_then_unread_then_recency() {
        let mut a = row("a");
        let mut b = row("b");
        b.unread = true;
        let c = row("c");
        let mut d = row("d");
        d.wants_you = true;
        // Gathered most-recent first: a, b, c, d.
        let m = menu(vec![a.clone(), b.clone(), c, d.clone()]);
        assert_eq!(names(&m), ["d", "b", "a", "c"]);
        // Calibration: with nothing flagged the recency order is untouched.
        a.unread = false;
        b.unread = false;
        d.wants_you = false;
        assert_eq!(names(&menu(vec![a, b, row("c"), d])), ["a", "b", "c", "d"]);
    }

    #[test]
    fn closed_rows_go_last_and_c_on_one_only_says_so() {
        let mut old = row("old");
        old.closed = true;
        let mut waiting = row("waiting");
        waiting.wants_you = true;
        // Gathered most recent first, the closed one most recent of all.
        let mut m = menu(vec![old, row("a"), waiting]);
        assert_eq!(
            names(&m),
            ["waiting", "a", "old"],
            "closed is history: last"
        );
        m.key(Key::End);
        assert_eq!(m.key(Key::Char('c')), Action::Redraw);
        assert_eq!(
            m.view().dialog,
            None,
            "nothing to ask: it is already closed"
        );
        assert_eq!(m.view().status.as_deref(), Some("3 is already closed"));
        assert_eq!(m.key(Key::Enter), Action::Open(2), "Enter resumes it");
    }

    #[test]
    fn later_readings_keep_places_and_add_new_rows_at_the_bottom() {
        let mut two = row("two");
        two.unread = true;
        let mut m = menu(vec![row("one"), two.clone(), row("three")]);
        assert_eq!(names(&m), ["two", "one", "three"]);
        // Mockup 6: back from "two", it is no longer unread and is the most recent — and it
        // stays where it was.
        two.unread = false;
        m.replace_rows(vec![two, row("four"), row("one")], false);
        assert_eq!(
            names(&m),
            ["two", "one", "four"],
            "three went, four joined at the bottom, nothing moved"
        );
    }

    #[test]
    fn the_cursor_follows_its_row_across_a_reading() {
        let mut m = menu(vec![row("a"), row("b"), row("c")]);
        m.key(Key::Down);
        m.key(Key::Down);
        assert_eq!(m.view().cursor, 2);
        m.replace_rows(vec![row("b"), row("c")], false);
        assert_eq!(m.rows()[m.view().cursor].title, "c");
    }

    #[test]
    fn arrows_and_wheel_move_the_cursor_and_stop_at_the_ends() {
        let mut m = menu(vec![row("a"), row("b")]);
        assert_eq!(m.key(Key::Up), Action::None, "already at the top");
        assert_eq!(m.key(Key::Down), Action::Redraw);
        assert_eq!(m.key(Key::Down), Action::None, "already at the bottom");
        assert_eq!(m.key(Key::WheelUp), Action::Redraw);
        assert_eq!(m.view().cursor, 0);
        assert_eq!(m.key(Key::End), Action::Redraw);
        assert_eq!(m.view().cursor, 1);
    }

    #[test]
    fn a_click_on_a_row_moves_the_cursor_there() {
        let mut m = menu(vec![row("a"), row("b"), row("c")]);
        // Line 0 is the header, line 1 its rule, rows from line 2.
        assert_eq!(m.key(Key::Click { row: 4, col: 10 }), Action::Redraw);
        assert_eq!(m.view().cursor, 2);
        assert_eq!(
            m.key(Key::Click { row: 0, col: 3 }),
            Action::None,
            "the header"
        );
        assert_eq!(
            m.key(Key::Click { row: 9, col: 3 }),
            Action::None,
            "below the last row"
        );
        assert_eq!(m.view().cursor, 2);
    }

    #[test]
    fn enter_opens_the_highlighted_row() {
        let mut m = menu(vec![row("a"), row("b")]);
        m.key(Key::Down);
        assert_eq!(m.key(Key::Enter), Action::Open(1));
        assert_eq!(
            menu(vec![]).key(Key::Enter),
            Action::None,
            "nothing to open"
        );
    }

    #[test]
    fn the_keys_the_design_lists() {
        let mut m = menu(vec![row("a")]);
        assert_eq!(m.key(Key::Char('n')), Action::New);
        assert_eq!(m.key(Key::Char('s')), Action::Shell);
        assert_eq!(m.key(Key::Esc), Action::Quit);
        assert_eq!(m.key(Key::Char('q')), Action::Quit);
        assert_eq!(
            m.key(Key::Char('x')),
            Action::None,
            "an unknown key does nothing"
        );
        assert_eq!(m.key(Key::Char('?')), Action::Redraw);
        assert_eq!(m.view().screen, Screen::Keys);
        assert_eq!(
            m.key(Key::Char('n')),
            Action::None,
            "the keys screen is only read"
        );
        assert_eq!(
            m.key(Key::Esc),
            Action::Redraw,
            "Esc leaves the keys screen"
        );
        assert_eq!(m.view().screen, Screen::List);
        m.key(Key::Char('?'));
        assert_eq!(m.key(Key::Char('q')), Action::Quit, "q quits from anywhere");
        assert_eq!(
            m.key(Key::Char('\u{3}')),
            Action::Quit,
            "and so does Ctrl-C"
        );
    }

    #[test]
    fn closing_asks_first_and_only_y_closes() {
        let mut m = menu(vec![row("a"), row("b")]);
        m.key(Key::Down);
        assert_eq!(m.key(Key::Char('c')), Action::Redraw);
        assert!(matches!(
            m.view().dialog,
            Some(Dialog::Close {
                row: 2,
                running: true,
                ..
            })
        ));
        assert_eq!(m.key(Key::Enter), Action::None, "only y or n answer it");
        assert_eq!(m.key(Key::Char('n')), Action::Redraw);
        assert_eq!(m.view().dialog, None, "n keeps it");
        m.key(Key::Char('c'));
        assert_eq!(m.key(Key::Char('y')), Action::Close(1));
        assert_eq!(m.view().dialog, None);
    }

    #[test]
    fn closing_an_offloaded_row_says_it_is_not_running() {
        let mut z = row("z");
        z.offloaded = true;
        let mut m = menu(vec![z]);
        m.key(Key::Char('c'));
        assert!(matches!(
            m.view().dialog,
            Some(Dialog::Close { running: false, .. })
        ));
    }

    #[test]
    fn no_room_y_offloads_the_offer_then_opens() {
        let mut m = menu(vec![row("a"), row("b"), row("c")]);
        let dialog = Dialog::NoRoom {
            used: 1,
            limit: 2,
            want: 1,
            offer: Some((2, "2d".into(), "b".into())),
        };
        m.ask_no_room(dialog.clone(), Some(2));
        assert_eq!(
            m.view().scroll,
            1,
            "the two rows above the dialog start at the one it offers"
        );
        assert_eq!(
            m.key(Key::Char('y')),
            Action::OffloadThenOpen {
                victim: 1,
                then: Some(2)
            }
        );
        // With nothing to offer, y does nothing and n cancels.
        m.ask_no_room(
            Dialog::NoRoom {
                used: 1,
                limit: 2,
                want: 1,
                offer: None,
            },
            None,
        );
        assert_eq!(m.key(Key::Char('y')), Action::None);
        assert_eq!(m.key(Key::Char('n')), Action::Redraw);
        assert_eq!(m.view().dialog, None);
    }

    #[test]
    fn a_failed_resume_offers_retry_close_and_back() {
        let mut m = menu(vec![row("a"), row("b")]);
        let failed = Dialog::ResumeFailed {
            row: 2,
            session: "0f9c4a1e".into(),
            status: 1,
            output: vec![],
        };
        m.ask_resume_failed(failed.clone(), 1);
        assert_eq!(m.key(Key::Char('r')), Action::Open(1));
        m.ask_resume_failed(failed.clone(), 1);
        assert_eq!(m.key(Key::Char('c')), Action::Redraw);
        assert!(matches!(
            m.view().dialog,
            Some(Dialog::Close { row: 2, .. })
        ));
        m.key(Key::Esc);
        m.ask_resume_failed(failed, 1);
        assert_eq!(m.key(Key::Esc), Action::Redraw);
        assert_eq!(m.view().dialog, None);
    }

    #[test]
    fn a_status_line_goes_with_the_next_key() {
        let mut m = menu(vec![row("a")]);
        m.set_status(Some("detached from 1 · it is still running".into()));
        assert!(m.view().status.is_some());
        assert_eq!(
            m.key(Key::Up),
            Action::Redraw,
            "nothing moved, but the status line went"
        );
        assert_eq!(m.view().status, None);
    }

    #[test]
    fn the_cursor_stays_on_screen_in_a_short_terminal() {
        let rows: Vec<Row> = (0..30).map(|i| row(&format!("r{i}"))).collect();
        let mut m = Menu::new(40, 10, header(), "/w".into(), rows);
        // 10 lines: header, rule, closing rule, two hint lines at 40 columns → 5 rows.
        assert_eq!(m.capacity(), 5);
        for _ in 0..7 {
            m.key(Key::Down);
        }
        let v = m.view();
        assert_eq!(v.cursor, 7);
        assert!(
            v.scroll <= 7 && 7 < v.scroll + 5,
            "cursor 7 not within scroll {}",
            v.scroll
        );
        m.key(Key::Home);
        assert_eq!(m.view().scroll, 0);
    }

    #[test]
    fn the_hint_line_count_matches_mockup_8() {
        let mut m = menu(vec![row("a")]);
        for (width, lines) in [(40, 2), (34, 2), (26, 2), (18, 3), (12, 5), (9, 4), (7, 4)] {
            m.resize(width, 24);
            assert_eq!(m.hint_lines(), lines, "at {width} columns");
        }
    }

    #[test]
    fn ages_are_measured_from_the_minute() {
        assert_eq!(age_clock(125_000), 120_000);
        assert_eq!(age_clock(120_000), 120_000);
        // Two rows whose ages would roll at different seconds roll together.
        let a = 10_000;
        let b = 50_000;
        let now = 61_000 + 20_000; // 81 s: a is 71 s old, b is 31 s old
        assert_eq!(fmt::age(age_clock(now) - a), "now", "floored: 50 s");
        assert_eq!(fmt::age(age_clock(now).saturating_sub(b)), "now");
        assert_eq!(
            fmt::age(age_clock(now + 39_000) - a),
            "1m",
            "both roll at 120 s"
        );
    }
}
