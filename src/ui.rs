//! The menu's contract: what the renderer draws from, what the terminal reads, and what the
//! launcher is asked to do. Four modules meet here and nowhere else:
//!
//! - `render.rs` turns a [`View`] into a [`Frame`] — pure, and tested against the approved
//!   screens in `docs/mockups.md` at 40×24 and 80×24.
//! - `term.rs` owns the terminal: raw mode, size, and bytes in as [`Key`]s.
//! - `launch.rs` does what a keypress asks — attach, resume, start, a shell, close — with the
//!   terminal handed over through [`Terminal`] while a child has it.
//! - `menu.rs` holds the state, builds the [`View`] from the registry and zmx, and maps a
//!   [`Key`] to what happens next.
//!
//! Nothing in this file does anything. It is the shape the others agree on, so each can be
//! built and tested without the rest.

/// Which thing a row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// A slot with a registry record, ours or not (`registered` on the record says which).
    Slot(String),
    /// A zmx session with no record at all — a session started before the hooks were
    /// installed, listed under Idle. All there is to show is its name.
    Socket(String),
    /// A conversation in Claude Code's own store that is not running (`store.rs`), listed
    /// under Closed: `Enter` resumes it in a slot, from the directory it started in.
    Conversation { id: String, cwd: String },
    /// The Archived group's heading (owner, 2026-10-03): the one line the cursor can rest on
    /// that is not a session. `Enter` on it folds the archive open or shut.
    ArchiveFold,
    /// A container, on the list of every container (the hub's home): `Enter` on it shows that
    /// container's sessions alone. Which container is the row's `host`.
    Container,
}

/// The groups the list is drawn in, top to bottom. Every row is in exactly one, decided by
/// [`Row::group`]; an empty group is not drawn. Three in a container, since 2026-10-26 (the
/// owner: "keep loaded / offloaded and a click to expand archived section"); before that six,
/// Needs you, Working, Idle, Offloaded, Closed and Archived (owner, 2026-10-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// A claude is running: mid-turn, at its prompt, or waiting for the owner — which is the
    /// amber `!` on its row, not a group of its own (the owner, 2026-10-26).
    Loaded,
    /// Stopped to save memory; `Enter` resumes it.
    Offloaded,
    /// Ended, and still resumable, under one heading folded shut until the owner opens it —
    /// `Enter` or a click on the heading. Not counted as open.
    Archived,
    /// Every container the hub reaches, on its home screen (2026-10-26): `Enter` on one shows
    /// its sessions.
    Containers,
}

impl Group {
    pub const ALL: [Group; 4] = [
        Group::Loaded,
        Group::Offloaded,
        Group::Archived,
        Group::Containers,
    ];

    /// The heading, as drawn.
    pub fn name(self) -> &'static str {
        match self {
            Group::Loaded => "Loaded",
            Group::Offloaded => "Offloaded",
            Group::Archived => "Archived",
            Group::Containers => "Containers",
        }
    }
}

/// One row of the list, already decided: the renderer formats it and judges nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The container the row is in, as the hub names it — its ssh alias, `local` for this
    /// machine. Two containers can each have a `claude-1`, so a row is the pair.
    pub host: String,
    pub key: RowKey,
    /// A prompt is waiting: the row carries the amber `!`, and leads its group.
    pub wants_you: bool,
    /// Mid-turn.
    pub busy: bool,
    /// It finished something since the owner last looked: the title is bold.
    pub unread: bool,
    /// Attached somewhere else right now: the row is drawn dim.
    pub attached: bool,
    /// Offloaded; `Enter` resumes it.
    pub offloaded: bool,
    /// Closed; `Enter` resumes it.
    pub closed: bool,
    /// Archived (`archive.rs`): put away with `c` before 2026-10-26, or 30 days unused. Under
    /// Archived like every closed row.
    pub archived: bool,
    pub title: String,
    /// Already formatted: `now`, `14m`, `5h`, `2d`. On the Archived heading's row, how many
    /// conversations are archived; on a container's row, how many sessions it has open.
    pub age: String,
    /// When it was last active, in milliseconds: how rows from several containers are put in
    /// one recency order. Within one container the order is the container's own.
    pub at: u64,
}

impl Row {
    /// Whether two rows stand for the same thing: the same key in the same container.
    pub fn same(&self, other: &Row) -> bool {
        self.host == other.host && self.key == other.key
    }

    /// Which group the row is drawn in. Being stopped outranks anything the record last said
    /// about the process.
    pub fn group(&self) -> Group {
        if self.key == RowKey::Container {
            Group::Containers
        } else if self.archived || self.closed || self.key == RowKey::ArchiveFold {
            Group::Archived
        } else if self.offloaded {
            Group::Offloaded
        } else {
            Group::Loaded
        }
    }
}

/// The header line: `infra-dev · 6 open · 812M of 1.0G`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The container's name — its hostname.
    pub host: String,
    /// Bytes used and the ceiling, from the cgroup. `None` when there is no ceiling to show.
    pub memory: Option<(u64, u64)>,
}

/// A question in a frame over the list. It names the session by its title: nothing on the
/// screen is numbered (owner, 2026-10-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Mockup 3. `running` picks the explanation: a live slot's process is stopped, an
    /// offloaded one is only hidden; either way the conversation stays on disk.
    Close { title: String, running: bool },
    /// Mockup 4. `offer` is the slot that could make room — its key, in the same container as
    /// the session being opened, its idle age and its title — or `None` when nothing is
    /// offloadable, and then the dialog can only say so.
    NoRoom {
        used: u64,
        limit: u64,
        want: u64,
        offer: Option<(RowKey, String, String)>,
    },
    /// Mockup 5. `session` is the conversation id as shown (its first 8 characters),
    /// `killed_for_memory` whether the container's OOM-kill count rose while it started,
    /// `output` the last lines claude wrote to stderr, and `closed` whether the row went back
    /// to Closed rather than Offloaded. There is no exit status: zmx does not report it.
    ResumeFailed {
        title: String,
        session: String,
        killed_for_memory: bool,
        output: Vec<String>,
        closed: bool,
    },
}

/// Which full screen is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    /// Mockup 7, on `?`.
    Keys,
}

/// Everything one frame shows. Built by `menu.rs`, drawn by `render.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub width: u16,
    pub height: u16,
    pub header: Header,
    /// The hub's home: every container, one row each, the ones with a session waiting under
    /// Needs you (mockup 12).
    pub home: bool,
    /// One container's sessions, reached from the home: `Esc` goes back to it (mockup 13).
    pub back: bool,
    /// One container that is not answering, and why (mockup 14). Its list is empty.
    pub down: Option<String>,
    /// Where `n` starts a session and `s` a shell — `/workspace` in the mockups.
    pub workspace: String,
    pub rows: Vec<Row>,
    /// Index into `rows` of the highlighted row. Ignored when `rows` is empty.
    pub cursor: usize,
    /// The first line of the list drawn — a line of the grouped layout, headings and blanks
    /// included (`render::layout`) — so the cursor can stay on screen in a short terminal.
    pub scroll: usize,
    /// Index into `rows` of the session a dialog is about, drawn above the box under its
    /// group's heading.
    pub about: Option<usize>,
    pub screen: Screen,
    pub dialog: Option<Dialog>,
    /// One line between the list's closing rule and the hint line, as in mockup 6:
    /// `detached · it is still running`.
    pub status: Option<String>,
    /// Something is being worked on and has taken long enough to show (mockups 10 and 11).
    pub busy: Option<Busy>,
}

/// Docker Compose's spinner (owner, 2026-10-04): ten braille frames, one every 100 ms. Braille
/// is in none of the monospace fonts measured on 2026-10-01; the owner has seen it drawn on
/// the terminals this is driven from, which put a fallback glyph in one cell, and it is only
/// ever drawn alone in a cell, so a fallback cannot drag a line out of true.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What is being worked on, as drawn: the spinner's frame, the row it is on, if it is about
/// one, and what is happening, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Busy {
    pub frame: usize,
    /// The row whose age the spinner stands in for. `None` for work that has no row yet — a
    /// new session — or is about the whole list.
    pub row: Option<usize>,
    pub what: String,
    /// The list itself is still being read for the first time: drawn blank, with the spinner
    /// where it will go (mockup 10).
    pub loading: bool,
}

impl Busy {
    pub fn glyph(&self) -> &'static str {
        SPINNER[self.frame % SPINNER.len()]
    }
}

/// A colour from the approved table in `docs/mockups.md`. Two, and only two: amber is spent
/// on the Needs-you heading and nothing else; blue is the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Amber,
    Blue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Colour>,
    pub bold: bool,
    pub dim: bool,
    /// The cursor's highlighted row (owner, 2026-10-02).
    pub reverse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// A rendered screen: exactly `height` lines, none wider than `width` columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub lines: Vec<Vec<Span>>,
}

/// Why nothing could be drawn: narrower than the narrowest way out (mockup 8). The menu exits
/// non-zero with this, and the door falls through to a login shell saying why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooNarrow {
    pub width: u16,
    pub need: u16,
}

/// What the terminal read. Mouse input is clicks only — see `docs/design.md` § *The menu*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Char(char),
    /// A left click, 0-based, on the screen as drawn.
    Click {
        row: u16,
        col: u16,
    },
    WheelUp,
    WheelDown,
    /// The window changed size.
    Resize,
}

/// Handing the terminal to a child process and taking it back. `launch.rs` suspends before
/// it runs `zmx`, `claude` or a shell, and resumes after, so the child gets an ordinary
/// cooked terminal and the menu gets its raw one back.
pub trait Terminal {
    fn suspend(&mut self) -> std::io::Result<()>;
    fn resume(&mut self) -> std::io::Result<()>;

    /// Run a child with the terminal handed over, and take the terminal back whatever
    /// happened. A container's agent overrides it: there the terminal is the hub's, and the
    /// child is run there, through `ssh -t` back into the container (`agent.rs`).
    fn run(
        &mut self,
        cmd: &mut std::process::Command,
    ) -> std::io::Result<std::process::ExitStatus> {
        self.suspend()?;
        let status = cmd.status();
        // A failure to take the terminal back is not reported: the menu repaints the whole
        // screen on its next frame, which is the only repair on offer.
        let _ = self.resume();
        status
    }
}

/// What came of asking `launch.rs` to do something, for the menu to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Back at the menu, with a line for the status slot: `detached · it is still running`,
    /// or `the session ended`, or nothing.
    Back(Option<String>),
    /// There is not room for another claude; ask (mockup 4).
    NoRoom(Dialog),
    /// The resume did not take; say so (mockup 5).
    ResumeFailed(Dialog),
    /// Nothing was done, and why — a lock that stayed busy, a conversation already running
    /// somewhere it could not be attached to.
    Refused(String),
}
