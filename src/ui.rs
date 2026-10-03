//! The menu's contract: what the renderer draws from, what the terminal reads, and what the
//! launcher is asked to do. Four modules meet here and nowhere else:
//!
//! - `render.rs` turns a [`View`] into a [`Frame`] — pure, and tested against the approved
//!   screens in `docs/mockups.md` at 40×24 and 80×24.
//! - `term.rs` owns the terminal: raw mode, size, and bytes in as [`Key`]s.
//! - `launch.rs` does what a keypress asks — attach, resume, start, a shell, close — with the
//!   terminal handed over through [`Terminal`] while a child has it.
//! - `menu.rs` holds the state, builds the [`View`] from the registry and abduco, and maps a
//!   [`Key`] to what happens next.
//!
//! Nothing in this file does anything. It is the shape the others agree on, so each can be
//! built and tested without the rest.

/// Which thing a row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// A slot with a registry record, ours or not (`registered` on the record says which).
    Slot(String),
    /// An abduco session with no record at all — the `u` row of a session started before the
    /// hooks were installed. All there is to show is its name.
    Socket(String),
}

/// One row of the list, already decided: the renderer formats it and judges nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: RowKey,
    /// `!` — a prompt is waiting. The only amber on the screen.
    pub wants_you: bool,
    /// `*` — it finished something since the owner last looked.
    pub unread: bool,
    /// `t` — a timer is pending.
    pub timer: bool,
    /// `@` — attached somewhere else right now.
    pub attached: bool,
    /// `z` — offloaded; `Enter` resumes it.
    pub offloaded: bool,
    /// `u` — not started by claude-sessions.
    pub unregistered: bool,
    /// `x` — closed; listed at the bottom so its conversation can be reached again, and
    /// `Enter` resumes it (owner, 2026-10-03). Not counted as open.
    pub closed: bool,
    pub title: String,
    /// Already formatted: `now`, `14m`, `5h`, `2d`.
    pub age: String,
}

/// The header line: `infra-dev · 6 open · 812M of 1.0G`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The container's name — its hostname.
    pub host: String,
    /// Bytes used and the ceiling, from the cgroup. `None` when there is no ceiling to show.
    pub memory: Option<(u64, u64)>,
}

/// A question in a frame over the list. Row numbers are the 1-based numbers the rows are
/// drawn with — what the owner sees — never slot names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Mockup 3. `running` picks the explanation: a live slot's process is stopped, an
    /// offloaded one is only hidden; either way the conversation stays on disk.
    Close {
        row: usize,
        title: String,
        running: bool,
    },
    /// Mockup 4. `offer` is the slot that could make room — its row, idle age and title —
    /// or `None` when nothing is offloadable, and then the dialog can only say so.
    NoRoom {
        used: u64,
        limit: u64,
        want: u64,
        offer: Option<(usize, String, String)>,
    },
    /// Mockup 5. `session` is the conversation id as shown (its first 8 characters), `status`
    /// the exit status, `output` the last lines claude wrote to stderr.
    ResumeFailed {
        row: usize,
        session: String,
        status: i32,
        output: Vec<String>,
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
    /// Where `n` starts a session and `s` a shell — `/workspace` in the mockups.
    pub workspace: String,
    pub rows: Vec<Row>,
    /// Index into `rows` of the highlighted row. Ignored when `rows` is empty.
    pub cursor: usize,
    /// Index into `rows` of the first row drawn, so the cursor can stay on screen in a short
    /// terminal and a dialog can show the rows around the one it is about.
    pub scroll: usize,
    pub screen: Screen,
    pub dialog: Option<Dialog>,
    /// One line between the list's closing rule and the hint line, as in mockup 6:
    /// `detached from 2 · it is still running`.
    pub status: Option<String>,
}

/// A colour from the approved table in `docs/mockups.md`. Two, and only two: amber is spent
/// on `!` and nothing else; blue is the keys and `t`.
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
/// it runs `abduco`, `claude` or a shell, and resumes after, so the child gets an ordinary
/// cooked terminal and the menu gets its raw one back.
pub trait Terminal {
    fn suspend(&mut self) -> std::io::Result<()>;
    fn resume(&mut self) -> std::io::Result<()>;
}

/// What came of asking `launch.rs` to do something, for the menu to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Back at the menu, with a line for the status slot: `detached from 2 · it is still
    /// running`, or `2 ended`, or nothing.
    Back(Option<String>),
    /// There is not room for another claude; ask (mockup 4).
    NoRoom(Dialog),
    /// The resume did not take; say so (mockup 5).
    ResumeFailed(Dialog),
    /// Nothing was done, and why — a lock that stayed busy, a conversation already running
    /// somewhere it could not be attached to.
    Refused(String),
}
