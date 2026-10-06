//! The menu's state: which rows there are, where the cursor is, what is being asked, and
//! what a key does to all of that. Pure, apart from [`gather`], which reads the registry and
//! zmx — so every key in `docs/design.md` § *The menu* is a test here rather than
//! something to try by hand over ssh.
//!
//! Two behaviours that are decisions, not accidents:
//!
//! **Rows are grouped by state, and keep their places within a group while the menu is open**
//! (owner, 2026-10-03). Needs you, Working, Idle, Offloaded, Closed, top to bottom; each in
//! recency order when the menu starts, except Idle, where unread rows come first. After that
//! a row that stays in its group stays where it was, and a row that is new, or has moved
//! group, joins the top of its group. Mockup 6 is the evidence for staying put: back from a
//! session, it reads `now` and is no longer unread, yet it is still where it was. Re-sorting
//! under the cursor would move the row the owner is reaching for; a row that changes group
//! has moved anyway, and the top is where the eye goes to see what changed.
//!
//! The cursor is an index into the rows, never a screen line, so it can only ever be on a
//! session: headings, the blank lines between groups and the `… N more` fold are not rows.
//!
//! **Ages are measured from a clock floored to the minute.** Each row's age would otherwise
//! roll over at its own second of the minute, and six rows could redraw six times a minute.
//! Floored, every age changes together, once a minute at most — the rule in `CLAUDE.md`, on
//! a metered link.

use crate::archive;
use crate::clock::Millis;
use crate::fmt;
use crate::live;
use crate::procinfo;
use crate::registry::{self, SlotRecord, State};
use crate::render;
use crate::store;
use crate::ui::{Busy, Dialog, Group, Header, Key, Row, RowKey, Screen, View};
use crate::zmx;

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
    /// `c` on a closed conversation: put it in the archive. No question asked (owner,
    /// 2026-10-03): nothing is lost, and `c` on it under Archived brings it back.
    Archive(usize),
    /// `c` on an archived conversation: take it out again.
    Unarchive(usize),
}

/// A dialog, plus what answering it needs that the drawing does not. Rows are named by key,
/// not index: a reading taken while the question is up can regroup them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    Close {
        key: RowKey,
    },
    NoRoom {
        then: Option<RowKey>,
        victim: Option<RowKey>,
    },
    ResumeFailed {
        key: RowKey,
    },
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub width: u16,
    pub height: u16,
    header: Header,
    workspace: String,
    /// Every row, in the order the owner has been looking at, archived ones included.
    all: Vec<Row>,
    /// What is drawn and what the cursor moves over: `all`, with the archived rows behind the
    /// Archived heading's row, and shown only while it is open.
    rows: Vec<Row>,
    /// The Archived group is open. Shut whenever the menu starts (owner, 2026-10-03).
    archive_open: bool,
    cursor: usize,
    scroll: usize,
    screen: Screen,
    ask: Option<(Ask, Dialog)>,
    status: Option<String>,
    busy: Option<Busy>,
}

impl Menu {
    pub fn new(width: u16, height: u16, header: Header, workspace: String, rows: Vec<Row>) -> Menu {
        let mut m = Menu {
            width,
            height,
            header,
            workspace,
            all: Vec::new(),
            rows: Vec::new(),
            archive_open: false,
            cursor: 0,
            scroll: 0,
            screen: Screen::List,
            ask: None,
            status: None,
            busy: None,
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

    /// Show, or stop showing, that something is being worked on.
    pub fn set_busy(&mut self, busy: Option<Busy>) {
        self.busy = busy;
        self.keep_cursor_visible();
    }

    /// Take a fresh reading of the rows, keeping the order the owner has been looking at.
    ///
    /// `first` sorts into groups — each in `gather`'s order, which is most recent first,
    /// except Idle, where unread rows come first. Afterwards, rows that are still in the same
    /// group keep their places, rows that are gone go, and rows that are new or have changed
    /// group join the top of theirs. The cursor stays on the row it was on.
    pub fn replace_rows(&mut self, fresh: Vec<Row>, first: bool) {
        let under_cursor = self.rows.get(self.cursor).map(|r| r.key.clone());
        let next: Vec<Row> = if first {
            let mut next = fresh;
            // Stable, so `gather`'s recency order survives within each group.
            next.sort_by_key(|r| (r.group(), r.group() == Group::Idle && !r.unread));
            next
        } else {
            let mut next = Vec::with_capacity(fresh.len());
            for g in Group::ALL {
                let stayed = |f: &Row| {
                    self.all
                        .iter()
                        .any(|old| old.key == f.key && old.group() == g)
                };
                let arrived = fresh.iter().filter(|f| f.group() == g && !stayed(f));
                next.extend(arrived.cloned());
                for old in self.all.iter().filter(|old| old.group() == g) {
                    if let Some(f) = fresh.iter().find(|f| f.key == old.key && f.group() == g) {
                        next.push(f.clone());
                    }
                }
            }
            next
        };
        self.all = next;
        // An archive emptied is shut again, so the group shows shut whenever it reappears.
        if !self.all.iter().any(|r| r.archived) {
            self.archive_open = false;
        }
        self.rows = self.visible();
        self.cursor = under_cursor
            .and_then(|k| self.index_of(&k))
            .unwrap_or(self.cursor)
            .min(self.rows.len().saturating_sub(1));
        // A dialog about a row that has gone is a question about nothing.
        let gone = match &self.ask {
            Some((Ask::Close { key } | Ask::ResumeFailed { key }, _)) => {
                self.index_of(key).is_none()
            }
            Some((Ask::NoRoom { victim, then }, _)) => [victim, then]
                .into_iter()
                .flatten()
                .any(|k| self.index_of(k).is_none()),
            None => false,
        };
        if gone {
            self.ask = None;
        }
        self.keep_cursor_visible();
    }

    /// The rows as drawn: every row not archived, then — when anything is archived — the
    /// Archived heading's row, counting them, then the archived rows if the group is open.
    fn visible(&self) -> Vec<Row> {
        let (archived, mut out): (Vec<Row>, Vec<Row>) =
            self.all.iter().cloned().partition(|r| r.archived);
        if archived.is_empty() {
            return out;
        }
        out.push(Row {
            key: RowKey::ArchiveFold,
            wants_you: false,
            busy: false,
            unread: false,
            attached: false,
            offloaded: false,
            // Not open, so the header does not count it.
            closed: true,
            archived: false,
            title: Group::Archived.name().to_string(),
            age: archived.len().to_string(),
        });
        if self.archive_open {
            out.extend(archived);
        }
        out
    }

    /// `Enter` on the Archived heading: open the group or shut it, the cursor staying put.
    fn toggle_archive(&mut self) -> Action {
        self.archive_open = !self.archive_open;
        self.rows = self.visible();
        self.cursor = self
            .index_of(&RowKey::ArchiveFold)
            .unwrap_or(self.cursor)
            .min(self.rows.len().saturating_sub(1));
        self.keep_cursor_visible();
        Action::Redraw
    }

    fn index_of(&self, key: &RowKey) -> Option<usize> {
        self.rows.iter().position(|r| r.key == *key)
    }

    /// How many lines the hint takes at this width, counted by the renderer that draws it.
    fn hint_lines(&self) -> usize {
        crate::render::hint_line_count(self.width, self.rows.is_empty())
    }

    /// Lines of the list that fit between the header's rule and the closing rule.
    fn capacity(&self) -> usize {
        // Work in progress takes the status line's place, as the renderer draws it.
        let busy = self
            .busy
            .as_ref()
            .filter(|b| !b.loading)
            .map(|b| format!("{} {}", b.glyph(), b.what));
        let status = busy
            .as_deref()
            .or(self.status.as_deref())
            .map_or(0, |s| render::status_line_count(self.width, s));
        let fixed = 3 + status + self.hint_lines();
        usize::from(self.height).saturating_sub(fixed).max(1)
    }

    fn keep_cursor_visible(&mut self) {
        let items = render::layout(&self.rows);
        self.scroll = render::scroll_to(&items, self.cursor, self.scroll, self.capacity());
    }

    pub fn view(&self) -> View {
        let (dialog, about) = match &self.ask {
            Some((ask, dialog)) => {
                let key = match ask {
                    Ask::Close { key } | Ask::ResumeFailed { key } => Some(key),
                    Ask::NoRoom { victim, then } => victim.as_ref().or(then.as_ref()),
                };
                let about = key.and_then(|k| self.index_of(k));
                (Some(dialog.clone()), about)
            }
            None => (None, None),
        };
        View {
            width: self.width,
            height: self.height,
            header: self.header.clone(),
            workspace: self.workspace.clone(),
            rows: self.rows.clone(),
            cursor: self.cursor,
            scroll: self.scroll,
            about,
            screen: self.screen,
            dialog,
            status: self.status.clone(),
            busy: self.busy.clone(),
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.keep_cursor_visible();
    }

    /// Ask the owner something a launch came back with (mockups 4 and 5). Indices are into
    /// the rows as they were when the launch was asked for, which they still are.
    pub fn ask_no_room(&mut self, dialog: Dialog, then: Option<usize>) {
        let victim = match &dialog {
            Dialog::NoRoom {
                offer: Some((index, _, _)),
                ..
            } => self.rows.get(*index).map(|r| r.key.clone()),
            _ => None,
        };
        let then = then.and_then(|i| self.rows.get(i)).map(|r| r.key.clone());
        self.ask = Some((Ask::NoRoom { then, victim }, dialog));
    }

    pub fn ask_resume_failed(&mut self, dialog: Dialog, index: usize) {
        if let Some(r) = self.rows.get(index) {
            let key = r.key.clone();
            self.ask = Some((Ask::ResumeFailed { key }, dialog));
        }
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
            title: row.title.clone(),
            running: !row.offloaded,
        };
        let key = row.key.clone();
        self.ask = Some((Ask::Close { key }, dialog));
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
            Key::Enter if self.on(|r| r.key == RowKey::ArchiveFold) => self.toggle_archive(),
            Key::Enter if !self.rows.is_empty() => Action::Open(self.cursor),
            Key::Char('n') => Action::New,
            Key::Char('s') => Action::Shell,
            Key::Char('c') if self.on(|r| r.key == RowKey::ArchiveFold) => Action::None,
            Key::Char('c') if self.on(|r| r.archived) => Action::Unarchive(self.cursor),
            Key::Char('c')
                if self.on(|r| r.closed && matches!(r.key, RowKey::Conversation { .. })) =>
            {
                Action::Archive(self.cursor)
            }
            Key::Char('c') if self.on(|r| r.closed) => {
                self.status = Some("that session is already closed".to_string());
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

    /// Whether the row under the cursor is one `pred` picks.
    fn on(&self, pred: impl Fn(&Row) -> bool) -> bool {
        self.rows.get(self.cursor).is_some_and(pred)
    }

    /// Which session a click on screen line `line` landed on: the list starts on the third
    /// line, after the header and its rule. A heading, a blank line or the fold is none.
    fn row_at(&self, line: u16) -> Option<usize> {
        let offset = usize::from(line).checked_sub(2)?;
        let items = render::layout(&self.rows);
        let (shown, _) = render::window(&items, self.scroll, self.capacity());
        items[shown].get(offset).and_then(|i| i.selects())
    }

    fn answer(&mut self, ask: Ask, key: Key) -> Action {
        let dismiss = |m: &mut Menu| {
            m.ask = None;
            Action::Redraw
        };
        // `replace_rows` drops a dialog whose row has gone, so these find their rows.
        let at = |m: &Menu, k: &RowKey| m.index_of(k).unwrap_or(m.cursor);
        match (ask, key) {
            (_, Key::Resize) => Action::Redraw,
            (Ask::Close { key }, Key::Char('y')) => {
                self.ask = None;
                Action::Close(at(self, &key))
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
                Action::OffloadThenOpen {
                    victim: at(self, &victim),
                    then: then.map(|k| at(self, &k)),
                }
            }
            (Ask::NoRoom { .. }, Key::Char('n') | Key::Esc) => dismiss(self),
            (Ask::ResumeFailed { key }, Key::Char('r')) => {
                self.ask = None;
                Action::Open(at(self, &key))
            }
            (Ask::ResumeFailed { key }, Key::Char('c')) => self.ask_close(at(self, &key)),
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

/// Read the rows: every slot in the registry that is not closed, every zmx session the
/// registry has never heard of, and every conversation started in `workspace` that is not
/// running. In recency order; `Menu::replace_rows` does the grouping. `sessions` is zmx's
/// listing, which the caller refreshes when it can have changed (`zmx::Watch`).
pub fn gather(now: Millis, workspace: &str, sessions: &[zmx::Session]) -> Vec<Row> {
    let clock = age_clock(now);
    let recs = registry::all().unwrap_or_default();
    let mut dated: Vec<(Millis, Row)> = recs
        .iter()
        // Closed slots are not listed as slots: their conversations are in Claude Code's own
        // store, with every other conversation, and are listed from there (below).
        .filter(|r| r.state != State::Closed)
        .map(|r| (r, slot_row(r, clock, sessions)))
        // An offloaded slot with no conversation on disk is nothing to open: offloaded
        // before its first prompt, or `/clear`ed and left, `Enter` on it would fail at once
        // (issue #5). Not listed.
        .filter(|(r, row)| !row.offloaded || r.has_conversation())
        .map(|(r, row)| (r.last_activity_ms, row))
        .collect();
    // The Closed group: conversations on disk (owner, 2026-10-03), whoever started them, so
    // a conversation from before a `/clear`, or from a `claude` run by hand, is one `Enter`
    // away. Not one that is running — Claude Code's live sessions say which — and not the
    // current conversation of a slot listed above, which is that slot's row.
    let mut taken: std::collections::HashSet<String> = live::all()
        .into_iter()
        .filter_map(|s| s.session_id)
        .collect();
    taken.extend(
        recs.iter()
            .filter(|r| r.state != State::Closed)
            .filter_map(|r| r.session_id.clone()),
    );
    let marks = archive::marks();
    for c in store::in_workspace(&live::config_dir(), workspace) {
        if taken.contains(&c.id) {
            continue;
        }
        // Against the floored clock, like the ages: a row crosses into the archive on the
        // minute's redraw, never between them.
        let archived = archive::is_archived(marks.get(&c.id).copied(), c.last_ms, clock);
        let row = Row {
            key: RowKey::Conversation {
                id: c.id,
                cwd: c.cwd,
            },
            wants_you: false,
            busy: false,
            unread: false,
            attached: false,
            offloaded: false,
            closed: true,
            archived,
            title: c.title,
            age: fmt::age(clock.saturating_sub(c.last_ms)),
        };
        dated.push((c.last_ms, row));
    }
    dated.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let mut rows: Vec<Row> = dated.into_iter().map(|(_, row)| row).collect();
    for sess in sessions {
        if recs.iter().any(|r| r.slot == sess.name) {
            continue;
        }
        // When zmx made it is the only clock a session with no record has.
        let since = sess.created.map(|s| s * 1000).unwrap_or(clock);
        rows.push(Row {
            key: RowKey::Socket(sess.name.clone()),
            wants_you: false,
            // Nothing says what a session with no hooks is doing; it is listed as Idle.
            busy: false,
            unread: false,
            attached: sess.attached,
            offloaded: false,
            closed: false,
            archived: false,
            title: sess.name.clone(),
            age: fmt::age(clock.saturating_sub(since)),
        });
    }
    rows
}

fn slot_row(r: &SlotRecord, clock: Millis, sessions: &[zmx::Session]) -> Row {
    let alive = matches!((r.pid, r.proc_start), (Some(p), Some(s)) if procinfo::is_alive(p, s));
    let attached = alive && sessions.iter().any(|s| s.name == r.slot && s.attached);
    // An Esc fires no hook, so a turn the owner interrupted — or a permission prompt they
    // dismissed — would read Working or Needs you until the next turn ended. The transcript's
    // trailing marker says it is over (`transcript::interrupted_at`).
    let interrupted = (r.busy || r.needs_you)
        && r.esc_ended(
            r.conversation_path()
                .and_then(|p| crate::transcript::interrupted_at_cached(&p)),
        )
        .is_some();
    Row {
        key: RowKey::Slot(r.slot.clone()),
        wants_you: r.needs_you && !interrupted,
        busy: r.busy && !interrupted,
        unread: r.unread(),
        attached,
        // A record whose process has gone is resumable whatever its state says — reconcile
        // has not caught up yet. One with no pid at all is a slot just started, waiting for
        // its SessionStart, and is not.
        offloaded: matches!(r.state, State::Offloaded | State::Offloading)
            || (r.state == State::Live && r.pid.is_some() && !alive),
        closed: r.state == State::Closed,
        archived: false,
        // What Claude Code's own session selector shows (`transcript.rs`), not the live
        // sessions file's `name`, which showed Claude's replies on the boxes (0.3.1).
        title: r.display_title(),
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

    #[test]
    fn a_turn_interrupted_with_esc_is_not_drawn_as_working_or_waiting() {
        let dir = std::env::temp_dir().join(format!("cs-menu-esc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("conv.jsonl");
        let prompted = r#"{"type":"user","message":{"role":"user","content":"go"},"timestamp":"2026-10-06T09:23:28.476Z"}"#;
        std::fs::write(&transcript, format!("{prompted}\n")).unwrap();
        let mut r = SlotRecord::new("claude-1", 0);
        r.transcript_path = Some(transcript.display().to_string());
        r.busy = true;
        r.needs_you = true;
        r.last_activity_ms = crate::store::iso_ms("2026-10-06T09:23:28.500Z").unwrap();
        let row = slot_row(&r, r.last_activity_ms, &[]);
        assert!(
            row.busy && row.wants_you,
            "calibration: as the hooks left it"
        );
        let mark = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"timestamp":"2026-10-06T09:23:35.516Z"}"#;
        std::fs::write(&transcript, format!("{prompted}\n{mark}\n")).unwrap();
        let row = slot_row(&r, r.last_activity_ms, &[]);
        assert!(!row.busy && !row.wants_you, "at its prompt, so Idle");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn row(name: &str) -> Row {
        Row {
            key: RowKey::Slot(name.into()),
            wants_you: false,
            unread: false,
            busy: false,
            attached: false,
            offloaded: false,
            closed: false,
            archived: false,
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
    fn the_first_reading_groups_and_puts_unread_first_in_idle() {
        let mut a = row("a");
        let mut b = row("b");
        b.unread = true;
        let c = row("c");
        let mut d = row("d");
        d.wants_you = true;
        let mut e = row("e");
        e.busy = true;
        e.unread = true;
        let mut f = row("f");
        f.offloaded = true;
        // Gathered most-recent first.
        let m = menu(vec![
            f.clone(),
            a.clone(),
            e.clone(),
            b.clone(),
            c,
            d.clone(),
        ]);
        assert_eq!(names(&m), ["d", "e", "b", "a", "c", "f"]);
        let groups: Vec<Group> = m.rows().iter().map(Row::group).collect();
        assert_eq!(
            groups,
            [
                Group::NeedsYou,
                Group::Working,
                Group::Idle,
                Group::Idle,
                Group::Idle,
                Group::Offloaded
            ]
        );
        // Calibration: with nothing flagged the recency order is untouched.
        a.unread = false;
        b.unread = false;
        d.wants_you = false;
        assert_eq!(names(&menu(vec![a, b, row("c"), d])), ["a", "b", "c", "d"]);
    }

    fn stored(name: &str, archived: bool) -> Row {
        Row {
            key: RowKey::Conversation {
                id: name.into(),
                cwd: "/workspace".into(),
            },
            closed: true,
            archived,
            ..row(name)
        }
    }

    #[test]
    fn the_archive_starts_shut_behind_one_heading_and_enter_opens_and_shuts_it() {
        let mut m = menu(vec![
            row("live"),
            stored("closed", false),
            stored("old-1", true),
            stored("old-2", true),
        ]);
        assert_eq!(names(&m), ["live", "closed", "Archived"], "shut at start");
        assert_eq!(m.rows()[2].key, RowKey::ArchiveFold);
        assert_eq!(m.rows()[2].age, "2", "it counts what it holds");
        // The cursor reaches it like a row, and Enter opens it rather than opening anything.
        m.key(Key::End);
        assert_eq!(m.rows()[m.view().cursor].key, RowKey::ArchiveFold);
        assert_eq!(m.key(Key::Enter), Action::Redraw);
        assert_eq!(names(&m), ["live", "closed", "Archived", "old-1", "old-2"]);
        assert_eq!(
            m.rows()[m.view().cursor].key,
            RowKey::ArchiveFold,
            "the cursor stays on the heading"
        );
        // An archived row opens like any closed one.
        m.key(Key::Down);
        assert_eq!(m.key(Key::Enter), Action::Open(3));
        // And Enter on the heading again shuts it.
        m.key(Key::Up);
        m.key(Key::Enter);
        assert_eq!(names(&m), ["live", "closed", "Archived"]);
        // Calibration: with nothing archived there is no heading at all.
        let m = menu(vec![row("live"), stored("closed", false)]);
        assert_eq!(names(&m), ["live", "closed"]);
    }

    #[test]
    fn c_archives_a_closed_conversation_without_asking_and_unarchives_an_archived_one() {
        let mut m = menu(vec![
            row("live"),
            stored("closed", false),
            stored("old", true),
        ]);
        m.key(Key::Down);
        assert_eq!(
            m.key(Key::Char('c')),
            Action::Archive(1),
            "no question asked"
        );
        assert_eq!(m.view().dialog, None);
        // On the heading, c does nothing.
        m.key(Key::Down);
        assert_eq!(m.rows()[m.view().cursor].key, RowKey::ArchiveFold);
        assert_eq!(m.key(Key::Char('c')), Action::None);
        m.key(Key::Enter);
        m.key(Key::Down);
        assert_eq!(m.key(Key::Char('c')), Action::Unarchive(3));
        // Calibration: on a live row, c still asks before closing.
        m.key(Key::Home);
        assert_eq!(m.key(Key::Char('c')), Action::Redraw);
        assert!(matches!(m.view().dialog, Some(Dialog::Close { .. })));
    }

    #[test]
    fn a_click_on_the_archived_heading_selects_it() {
        let mut m = menu(vec![row("a"), stored("old", true)]);
        // Header, rule, then: Idle heading, a, blank, the Archived heading on line 5.
        assert_eq!(m.key(Key::Click { row: 5, col: 4 }), Action::Redraw);
        assert_eq!(m.rows()[m.view().cursor].key, RowKey::ArchiveFold);
        // Calibration: the Idle heading on line 2 is not selectable.
        assert_eq!(m.key(Key::Click { row: 2, col: 4 }), Action::None);
    }

    #[test]
    fn a_row_archived_in_a_reading_leaves_closed_and_joins_the_archive() {
        let mut m = menu(vec![row("live"), stored("closed", false)]);
        assert_eq!(names(&m), ["live", "closed"]);
        m.replace_rows(vec![row("live"), stored("closed", true)], false);
        assert_eq!(names(&m), ["live", "Archived"]);
        m.key(Key::End);
        m.key(Key::Enter);
        assert_eq!(names(&m), ["live", "Archived", "closed"]);
    }

    #[test]
    fn an_archive_emptied_while_open_comes_back_shut() {
        let mut m = menu(vec![row("live"), stored("old", true)]);
        m.key(Key::End);
        m.key(Key::Enter);
        assert_eq!(names(&m), ["live", "Archived", "old"], "opened");
        m.replace_rows(vec![row("live"), stored("old", false)], false);
        assert_eq!(names(&m), ["live", "old"], "unarchived: no heading");
        m.replace_rows(vec![row("live"), stored("old", true)], false);
        assert_eq!(names(&m), ["live", "Archived"], "archived again: shut");
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
        assert_eq!(
            m.view().status.as_deref(),
            Some("that session is already closed")
        );
        assert_eq!(m.key(Key::Enter), Action::Open(2), "Enter resumes it");
    }

    #[test]
    fn later_readings_keep_places_and_add_new_rows_at_the_top_of_their_group() {
        let mut two = row("two");
        two.unread = true;
        let mut m = menu(vec![row("one"), two.clone(), row("three")]);
        assert_eq!(names(&m), ["two", "one", "three"]);
        // Mockup 6: back from "two", it is no longer unread and is the most recent — and it
        // stays where it was.
        two.unread = false;
        m.replace_rows(vec![two, row("one"), row("four")], false);
        assert_eq!(
            names(&m),
            ["four", "two", "one"],
            "three went, four joined at the top of Idle, nothing else moved"
        );
    }

    #[test]
    fn a_row_that_changes_group_joins_the_top_of_its_new_group_and_the_cursor_follows() {
        let mut busy = row("busy");
        busy.busy = true;
        let mut m = menu(vec![busy.clone(), row("a"), row("b"), row("c")]);
        assert_eq!(names(&m), ["busy", "a", "b", "c"]);
        m.key(Key::End);
        assert_eq!(m.rows()[m.view().cursor].title, "c");
        // "c" starts a turn and "busy" finishes one.
        let mut c = row("c");
        c.busy = true;
        m.replace_rows(vec![row("busy"), row("a"), row("b"), c], false);
        assert_eq!(names(&m), ["c", "busy", "a", "b"]);
        assert_eq!(
            m.rows()[m.view().cursor].title,
            "c",
            "the cursor went with it"
        );
        // Calibration: the same reading with nothing changing group moves nothing.
        let before = names(&m);
        let same = m.rows().to_vec();
        m.replace_rows(same, false);
        assert_eq!(names(&m), before);
    }

    #[test]
    fn a_dialog_follows_its_row_through_a_regroup() {
        let mut m = menu(vec![row("a"), row("b")]);
        m.key(Key::Down);
        m.key(Key::Char('c'));
        // "a" starts a turn and moves above "b" into Working: the question is still about b.
        let mut a = row("a");
        a.busy = true;
        m.replace_rows(vec![a, row("b")], false);
        assert_eq!(names(&m), ["a", "b"]);
        assert_eq!(m.view().about, Some(1));
        assert_eq!(m.key(Key::Char('y')), Action::Close(1));
        // And a dialog about a row that has gone goes with it.
        m.key(Key::Char('c'));
        m.replace_rows(vec![row("a")], false);
        assert_eq!(m.view().dialog, None);
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
    fn a_click_on_a_row_moves_the_cursor_there_and_anywhere_else_does_nothing() {
        let mut w = row("w");
        w.wants_you = true;
        let mut m = menu(vec![w, row("a"), row("b")]);
        // Header, rule, then: Needs you, w, blank, Idle, a, b.
        assert_eq!(m.key(Key::Click { row: 7, col: 10 }), Action::Redraw);
        assert_eq!(m.rows()[m.view().cursor].title, "b");
        for (line, what) in [
            (0, "the header"),
            (2, "a heading"),
            (4, "a blank"),
            (5, "a heading"),
            (9, "below the list"),
        ] {
            assert_eq!(
                m.key(Key::Click { row: line, col: 3 }),
                Action::None,
                "{what}"
            );
        }
        assert_eq!(m.rows()[m.view().cursor].title, "b");
        // Calibration: a click on the first session does move it.
        assert_eq!(m.key(Key::Click { row: 3, col: 0 }), Action::Redraw);
        assert_eq!(m.view().cursor, 0);
    }

    #[test]
    fn the_cursor_only_ever_lands_on_a_session() {
        // Every group, a short terminal so the list scrolls and folds, and every key that
        // moves the cursor, many times over: the cursor's line is always a session's.
        let mut rows = Vec::new();
        for (i, flag) in ["!", "b", "", "*", "z", "x", "x", "x", "x", "x"]
            .iter()
            .enumerate()
        {
            let mut r = row(&format!("r{i}"));
            r.wants_you = *flag == "!";
            r.busy = *flag == "b";
            r.unread = *flag == "*";
            r.offloaded = *flag == "z";
            r.closed = *flag == "x";
            rows.push(r);
        }
        let mut m = Menu::new(40, 12, header(), "/w".into(), rows);
        let keys = [
            Key::Down,
            Key::Down,
            Key::PageDown,
            Key::End,
            Key::Up,
            Key::PageUp,
            Key::Home,
            Key::WheelDown,
            Key::Down,
        ];
        for _ in 0..5 {
            for k in keys {
                m.key(k);
                let v = m.view();
                let items = render::layout(&v.rows);
                let (shown, _) = render::window(&items, v.scroll, m.capacity());
                let on = items[shown].iter().any(|i| i.selects() == Some(v.cursor));
                assert!(
                    on,
                    "after {k:?} the cursor {} is not a session on screen",
                    v.cursor
                );
            }
        }
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
        assert_eq!(
            m.view().dialog,
            Some(Dialog::Close {
                title: "b".into(),
                running: true,
            })
        );
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
            offer: Some((1, "2d".into(), "b".into())),
        };
        m.ask_no_room(dialog.clone(), Some(2));
        assert_eq!(
            m.view().about,
            Some(1),
            "the dialog shows the session it offers"
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
            title: "b".into(),
            session: "0f9c4a1e".into(),
            killed_for_memory: false,
            output: vec![],
            closed: false,
        };
        m.ask_resume_failed(failed.clone(), 1);
        assert_eq!(m.key(Key::Char('r')), Action::Open(1));
        m.ask_resume_failed(failed.clone(), 1);
        assert_eq!(m.key(Key::Char('c')), Action::Redraw);
        assert!(matches!(m.view().dialog, Some(Dialog::Close { ref title, .. }) if title == "b"));
        m.key(Key::Esc);
        m.ask_resume_failed(failed, 1);
        assert_eq!(m.key(Key::Esc), Action::Redraw);
        assert_eq!(m.view().dialog, None);
    }

    #[test]
    fn a_status_line_goes_with_the_next_key() {
        let mut m = menu(vec![row("a")]);
        m.set_status(Some("detached · it is still running".into()));
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
        // Line 0 of the list is the Idle heading, so session 7 is on line 8.
        let items = render::layout(&v.rows);
        let (shown, _) = render::window(&items, v.scroll, 5);
        assert!(shown.contains(&8), "cursor 7 not within {shown:?}");
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
