//! The menu's renderer: a [`View`] in, a [`Frame`] out, and nothing else — no terminal, no
//! clock, no registry. That is what lets it be tested against the approved screens.
//!
//! **The screens in `docs/mockups.md` are binding** (owner, 2026-10-01), character for
//! character, at 40 and at 80 columns; the tests at the bottom of this file read the fenced
//! blocks out of both mockup files rather than keeping copies, so a change to the approved
//! screens breaks the build until the renderer follows it. Every layout number below is
//! measured off those screens, and each says which one. The one thing they do not fix is
//! the vertical gap: a full screen's footer sits on the terminal's last lines (owner,
//! 2026-10-03), and the screens draw it straight after the content only because they are
//! shorter than a terminal.
//!
//! Every glyph drawn here is ASCII, box drawing, `·` or `—` — the set the mockups use, which
//! was checked against the common monospace fonts on 2026-10-01 — so one `char` is one
//! column, and widths are counted in chars. Text that comes from outside (titles, the host,
//! the workspace, claude's stderr) has its control characters replaced before it is counted:
//! a title is the owner's first prompt, and an escape sequence in it must not reach the
//! terminal as one.

use crate::fmt::human;
use crate::ui::{Colour, Dialog, Frame, Group, Row, RowKey, Screen, Span, Style, TooNarrow, View};
use std::ops::Range;

const FG: Style = Style {
    fg: None,
    bold: false,
    dim: false,
    reverse: false,
};
const DIM: Style = Style { dim: true, ..FG };
const BOLD: Style = Style { bold: true, ..FG };
const BLUE: Style = Style {
    fg: Some(Colour::Blue),
    ..FG
};
/// Spent on the Needs-you heading and nothing else: the colour table in `docs/mockups.md`.
const AMBER: Style = Style {
    fg: Some(Colour::Amber),
    ..FG
};
const AMBER_BOLD: Style = Style {
    fg: Some(Colour::Amber),
    bold: true,
    ..FG
};

/// A key and what it does, drawn `key description` with the key in blue.
type Hint = (&'static str, &'static str);

/// The way out that is always offered. Its width is the narrowest the menu will draw at.
const WAY_OUT: Hint = ("s", "shell");
const HINTS: [Hint; 5] = [
    ("Enter", "open"),
    ("n", "new"),
    ("c", "close"),
    ("?", "keys"),
    WAY_OUT,
];
/// With nothing listed there is nothing to open or close (mockup 2).
const HINTS_EMPTY: [Hint; 3] = [("n", "new"), ("?", "keys"), WAY_OUT];
/// Between hint items, and between most dialog buttons: three spaces, as every mockup draws.
const GAP: &str = "   ";

/// The age field, right-aligned: wide enough for `now`, `14m`, `999d`.
const AGE_WIDTH: usize = 5;
/// Mockups 3 to 5: the box is 40 columns at either width, centred at 80 — "their content's
/// width", in `docs/mockups-80.md`. Below 40 it shrinks to the terminal.
const DIALOG_WIDTH: usize = 40;

/// Draw one frame.
///
/// **`TooNarrow` below the width of `s shell`, 7 columns.** Mockup 8 draws at 7 and refuses
/// at 6, and the rule that produces exactly that is the shell: it is the way out the hint
/// line always offers, and at 6 it no longer fits — the menu could still show `n new` and
/// `? keys`, but not the one item that gets the owner somewhere useful at a width like that.
/// So the refusal hands over the very thing the item would have: the door falls through to a
/// login shell and says why (`docs/design.md` § *The menu*, "below the width of the shortest
/// drawn way out"). `c close` is also 7 wide; that is a coincidence, not the reason.
pub fn render(view: &View) -> Result<Frame, TooNarrow> {
    let need = hint_width(WAY_OUT) as u16;
    if view.width < need {
        return Err(TooNarrow {
            width: view.width,
            need,
        });
    }
    let (w, h) = (view.width as usize, view.height as usize);
    // A dialog wins over either screen: it is a question the next key answers, so it must be
    // what is on the screen when that key is pressed.
    let mut lines = match (&view.dialog, view.screen) {
        // A dialog has no footer: it flows from the top and the rest of the screen is blank.
        (Some(dialog), _) => dialog_screen(view, dialog, w, h),
        (None, Screen::Keys) => pinned(keys_screen(view, w), h),
        (None, Screen::List) => pinned(list_screen(view, w, h), h),
    };
    lines.truncate(h);
    lines.resize_with(h, Line::default);
    Ok(Frame {
        lines: lines.into_iter().map(|l| l.cut(w)).collect(),
    })
}

impl Frame {
    /// The text alone: styles dropped, trailing spaces trimmed, lines joined with `\n`. What
    /// a pipe or a screen reader would get, and what the tests compare against the mockups.
    // The tests' view of a frame — the live path only ever writes `ansi()`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn plain(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                let text: String = line.iter().map(|s| s.text.as_str()).collect();
                text.trim_end_matches(' ').to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A full redraw: home the cursor, then each line cleared and drawn. No newline after the
    /// last line — on the bottom row it would scroll the screen.
    ///
    /// **The clear comes before the line, never after it.** A line that fills the last column
    /// leaves the terminal's cursor on that column with a wrap pending, and an erase sent then
    /// erases from where the cursor is — the last character. That is how the owner came to see
    /// `no` for `now` and `14` for `14m` on a real terminal (0.3.2), while pyte, which does not
    /// model the pending wrap, drew it whole. Cleared first, a line can fill the width safely.
    ///
    /// Trailing blanks are not sent unless they are reverse video: the clear has already
    /// blanked them, and on a metered link they are bytes that draw nothing.
    pub fn ansi(&self) -> String {
        let mut out = String::from("\x1b[H");
        for i in 0..self.lines.len() {
            if i > 0 {
                out.push_str("\r\n");
            }
            out.push_str(&self.ansi_line(i));
        }
        out
    }

    /// One line as `ansi` writes it: cleared, then drawn. The loop sends only the lines that
    /// changed, each after a move to its row, so a spinner turning costs one line, not a
    /// screen.
    pub fn ansi_line(&self, i: usize) -> String {
        let line = &self.lines[i];
        let mut out = String::from(CLEAR_LINE);
        let ink = line
            .iter()
            .rposition(|s| s.style.reverse || s.text.chars().any(|c| c != ' '));
        let Some(last) = ink else {
            return out;
        };
        for (j, span) in line[..=last].iter().enumerate() {
            let text = if j == last && !span.style.reverse {
                span.text.trim_end_matches(' ')
            } else {
                span.text.as_str()
            };
            let blank = !span.style.reverse && text.chars().all(|c| c == ' ');
            match sgr(span.style) {
                Some(code) if !blank => {
                    out.push_str(&code);
                    out.push_str(text);
                    out.push_str("\x1b[0m");
                }
                _ => out.push_str(text),
            }
        }
        out
    }
}

/// Erase the whole line the cursor is on, whatever column it is in.
const CLEAR_LINE: &str = "\x1b[2K";

/// The SGR sequence for a style, or `None` for the plain foreground.
fn sgr(style: Style) -> Option<String> {
    let mut codes = Vec::new();
    if style.bold {
        codes.push("1");
    }
    if style.dim {
        codes.push("2");
    }
    if style.reverse {
        codes.push("7");
    }
    match style.fg {
        Some(Colour::Amber) => codes.push("38;5;214"),
        Some(Colour::Blue) => codes.push("38;5;75"),
        None => {}
    }
    (!codes.is_empty()).then(|| format!("\x1b[{}m", codes.join(";")))
}

/// A full screen's two parts laid on the terminal: the top from line 0 down, the footer —
/// closing rule, status, hints — on the last lines, and blank between (owner, 2026-10-03;
/// the mockups show the footer straight after the content only because they are drawn
/// shorter than a terminal). With no room for a gap the footer follows the top directly, so
/// nothing overlaps, and the cut to `h` takes the same lines it did before the footer moved.
/// The top always starts on line 0, which keeps rows on line 2 for `menu.rs`'s click mapping.
fn pinned((top, footer): (Vec<Line>, Vec<Line>), h: usize) -> Vec<Line> {
    let gap = h.saturating_sub(top.len() + footer.len());
    let mut out = top;
    out.extend(std::iter::repeat_with(Line::default).take(gap));
    out.extend(footer);
    out
}

// ── Lines ───────────────────────────────────────────────────────────────────────────────

/// A line being built: spans, with adjacent spans of one style merged so the ANSI form does
/// not reset and re-set a style between two words that share it.
#[derive(Default)]
struct Line {
    spans: Vec<Span>,
    width: usize,
}

impl Line {
    fn push(&mut self, text: &str, style: Style) -> &mut Self {
        if text.is_empty() {
            return self;
        }
        let clean: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        self.width += clean.chars().count();
        match self.spans.last_mut() {
            Some(last) if last.style == style => last.text.push_str(&clean),
            _ => self.spans.push(Span { text: clean, style }),
        }
        self
    }

    /// Pad with spaces out to column `to`.
    fn pad(&mut self, to: usize, style: Style) -> &mut Self {
        if self.width < to {
            self.push(&" ".repeat(to - self.width), style);
        }
        self
    }

    fn append(&mut self, spans: Vec<Span>) -> &mut Self {
        for s in spans {
            self.push(&s.text, s.style);
        }
        self
    }

    /// The spans, cut to `width` columns. The last thing done to every line of a frame, so
    /// nothing a caller passes in can draw past the edge.
    fn cut(self, width: usize) -> Vec<Span> {
        let mut left = width;
        let mut out = Vec::new();
        for s in self.spans {
            if left == 0 {
                break;
            }
            let n = s.text.chars().count();
            if n <= left {
                left -= n;
                out.push(s);
            } else {
                out.push(Span {
                    text: s.text.chars().take(left).collect(),
                    style: s.style,
                });
                left = 0;
            }
        }
        out
    }
}

fn text(s: &str, style: Style) -> Line {
    let mut line = Line::default();
    line.push(s, style);
    line
}

fn blank() -> Line {
    Line::default()
}

fn rule(w: usize) -> Line {
    text(&"─".repeat(w), DIM)
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn hint_width((key, what): Hint) -> usize {
    width(key) + 1 + width(what)
}

/// Greedy word wrap. A word longer than a whole line is broken, because nothing else can make
/// it fit; a width of zero means there is no room to wrap into, and the line is left whole
/// for the frame's edge to cut.
fn wrap(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for word in s.split(' ').filter(|word| !word.is_empty()) {
        let mut chars: Vec<char> = word.chars().collect();
        while chars.len() > w {
            if cur_w > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            lines.push(chars.drain(..w).collect());
        }
        if chars.is_empty() {
            continue;
        }
        if cur_w > 0 && cur_w + 1 + chars.len() > w {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if cur_w > 0 {
            cur.push(' ');
            cur_w += 1;
        }
        cur_w += chars.len();
        cur.extend(chars);
    }
    if cur_w > 0 || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// `s` wrapped to `w` columns, every line indented by `indent`.
fn paragraph(out: &mut Vec<Line>, s: &str, indent: usize, w: usize, style: Style) {
    for l in wrap(s, w.saturating_sub(indent)) {
        let mut line = Line::default();
        line.pad(indent, FG).push(&l, style);
        out.push(line);
    }
}

/// A key in a column of its own and what it does beside it, as on the keys screen and the
/// empty list: the key's column is `col` wide after `indent`, and each segment of the
/// description starts a line of its own, wrapped with the same hanging indent.
fn keyed(
    out: &mut Vec<Line>,
    indent: usize,
    (key, style): (&str, Style),
    col: usize,
    segments: &[&str],
    w: usize,
) {
    let mut first = true;
    for seg in segments {
        for l in wrap(seg, w.saturating_sub(indent + col)) {
            let mut line = Line::default();
            line.pad(indent, FG);
            if first {
                line.push(key, style);
                first = false;
            }
            line.pad(indent + col, FG).push(&l, FG);
            out.push(line);
        }
    }
}

/// Hint items laid out greedily to `w`, `sep` between them. An item wider than the whole
/// width is dropped rather than cut — that is why `Enter open` vanishes at 9 columns in
/// mockup 8, while the shorter items wrap one to a line.
/// How many lines the hint takes at `width` — for `menu.rs`, which has to know how many rows
/// fit above it, and must count them the way they are drawn.
pub fn hint_line_count(width: u16, empty: bool) -> usize {
    let items: &[Hint] = if empty { &HINTS_EMPTY } else { &HINTS };
    hint_lines(items, GAP, usize::from(width)).len()
}

fn hint_lines(items: &[Hint], sep: &str, w: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut cur = Line::default();
    for &item in items {
        let iw = hint_width(item);
        if iw > w {
            continue;
        }
        if cur.width > 0 && cur.width + width(sep) + iw > w {
            lines.push(std::mem::take(&mut cur));
        }
        if cur.width > 0 {
            cur.push(sep, FG);
        }
        cur.push(item.0, BLUE).push(" ", FG).push(item.1, FG);
    }
    if cur.width > 0 {
        lines.push(cur);
    }
    lines
}

// ── The pieces every screen shares ──────────────────────────────────────────────────────

/// `infra-dev · 6 open · 812M of 1.0G`: the container's name in the foreground, the rest
/// dim, as structure.
fn header(view: &View) -> Line {
    // Closed rows are listed but are not open (owner, 2026-10-03). While the list is first
    // being read there is no count to give, so none is given.
    let mut rest = match view.rows.iter().filter(|r| !r.closed).count() {
        _ if loading(view) => String::new(),
        0 => " · nothing open".to_string(),
        n => format!(" · {n} open"),
    };
    if let Some((used, limit)) = view.header.memory {
        rest.push_str(&format!(" · {} of {}", human(used), human(limit)));
    }
    let mut line = text(&view.header.host, FG);
    line.push(&rest, DIM);
    line
}

/// One line of the list as laid out: a group's heading, a session, the Archived heading —
/// which the cursor can rest on, so it is a row of its own — or the blank line between two
/// groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Heading(Group),
    Row(usize),
    Fold(usize),
    Blank,
}

impl Item {
    /// The row the cursor is on when it is on this line, if it can be.
    pub fn selects(self) -> Option<usize> {
        match self {
            Item::Row(i) | Item::Fold(i) => Some(i),
            Item::Heading(_) | Item::Blank => None,
        }
    }
}

/// The list's lines: each group's heading, then its rows, with a blank line between groups
/// (owner, 2026-10-03). `rows` is in group order — `menu.rs` keeps it so — and a heading is
/// drawn wherever the group changes, so an empty group has none.
pub fn layout(rows: &[Row]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut last: Option<Group> = None;
    for (i, r) in rows.iter().enumerate() {
        let g = r.group();
        if last != Some(g) {
            if last.is_some() {
                out.push(Item::Blank);
            }
            // The Archived group's heading is its fold row, which `menu.rs` keeps first.
            if g != Group::Archived {
                out.push(Item::Heading(g));
            }
            last = Some(g);
        }
        out.push(if r.key == RowKey::ArchiveFold {
            Item::Fold(i)
        } else {
            Item::Row(i)
        });
    }
    out
}

/// Which lines of the layout `room` lines from `scroll` show, and how many sessions are
/// folded into the `… N more` line drawn at the foot when the rest does not fit (0 when it
/// does, and then there is no such line). The fold takes a line of its own, so it costs one
/// session's line and never hides that sessions are there.
pub fn window(items: &[Item], scroll: usize, room: usize) -> (Range<usize>, usize) {
    let scroll = scroll.min(items.len());
    if items.len() - scroll <= room {
        return (scroll..items.len(), 0);
    }
    let end = scroll + room.saturating_sub(1);
    let more = items[end..]
        .iter()
        .filter(|i| matches!(i, Item::Row(_)))
        .count();
    (scroll..end, if room == 0 { 0 } else { more })
}

/// The scroll that shows the session at index `cursor`, moving as little as possible from
/// `scroll`. Scrolling up to a group's first session brings its heading with it.
pub fn scroll_to(items: &[Item], cursor: usize, scroll: usize, room: usize) -> usize {
    let mut s = scroll.min(items.len().saturating_sub(room));
    let Some(at) = items.iter().position(|i| i.selects() == Some(cursor)) else {
        return s;
    };
    if at < s {
        s = match at.checked_sub(1).map(|h| items[h]) {
            Some(Item::Heading(_)) => at - 1,
            _ => at,
        };
    }
    while s < at && !window(items, s, room).0.contains(&at) {
        s += 1;
    }
    s
}

/// The list's lines from `scroll`, as many as `room` holds.
fn list_lines(view: &View, w: usize, room: usize) -> Vec<Line> {
    let items = layout(&view.rows);
    let (shown, more) = window(&items, view.scroll, room);
    let indent = indent(w);
    let mut out: Vec<Line> = items[shown]
        .iter()
        .map(|item| match *item {
            Item::Heading(g) => heading(g, &count(&view.rows, g), w, false),
            Item::Row(i) => row(&view.rows[i], w, indent, i == view.cursor, spin_on(view, i)),
            Item::Fold(i) => heading(Group::Archived, &view.rows[i].age, w, i == view.cursor),
            Item::Blank => blank(),
        })
        .collect();
    if more > 0 {
        // `…` is in every face `↵` was missing from — see `docs/mockups.md`.
        let mut line = Line::default();
        line.pad(indent, FG).push(&format!("… {more} more"), DIM);
        out.push(line);
    }
    out
}

/// From this width up, rows are indented under their heading (owner, 2026-10-03). Below it
/// — the phone, at 40 — the two columns stay with the titles.
const INDENT_FROM: usize = 60;
const INDENT: usize = 2;

fn indent(w: usize) -> usize {
    if w >= INDENT_FROM { INDENT } else { 0 }
}

/// How many sessions a group holds, as its heading says. The Archived heading carries its
/// own count, since its rows are not drawn while it is shut.
fn count(rows: &[Row], g: Group) -> String {
    rows.iter()
        .filter(|r| r.group() == g && r.key != RowKey::ArchiveFold)
        .count()
        .to_string()
}

/// A group's heading, as a labelled rule across the width (owner, 2026-10-03):
/// `── Closed · 7 ──────`. The rule is dim and the name bold; for Needs you, the one group
/// waiting for the owner, the whole line is amber. A rule is a glyph, so a monochrome
/// terminal and a pipe still see a divider. `cursor` is for the Archived heading, which the
/// cursor can rest on.
fn heading(g: Group, count: &str, w: usize, cursor: bool) -> Line {
    let st = |s: Style| Style {
        reverse: cursor,
        ..s
    };
    let (rule_style, name_style) = match g {
        Group::NeedsYou => (AMBER, AMBER_BOLD),
        _ => (DIM, BOLD),
    };
    let label = format!("{} · {count}", g.name());
    let mut line = Line::default();
    line.push("── ", st(rule_style))
        .push(&label, st(name_style));
    let rest = w.saturating_sub(line.width + 1);
    line.push(&format!(" {}", "─".repeat(rest)), st(rule_style));
    line
}

fn heading_style(g: Group) -> Style {
    match g {
        Group::NeedsYou => AMBER_BOLD,
        _ => BOLD,
    }
}

/// A session: its title, cut to fit, and its age right-aligned in the last five columns. No
/// marks and no number (owner, 2026-10-03): the group says what state it is in, an unread
/// title is bold, and a session attached somewhere else is drawn dim.
fn row(r: &Row, w: usize, indent: usize, cursor: bool, spin: Option<&str>) -> Line {
    // The cursor is reverse video across the whole row (owner, 2026-10-02): a monochrome
    // terminal shows it too, and the styles inside it still read.
    let st = |s: Style| Style {
        reverse: cursor,
        dim: s.dim || r.attached,
        ..s
    };
    let title_style = if r.unread { BOLD } else { FG };
    let title_w = w.saturating_sub(AGE_WIDTH + indent);
    let mut line = Line::default();
    line.pad(indent, st(FG));
    // Cut, with no ellipsis: the fold line is the one place `…` is drawn, as a word.
    let title: String = r.title.chars().take(title_w).collect();
    line.push(&title, st(title_style))
        .pad(indent + title_w, st(FG));
    // While it is being worked on, the spinner stands in for its age, in the foreground.
    match spin {
        Some(glyph) => line.push(&format!("{glyph:>AGE_WIDTH$}"), st(FG)),
        None => {
            let age: String = r.age.chars().take(AGE_WIDTH).collect();
            line.push(&format!("{age:>AGE_WIDTH$}"), st(DIM))
        }
    };
    line
}

// ── Screens ─────────────────────────────────────────────────────────────────────────────

/// Mockups 1, 2 and 6: header, rule and the list's lines that fit on top; the closing rule,
/// the status line if there is one and the hint line as the footer.
fn list_screen(view: &View, w: usize, h: usize) -> (Vec<Line>, Vec<Line>) {
    let mut out = vec![header(view), rule(w)];
    let hints = if let Some(b) = view.busy.as_ref().filter(|b| b.loading) {
        // Mockup 10: the frame at once, and the spinner where the list will go.
        out.push(blank());
        let mut line = Line::default();
        line.push(b.glyph(), FG).push(" ", FG).push(&b.what, DIM);
        out.push(line);
        hint_lines(&HINTS_EMPTY, GAP, w)
    } else if view.rows.is_empty() {
        out.extend(empty_body(view, w));
        hint_lines(&HINTS_EMPTY, GAP, w)
    } else {
        let tail = 1 + status_lines(view, w).len() + hints_len(w);
        let room = h.saturating_sub(out.len() + tail);
        out.extend(list_lines(view, w, room));
        hint_lines(&HINTS, GAP, w)
    };
    let mut footer = vec![rule(w)];
    footer.extend(status_lines(view, w).iter().map(|l| text(l, FG)));
    footer.extend(hints);
    (out, footer)
}

/// The status line, wrapped rather than cut: it is usually a reason something was refused,
/// and a reason cut off at the edge of a phone screen is no reason at all. Mockup 6's fits on
/// one line, as most do.
///
/// While something is being worked on, the line is the spinner and what is happening, in
/// place of any status (mockup 11).
fn status_lines(view: &View, w: usize) -> Vec<String> {
    match &view.busy {
        Some(b) if !b.loading => wrap(&format!("{} {}", b.glyph(), b.what), w),
        _ => view
            .status
            .as_deref()
            .map(|s| wrap(s, w))
            .unwrap_or_default(),
    }
}

/// The list is still being read for the first time.
fn loading(view: &View) -> bool {
    view.busy.as_ref().is_some_and(|b| b.loading)
}

/// The spinner's glyph if it stands in for row `i`'s age.
fn spin_on(view: &View, i: usize) -> Option<&'static str> {
    view.busy
        .as_ref()
        .filter(|b| !b.loading && b.row == Some(i))
        .map(|b| b.glyph())
}

/// How many lines a status takes at `width` — for `menu.rs`, which must leave room for it.
pub fn status_line_count(width: u16, status: &str) -> usize {
    wrap(status, usize::from(width)).len()
}

fn hints_len(w: usize) -> usize {
    hint_lines(&HINTS, GAP, w).len()
}

/// Mockup 2, between the rules. `s   a shell instead` is the owner's correction of
/// 2026-10-02 (it read `q`, which quits).
fn empty_body(view: &View, w: usize) -> Vec<Line> {
    let mut out = vec![blank()];
    paragraph(&mut out, "No claude session in this container.", 2, w, FG);
    out.push(blank());
    let start = format!("start one in {}", view.workspace);
    keyed(&mut out, 2, ("n", BLUE), 4, &[&start], w);
    keyed(&mut out, 2, ("s", BLUE), 4, &["a shell instead"], w);
    out.push(blank());
    out
}

/// Mockup 7, on `?`. Keys are blue, as everywhere a key can be pressed; each group's name is
/// drawn as its heading is, so the screen is also the key to the list. The closing rule and
/// the hint line are the footer.
fn keys_screen(view: &View, w: usize) -> (Vec<Line>, Vec<Line>) {
    let mut head = text(&view.header.host, FG);
    head.push(" · keys", DIM);
    let mut out = vec![head, rule(w)];
    let ws = &view.workspace;
    let new = format!("new session in {ws}");
    let shell = format!("a shell in {ws}");
    // `Enter`'s line breaks after "it" at 80 columns as well as at 40: the approved screens
    // break it there, so it is two segments rather than one wrapped sentence.
    let keys: [(&str, &[&str]); 6] = [
        (
            "Enter",
            &["open the session; resumes it", "if offloaded or closed"],
        ),
        ("n", &[&new]),
        ("c", &["close the session, or", "archive a closed one"]),
        ("s", &[&shell]),
        ("Esc", &["quit the launcher"]),
        ("?", &["this"]),
    ];
    for (key, what) in keys {
        keyed(&mut out, 0, (key, BLUE), 7, what, w);
    }
    out.push(blank());
    for g in Group::ALL {
        let what: &[&str] = match g {
            Group::NeedsYou => &["a prompt is waiting"],
            Group::Working => &["claude is mid-turn"],
            Group::Idle => &["at its prompt, waiting"],
            Group::Offloaded => &["stopped to save memory"],
            Group::Closed => &["ended; still resumable"],
            Group::Archived => &[
                "put away with c, or after",
                "30 days unused; Enter on it",
                "opens or shuts it",
            ],
        };
        keyed(&mut out, 0, (g.name(), heading_style(g)), 11, what, w);
    }
    let hints: &[Hint] = if view.rows.is_empty() {
        &HINTS_EMPTY
    } else {
        &HINTS
    };
    let mut footer = vec![rule(w)];
    footer.extend(hint_lines(hints, GAP, w));
    (out, footer)
}

/// Mockups 3 to 5: header, rule, the session the dialog is about under its group's heading,
/// the box, and nothing after it — no closing rule and no hint line, because the box carries
/// its own keys.
fn dialog_screen(view: &View, dialog: &Dialog, w: usize, h: usize) -> Vec<Line> {
    let box_w = w.min(DIALOG_WIDTH);
    // Header and rule above, the box's own two borders around the body.
    let room = h.saturating_sub(4);
    let body = dialog_body(dialog, box_w.saturating_sub(4), room);
    let mut out = vec![header(view), rule(w)];
    // In a short terminal the context goes first; the question never does.
    if let Some(r) = view.about.and_then(|i| view.rows.get(i).map(|r| (i, r))) {
        if room.saturating_sub(body.len()) >= 2 {
            let g = r.1.group();
            out.push(heading(g, &count(&view.rows, g), w, false));
            out.push(row(r.1, w, indent(w), r.0 == view.cursor, None));
        }
    }
    out.extend(boxed(body, w, box_w));
    out
}

/// What a dialog says, wrapped to `tw` — 36 columns in a 40-column box, one space of padding
/// each side. The first line is the question, in bold: a question is not a warning, so it is
/// not amber. Indented lines (a title, claude's stderr) wrap with their indent.
fn dialog_body(dialog: &Dialog, tw: usize, room: usize) -> Vec<Line> {
    let mut out = Vec::new();
    match dialog {
        Dialog::Close { title, running } => {
            paragraph(&mut out, "Close this session?", 0, tw, BOLD);
            paragraph(&mut out, title, 2, tw, FG);
            out.push(blank());
            // Closing is never destructive, and the dialog says so either way: a live slot's
            // process is stopped, an offloaded one has no process and is only hidden.
            let what = if *running {
                "Running. Stops the process, not the conversation — resumable from disk."
            } else {
                "Offloaded. Hides the row, not the conversation — claude --resume still finds it."
            };
            paragraph(&mut out, what, 0, tw, FG);
            out.push(blank());
            // Four spaces between these two, three in mockup 5: as drawn.
            out.extend(hint_lines(&[("y", "close"), ("n", "keep")], "    ", tw));
        }
        Dialog::NoRoom {
            used,
            limit,
            want,
            offer,
        } => {
            paragraph(&mut out, "No room for another claude", 0, tw, BOLD);
            out.push(blank());
            let why = format!(
                "{} of {} used. A new one wants about {}.",
                human(*used),
                human(*limit),
                human(*want)
            );
            paragraph(&mut out, &why, 0, tw, FG);
            out.push(blank());
            match offer {
                Some((_, age, title)) => {
                    let ask = format!("Offload this session, idle {age}?");
                    paragraph(&mut out, &ask, 0, tw, FG);
                    paragraph(&mut out, title, 2, tw, FG);
                    paragraph(&mut out, "resumable from disk", 2, tw, FG);
                    out.push(blank());
                    let buttons = [("y", "offload, then open"), ("n", "cancel")];
                    out.extend(hint_lines(&buttons, "    ", tw));
                }
                // Nothing to offer, so nothing to accept: the dialog can only say so and go
                // back. Closing a live slot is what frees memory, so that is the advice.
                None => {
                    let none = "Nothing can be offloaded right now. Close a session to make room.";
                    paragraph(&mut out, none, 0, tw, FG);
                    out.push(blank());
                    out.extend(hint_lines(&[("Esc", "back")], GAP, tw));
                }
            }
        }
        Dialog::ResumeFailed {
            title,
            session,
            killed_for_memory,
            output,
            closed,
        } => {
            paragraph(&mut out, "This session did not resume", 0, tw, BOLD);
            paragraph(&mut out, title, 2, tw, FG);
            out.push(blank());
            // zmx does not report its program's status (owner, 2026-10-06: `ended`, not
            // `exited 1`); the one thing the number told, a kill for memory, is read from the
            // cgroup instead.
            let cmd = if *killed_for_memory {
                format!("claude --resume {session} was killed for memory")
            } else {
                format!("claude --resume {session} ended")
            };
            paragraph(&mut out, &cmd, 0, tw, FG);
            let mut said = Vec::new();
            for l in output {
                paragraph(&mut said, l, 2, tw, FG);
            }
            // A claude that dies before writing anything still gets a line: an empty space
            // under the command reads as the dialog having lost the reason (issue #5).
            if said.is_empty() {
                paragraph(&mut said, "and wrote nothing to stderr", 2, tw, FG);
            }
            let mut tail = vec![blank()];
            let left = if *closed {
                "Left closed. Nothing was deleted; the transcript may be gone."
            } else {
                "Left offloaded. Nothing was deleted; the transcript may be gone."
            };
            paragraph(&mut tail, left, 0, tw, FG);
            tail.push(blank());
            let buttons = [("r", "retry"), ("c", "close it"), ("Esc", "back")];
            tail.extend(hint_lines(&buttons, GAP, tw));
            // stderr is the one part of unbounded length. When it cannot all fit, its last
            // lines are the ones kept — that is where the error is — and the keys always are.
            let fits = room.saturating_sub(out.len() + tail.len());
            let skip = said.len().saturating_sub(fits);
            out.extend(said.into_iter().skip(skip));
            out.extend(tail);
        }
    }
    out
}

/// The body framed in `╭╮╰╯│─`, dim, centred in the terminal.
fn boxed(body: Vec<Line>, w: usize, box_w: usize) -> Vec<Line> {
    let left = (w - box_w) / 2;
    let inner = box_w.saturating_sub(2);
    let tw = box_w.saturating_sub(4);
    let edge = |l: &str, r: &str| {
        let mut line = Line::default();
        line.pad(left, FG)
            .push(&format!("{l}{}{r}", "─".repeat(inner)), DIM);
        line
    };
    let mut out = vec![edge("╭", "╮")];
    for b in body {
        let mut line = Line::default();
        line.pad(left, FG).push("│", DIM).push(" ", FG);
        line.append(b.cut(tw))
            .pad(left + 2 + tw, FG)
            .push(" ", FG)
            .push("│", DIM);
        out.push(line);
    }
    out.push(edge("╰", "╯"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{Busy, Header, RowKey};

    const MOCKUPS_40: &str = include_str!("../docs/mockups.md");
    const MOCKUPS_80: &str = include_str!("../docs/mockups-80.md");
    /// Both sets, each at the width it is drawn at.
    const SETS: [(&str, u16); 2] = [(MOCKUPS_40, 40), (MOCKUPS_80, 80)];
    const MIB: u64 = 1024 * 1024;

    /// The fenced block under `### Mockup {n} ` — read from the doc, never copied, so the
    /// approved screens stay the only source of truth.
    fn mockup(doc: &'static str, n: usize) -> Vec<&'static str> {
        let heading = format!("### Mockup {n} ");
        let mut lines = doc.lines().skip_while(|l| !l.starts_with(&heading));
        assert!(lines.next().is_some(), "no mockup {n} in the doc");
        let mut lines = lines.skip_while(|l| *l != "```");
        assert_eq!(lines.next(), Some("```"), "mockup {n} has no fenced block");
        lines.take_while(|l| *l != "```").collect()
    }

    /// A row from its flags: `!` wants you, `b` busy, `*` unread, `@` attached, `z`
    /// offloaded, `x` closed.
    fn row(flags: &str, title: &str, age: &str) -> Row {
        Row {
            key: RowKey::Slot(title.to_string()),
            wants_you: flags.contains('!'),
            busy: flags.contains('b'),
            unread: flags.contains('*'),
            attached: flags.contains('@'),
            offloaded: flags.contains('z'),
            closed: flags.contains('x'),
            archived: false,
            title: title.to_string(),
            age: age.to_string(),
        }
    }

    /// The sessions of mockup 1, in group order: six open, seven closed.
    fn listed() -> Vec<Row> {
        let mut claude = row("", "claude", "5h");
        claude.key = RowKey::Socket("claude".to_string());
        vec![
            row("!", "permission: write hosts/one", "2m"),
            row("b", "loop: watch the base build", "now"),
            row("*", "retire the old tunnel", "14m"),
            row("@", "immich upgrade", "3m"),
            claude,
            row("z", "mount guards on one", "2d"),
            row("x", "fix the dns records", "3d"),
            row("x", "tunnel cutover notes", "4d"),
            row("x", "bcache register script", "5d"),
            row("x", "syncthing share rename", "6d"),
            row("x", "pihole blocklist sync", "8d"),
            row("x", "two podman iptables", "9d"),
            row("x", "smartd on one", "12d"),
        ]
    }

    /// Indices into `listed()`.
    const RETIRE: usize = 2;
    const MOUNT: usize = 5;

    fn view(width: u16, rows: Vec<Row>, used_mib: u64) -> View {
        View {
            width,
            height: 24,
            header: Header {
                host: "infra-dev".to_string(),
                memory: Some((used_mib * MIB, 1024 * MIB)),
            },
            workspace: "/workspace".to_string(),
            rows,
            cursor: 0,
            scroll: 0,
            about: None,
            screen: Screen::List,
            dialog: None,
            status: None,
            busy: None,
        }
    }

    // The View each mockup depicts, at a given width.

    fn mockup_1(w: u16) -> View {
        view(w, listed(), 812)
    }
    fn mockup_2(w: u16) -> View {
        view(w, vec![], 812)
    }
    fn mockup_3(w: u16) -> View {
        View {
            cursor: RETIRE,
            about: Some(RETIRE),
            dialog: Some(Dialog::Close {
                title: "retire the old tunnel".to_string(),
                running: true,
            }),
            ..view(w, listed(), 812)
        }
    }
    fn mockup_4(w: u16) -> View {
        View {
            about: Some(RETIRE),
            dialog: Some(Dialog::NoRoom {
                used: 892 * MIB,
                limit: 1024 * MIB,
                want: 250 * MIB,
                offer: Some((
                    RETIRE,
                    "14m".to_string(),
                    "retire the old tunnel".to_string(),
                )),
            }),
            ..view(w, listed(), 892)
        }
    }
    fn mockup_5(w: u16) -> View {
        View {
            cursor: MOUNT,
            about: Some(MOUNT),
            dialog: Some(Dialog::ResumeFailed {
                title: "mount guards on one".to_string(),
                session: "0f9c4a1e".to_string(),
                killed_for_memory: false,
                output: vec!["No conversation found with that session id".to_string()],
                closed: false,
            }),
            ..view(w, listed(), 812)
        }
    }
    fn mockup_6(w: u16) -> View {
        let mut rows = listed();
        rows[RETIRE] = row("", "retire the old tunnel", "now");
        rows[3].age = "12m".to_string();
        View {
            cursor: RETIRE,
            status: Some("detached · it is still running".to_string()),
            ..view(w, rows, 1024)
        }
    }
    fn mockup_7(w: u16) -> View {
        View {
            screen: Screen::Keys,
            ..view(w, listed(), 812)
        }
    }

    /// Mockup 9: the Archived group open, the cursor on its heading, as `menu.rs` lays the
    /// rows out — everything not archived, the heading's row, then the archived rows.
    fn mockup_9(w: u16) -> View {
        let mut fold = row("x", "Archived", "3");
        fold.key = RowKey::ArchiveFold;
        let archived = |t: &str, age: &str| Row {
            archived: true,
            ..row("x", t, age)
        };
        let mut claude = row("", "claude", "5h");
        claude.key = RowKey::Socket("claude".to_string());
        let rows = vec![
            row("*", "retire the old tunnel", "14m"),
            claude,
            row("x", "fix the dns records", "3d"),
            row("x", "tunnel cutover notes", "4d"),
            fold,
            archived("bcache register script", "41d"),
            archived("syncthing share rename", "52d"),
            archived("smartd on one", "63d"),
        ];
        View {
            cursor: 4,
            ..view(w, rows, 812)
        }
    }

    /// Mockup 10: the first frame while the list is still being read.
    fn mockup_10(w: u16) -> View {
        View {
            busy: Some(Busy {
                frame: 0,
                row: None,
                what: "reading sessions".to_string(),
                loading: true,
            }),
            ..view(w, vec![], 812)
        }
    }

    /// Mockup 11: closing a running session, the spinner in its age and on the status line.
    fn mockup_11(w: u16) -> View {
        View {
            cursor: RETIRE,
            busy: Some(Busy {
                frame: 0,
                row: Some(RETIRE),
                what: "closing session".to_string(),
                loading: false,
            }),
            ..view(w, listed(), 812)
        }
    }

    /// Prints the screens the mockup files draw, for redrawing those files to width:
    /// `cargo test print_the_mockups -- --ignored --nocapture`. The files list and keys
    /// screens without the blank gap above the footer (see `compare`).
    #[test]
    #[ignore]
    fn print_the_mockups() {
        // Numbered as the files number them; mockup 8 is the refusal, not a View.
        type Build = fn(u16) -> View;
        let builds: [(u8, Build); 10] = [
            (1, mockup_1),
            (2, mockup_2),
            (3, mockup_3),
            (4, mockup_4),
            (5, mockup_5),
            (6, mockup_6),
            (7, mockup_7),
            (9, mockup_9),
            (10, mockup_10),
            (11, mockup_11),
        ];
        for w in [40u16, 80] {
            for (n, b) in builds {
                let p = render(&b(w)).unwrap().plain();
                let p = p.trim_end_matches('\n');
                println!("=== {n} {w}");
                println!("{p}");
            }
        }
    }

    fn lines(frame: &Frame) -> Vec<String> {
        frame.plain().split('\n').map(str::to_string).collect()
    }

    /// The frame's shape, whatever it shows: exactly `height` lines, none past `width`.
    fn check_shape(frame: &Frame, v: &View) -> Result<(), String> {
        if frame.lines.len() != v.height as usize {
            return Err(format!(
                "{} lines for height {}",
                frame.lines.len(),
                v.height
            ));
        }
        for (i, line) in frame.lines.iter().enumerate() {
            let n: usize = line.iter().map(|s| s.text.chars().count()).sum();
            if n > v.width as usize {
                return Err(format!("line {i} is {n} wide at width {}", v.width));
            }
        }
        Ok(())
    }

    /// The comparison every mockup test makes. Returns rather than asserts so the calibration
    /// test can watch it fail.
    fn matches(v: &View, expected: &[&str]) -> Result<(), String> {
        let frame = render(v).map_err(|e| format!("{e:?}"))?;
        compare(&frame, v, expected)
    }

    /// A dialog's screen is its lines from the top and then blank. Any other screen has a
    /// footer, which sits on the terminal's last lines (owner, 2026-10-03): the mockup down to
    /// its closing rule is the frame's first lines, the closing rule onward is its last
    /// lines, and everything between is blank.
    fn compare(frame: &Frame, v: &View, expected: &[&str]) -> Result<(), String> {
        check_shape(frame, v)?;
        let got = lines(frame);
        if got.len() < expected.len() {
            return Err("the frame is shorter than the mockup".to_string());
        }
        let split = if v.dialog.is_some() {
            expected.len()
        } else {
            closing_rule(expected)?
        };
        let (top, footer) = expected.split_at(split);
        let bottom = got.len() - footer.len();
        let want = top
            .iter()
            .enumerate()
            .chain(footer.iter().enumerate().map(|(i, e)| (bottom + i, e)));
        for (i, e) in want {
            if got[i] != *e {
                return Err(format!("line {i}:\n  got  {:?}\n  want {e:?}", got[i]));
            }
        }
        if let Some(i) = got[top.len()..bottom].iter().position(|l| !l.is_empty()) {
            return Err(format!("line {} should be blank", top.len() + i));
        }
        Ok(())
    }

    /// Where a full screen's footer starts: its closing rule, the last line drawn in `─`
    /// after the header's own rule.
    fn closing_rule(expected: &[&str]) -> Result<usize, String> {
        match expected.iter().rposition(|l| l.starts_with('─')) {
            Some(i) if i > 1 => Ok(i),
            _ => Err("the mockup has no closing rule".to_string()),
        }
    }

    /// The line the last hint is drawn on: the last line with ink on it. `None` when the
    /// last ink is not a hint line (it must start with a key from the hint line).
    fn last_hint_line(frame: &Frame) -> Option<usize> {
        let got = lines(frame);
        let i = got.iter().rposition(|l| !l.is_empty())?;
        let keys = ["Enter ", "n ", "c ", "? ", "s "];
        keys.iter().any(|k| got[i].starts_with(k)).then_some(i)
    }

    /// The frame as the renderer drew it before the footer moved: the first `top` lines, the
    /// last `footer` lines straight after them, then blank. For calibration only — the layout
    /// the checks for the new one must reject.
    fn footer_raised(frame: &Frame, top: usize, footer: usize) -> Frame {
        let n = frame.lines.len();
        let mut lines = frame.lines[..top].to_vec();
        lines.extend_from_slice(&frame.lines[n - footer..]);
        lines.resize_with(n, Vec::new);
        Frame { lines }
    }

    fn assert_mockup(n: usize, build: fn(u16) -> View) {
        for (doc, w) in SETS {
            let expected = mockup(doc, n);
            if let Err(e) = matches(&build(w), &expected) {
                panic!("mockup {n} at {w} columns: {e}");
            }
        }
    }

    // ── The approved screens, at 40×24 and 80×24 ─────────────────────────────────────

    #[test]
    fn mockup_1_the_list_in_its_groups() {
        assert_mockup(1, mockup_1);
    }

    #[test]
    fn mockup_2_nothing_open() {
        assert_mockup(2, mockup_2);
    }

    #[test]
    fn mockup_3_closing_a_live_session() {
        assert_mockup(3, mockup_3);
    }

    #[test]
    fn mockup_4_no_room_to_open_another() {
        assert_mockup(4, mockup_4);
    }

    #[test]
    fn mockup_5_a_resume_that_fails() {
        assert_mockup(5, mockup_5);
    }

    #[test]
    fn mockup_6_back_from_a_session_with_a_status_line() {
        assert_mockup(6, mockup_6);
    }

    #[test]
    fn mockup_7_the_keys() {
        assert_mockup(7, mockup_7);
    }

    #[test]
    fn mockup_9_the_archive_opened() {
        assert_mockup(9, mockup_9);
    }

    #[test]
    fn the_spinner_stands_in_for_the_age_and_the_status_and_steps_through_every_frame() {
        let mut v = mockup_11(40);
        v.status = Some("an earlier status".to_string());
        v.cursor = 0;
        let f = render(&v).unwrap();
        let p = lines(&f);
        // Its row: the title as ever, the spinner where the age was, in the foreground.
        assert_eq!(p[9], format!("{:<35}{:>5}", "retire the old tunnel", "⠋"));
        assert_eq!(style_at(&f, 9, 39), FG);
        // The status line says what is happening, in place of any status.
        assert!(p.contains(&"⠋ closing session".to_string()), "{p:?}");
        assert!(!p.iter().any(|l| l.contains("earlier status")));
        // Every other row keeps its age.
        assert!(p[10].ends_with("3m"));
        // Ten frames, in Compose's order, then round again.
        let glyphs: String = (0..11)
            .map(|frame| {
                let b = Busy {
                    frame,
                    ..v.busy.clone().unwrap()
                };
                b.glyph()
            })
            .collect();
        assert_eq!(glyphs, "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⠋");
        // Calibration: with nothing being worked on, the age and the status are back.
        let idle = View { busy: None, ..v };
        let p = lines(&render(&idle).unwrap());
        assert!(p[9].ends_with("14m"));
        assert!(p.contains(&"an earlier status".to_string()));
    }

    #[test]
    fn the_first_frame_while_reading_has_no_count_and_offers_only_what_needs_no_list() {
        let p = lines(&render(&mockup_10(40)).unwrap());
        assert_eq!(p[0], "infra-dev · 812M of 1.0G", "no count yet");
        assert_eq!(p[3], "⠋ reading sessions");
        assert_eq!(p[23], "n new   ? keys   s shell");
        // Calibration: the same empty list, read, says nothing is open.
        let read = View {
            busy: None,
            ..mockup_10(40)
        };
        assert!(lines(&render(&read).unwrap())[0].contains("nothing open"));
    }

    #[test]
    fn mockup_10_the_first_frame_while_the_list_is_read() {
        assert_mockup(10, mockup_10);
    }

    #[test]
    fn mockup_11_closing_a_session_the_spinner_turning() {
        assert_mockup(11, mockup_11);
    }

    #[test]
    fn the_archived_heading_is_a_row_the_cursor_rests_on_and_the_fold_never_counts_it() {
        let v = mockup_9(40);
        let items = layout(&v.rows);
        // Its heading is the fold row itself: no separate heading line for the group.
        assert!(!items.contains(&Item::Heading(Group::Archived)));
        assert_eq!(
            items.iter().filter(|i| matches!(i, Item::Fold(_))).count(),
            1
        );
        assert_eq!(Item::Fold(4).selects(), Some(4));
        // Under the cursor it is reverse across the width, like a row.
        let f = render(&v).unwrap();
        let at = 2 + items.iter().position(|i| *i == Item::Fold(4)).unwrap();
        assert!(f.lines[at].iter().all(|s| s.style.reverse));
        assert!(lines(&f)[at].starts_with("── Archived · 3 ─"));
        // Calibration: with the cursor elsewhere it is not.
        let f = render(&View {
            cursor: 0,
            ..v.clone()
        })
        .unwrap();
        assert!(f.lines[at].iter().all(|s| !s.style.reverse));
        // `… N more` counts sessions, not the heading: shut, with one line of room above
        // the closed rows, the fold line counts the two closed rows only.
        let mut shut = v.clone();
        shut.rows.truncate(5);
        let items = layout(&shut.rows);
        let start = items.iter().position(|i| *i == Item::Row(2)).unwrap();
        assert_eq!(window(&items, start, 2), (start..start + 1, 1));
    }

    #[test]
    fn rows_are_indented_from_sixty_columns() {
        let at = |w: u16| lines(&render(&mockup_1(w)).unwrap())[3].clone();
        assert!(at(59).starts_with("permission"), "{:?}", at(59));
        assert!(at(60).starts_with("  permission"), "{:?}", at(60));
        // The heading is never indented, and still reaches the edge.
        let h = lines(&render(&mockup_1(60)).unwrap())[2].clone();
        assert!(h.starts_with("── Needs you · 1 ─"));
        assert_eq!(h.chars().count(), 60);
    }

    #[test]
    fn a_long_status_wraps_and_loses_nothing() {
        // A refusal's reason, at 40 columns: every word must reach the screen.
        let reason = "\"retire the old tunnel\" ran in /home/owner/projects/old-thing, which is \
                      gone - claude --resume would start somewhere else";
        let mut v = View {
            status: Some(reason.to_string()),
            ..mockup_1(40)
        };
        v.rows.truncate(2);
        let plain = render(&v).unwrap().plain();
        let shown: Vec<&str> = plain.split_whitespace().collect();
        for word in reason.split_whitespace() {
            assert!(
                shown.contains(&word),
                "{word:?} was cut from the status line"
            );
        }
        assert!(
            status_line_count(40, reason) > 1,
            "calibration: it does need wrapping"
        );
        // And the list gives way to it rather than the hint line falling off the bottom.
        v.height = 9;
        let frame = render(&v).unwrap();
        assert!(
            frame.plain().contains("s shell"),
            "the hint must survive a long status"
        );
    }

    #[test]
    fn the_footer_sits_on_the_last_line_of_the_terminal() {
        // Owner, 2026-10-03: the list, the empty list and the keys screen put their closing
        // rule, status and hints on the terminal's last lines, however little is above them.
        let builds: [fn(u16) -> View; 4] = [mockup_1, mockup_2, mockup_6, mockup_7];
        for (doc, w) in SETS {
            for (n, build) in [1, 2, 6, 7].into_iter().zip(builds) {
                let v = build(w);
                assert_eq!(v.height, 24);
                let f = render(&v).unwrap();
                assert_eq!(last_hint_line(&f), Some(23), "mockup {n} at {w}x24");
                // Mockups 1 and 6 fill the screen, so the footer is at the bottom either way.
                let expected = mockup(doc, n);
                if expected.len() == 24 {
                    continue;
                }
                // Calibration: the same frame laid out as before the move — footer straight
                // after the content — must fail both this check and the mockup comparison.
                let split = closing_rule(&expected).unwrap();
                let old = footer_raised(&f, split, expected.len() - split);
                assert_eq!(
                    last_hint_line(&old),
                    Some(expected.len() - 1),
                    "calibration: the raised footer's last hint is where the mockup draws it"
                );
                assert!(
                    compare(&old, &v, &expected).is_err(),
                    "mockup {n} at {w}: the old layout passed the new comparison"
                );
            }
        }
        // A dialog draws no footer: its last line is the box's bottom, not a hint.
        for build in [mockup_3, mockup_4, mockup_5] {
            assert_eq!(last_hint_line(&render(&build(40)).unwrap()), None);
        }
    }

    // ── The groups ────────────────────────────────────────────────────────────────────

    #[test]
    fn the_layout_heads_each_group_and_puts_one_blank_line_between_groups() {
        use Item::{Blank, Heading, Row as R};
        let items = layout(&listed());
        assert_eq!(
            items[..16],
            [
                Heading(Group::NeedsYou),
                R(0),
                Blank,
                Heading(Group::Working),
                R(1),
                Blank,
                Heading(Group::Idle),
                R(2),
                R(3),
                R(4),
                Blank,
                Heading(Group::Offloaded),
                R(5),
                Blank,
                Heading(Group::Closed),
                R(6),
            ]
        );
        assert_eq!(
            items.len(),
            22,
            "and the other six closed rows, no blank after"
        );
        // An empty group draws nothing at all: with no one waiting, the list opens on Working.
        let rest = listed()[1..].to_vec();
        assert_eq!(layout(&rest)[0], Heading(Group::Working));
        assert!(!layout(&rest).contains(&Heading(Group::NeedsYou)));
        assert_eq!(layout(&[]), []);
    }

    #[test]
    fn a_row_is_grouped_by_what_it_is_doing_stopped_first() {
        assert_eq!(row("", "a", "1m").group(), Group::Idle);
        assert_eq!(row("*@", "a", "1m").group(), Group::Idle);
        assert_eq!(row("b", "a", "1m").group(), Group::Working);
        assert_eq!(row("!b", "a", "1m").group(), Group::NeedsYou);
        assert_eq!(row("!bz", "a", "1m").group(), Group::Offloaded);
        assert_eq!(row("!bzx", "a", "1m").group(), Group::Closed);
    }

    #[test]
    fn what_does_not_fit_folds_into_a_count_of_the_sessions_left() {
        let items = layout(&listed());
        // Mockup 1 at 40: nineteen lines between the rules; three closed shown, four folded.
        assert_eq!(window(&items, 0, 19), (0..18, 4));
        assert_eq!(window(&items, 0, 20), (0..19, 3), "at 80, one hint line");
        // Everything fits: no fold line at all.
        assert_eq!(window(&items, 0, 22), (0..22, 0));
        assert_eq!(window(&items, 0, 99), (0..22, 0));
        // Scrolled to the end, nothing is below.
        assert_eq!(window(&items, 3, 19), (3..22, 0));
        // No room is no lines and no fold.
        assert_eq!(window(&items, 0, 0), (0..0, 0));
        // One line is the fold alone, counting every session.
        assert_eq!(window(&items, 0, 1), (0..0, 13));
    }

    #[test]
    fn scrolling_shows_the_cursor_and_brings_its_heading() {
        let items = layout(&listed());
        // The last closed session, at 40×24: scrolled until it is above the fold.
        let s = scroll_to(&items, 12, 0, 19);
        assert!(window(&items, s, 19).0.contains(&21));
        assert_eq!(s, 3, "the least scroll that shows it");
        // In six lines, scrolled down to the closed rows and back up to the first Idle
        // session: its heading comes with it. And to the top, the Needs-you heading.
        let deep = scroll_to(&items, 9, 0, 6);
        assert!(deep > 7, "{deep}");
        assert_eq!(scroll_to(&items, 2, deep, 6), 6);
        assert_eq!(scroll_to(&items, 0, deep, 6), 0);
        // Already on screen: nothing moves.
        assert_eq!(scroll_to(&items, 6, 0, 19), 0);
        // Calibration: the fold line is not on screen for a session. The fifth closed row is
        // on line 19, which is the fold at scroll 0, so scrolling must move.
        assert_eq!(window(&items, 0, 19).0.end, 18);
        assert!(scroll_to(&items, 10, 0, 19) > 0);
    }

    #[test]
    fn when_the_list_fills_the_space_the_fold_sits_on_the_closing_rule() {
        let rows: Vec<Row> = (0..30).map(|i| row("", &format!("r{i}"), "1m")).collect();
        for (w, hints) in [(40u16, 2), (80, 1)] {
            let v = view(w, rows.clone(), 812);
            let p = lines(&render(&v).unwrap());
            // 24 less header, rule, closing rule and hints; the heading and the fold take two.
            let room = 24 - 3 - hints;
            assert!(p[2].starts_with("── Idle · 30 ─"), "{:?}", p[2]);
            assert!(p[3].trim_start().starts_with("r0 "), "{:?}", p[3]);
            assert_eq!(
                p[1 + room].trim_start(),
                format!("… {} more", 30 - (room - 2))
            );
            assert!(p[2 + room].starts_with('─'), "the closing rule follows");
            assert!(p[23].ends_with("s shell"));
        }
    }

    #[test]
    fn nothing_is_numbered_and_nothing_is_marked() {
        for w in [40u16, 80] {
            let p = lines(&render(&mockup_1(w)).unwrap());
            let ind = indent(w as usize);
            let title_w = w as usize - AGE_WIDTH - ind;
            for (r, line) in [(0, 3), (1, 6), (2, 9), (5, 14), (6, 17)] {
                let r = &listed()[r];
                assert_eq!(
                    p[line],
                    format!("{:ind$}{:<title_w$}{:>5}", "", r.title, r.age),
                    "line {line} at {w}"
                );
            }
        }
    }

    #[test]
    fn a_dialog_shows_its_session_under_that_sessions_heading() {
        let p = lines(&render(&mockup_5(40)).unwrap());
        assert!(p[2].starts_with("── Offloaded · 1 ─"), "{:?}", p[2]);
        assert!(p[3].starts_with("mount guards on one"));
        assert!(p[4].starts_with('╭'));
        // With nothing to say it is about, the box follows the rule.
        let v = View {
            about: None,
            ..mockup_5(40)
        };
        assert!(lines(&render(&v).unwrap())[2].starts_with('╭'));
    }

    fn narrowing(doc: &'static str) -> Vec<(u16, Vec<&'static str>)> {
        let mut out: Vec<(u16, Vec<&str>)> = Vec::new();
        for l in mockup(doc, 8) {
            if let Some(rest) = l.strip_prefix("at ") {
                let w = rest.split(' ').next().unwrap().parse().unwrap();
                out.push((w, Vec::new()));
            } else if let Some((_, lines)) = out.last_mut() {
                lines.push(l);
            }
        }
        // Each block opens with its ruler; it must be the width it claims, or the parse (or
        // the doc) is wrong and nothing below it means anything.
        for (w, lines) in &mut out {
            let ruler = lines.remove(0);
            assert_eq!(ruler, "=".repeat(*w as usize), "mockup 8's ruler at {w}");
        }
        out
    }

    #[test]
    fn mockup_8_the_hint_line_as_the_terminal_narrows() {
        for (doc, _) in SETS {
            let blocks = narrowing(doc);
            assert_eq!(
                blocks.iter().map(|b| b.0).collect::<Vec<_>>(),
                [40, 34, 26, 18, 12, 9, 7, 6]
            );
            for (w, expected) in blocks {
                let v = mockup_1(w);
                if w == 6 {
                    assert_eq!(render(&v), Err(TooNarrow { width: 6, need: 7 }));
                    continue;
                }
                let frame = render(&v).expect("mockup 8 draws at this width");
                check_shape(&frame, &v).unwrap();
                let got = lines(&frame);
                // The hint line is the last thing drawn.
                let end = got.iter().rposition(|l| !l.is_empty()).unwrap() + 1;
                let hint = &got[end - expected.len()..end];
                assert_eq!(hint, expected.as_slice(), "the hint line at {w} columns");
            }
        }
    }

    #[test]
    fn too_narrow_holds_for_every_screen_and_seven_draws_them_all() {
        for build in [
            mockup_1, mockup_2, mockup_3, mockup_4, mockup_5, mockup_6, mockup_7,
        ] {
            for w in 0..7 {
                assert_eq!(render(&build(w)), Err(TooNarrow { width: w, need: 7 }));
            }
            let v = build(7);
            check_shape(&render(&v).unwrap(), &v).unwrap();
        }
    }

    #[test]
    fn closed_rows_are_listed_under_closed_and_not_counted_as_open() {
        let plain = render(&mockup_1(40)).unwrap().plain();
        assert!(plain.starts_with("infra-dev · 6 open"), "got {plain}");
        // Calibration: reopen one and it counts.
        let mut v = mockup_1(40);
        v.rows[6].closed = false;
        assert!(
            render(&v)
                .unwrap()
                .plain()
                .starts_with("infra-dev · 7 open")
        );
    }

    // ── Calibration: the comparison must be able to fail ──────────────────────────────

    #[test]
    fn the_mockup_comparison_fails_on_a_wrong_view() {
        // The parse finds real screens, not empty ones that would match anything.
        assert_eq!(mockup(MOCKUPS_40, 1).len(), 24);
        assert_eq!(mockup(MOCKUPS_80, 7).len(), 21);
        assert!(mockup(MOCKUPS_40, 5)[0].starts_with("infra-dev · 6 open"));

        let wrong_title = {
            let mut v = mockup_1(40);
            v.rows[RETIRE].title = "retire the new tunnel".to_string();
            v
        };
        let wrong_age = {
            let mut v = mockup_1(40);
            v.rows[0].age = "3m".to_string();
            v
        };
        let wrong_group = {
            let mut v = mockup_1(40);
            v.rows[1].busy = false;
            v
        };
        let wrong_memory = view(40, listed(), 813);
        let wrong_status = View {
            status: None,
            ..mockup_6(40)
        };
        let wrong_about = View {
            about: Some(MOUNT),
            ..mockup_4(40)
        };
        let wrong_dialog = View {
            dialog: Some(Dialog::Close {
                title: "retire the old tunnel".to_string(),
                running: false,
            }),
            ..mockup_3(40)
        };
        let wrong_workspace = View {
            workspace: "/work".to_string(),
            ..mockup_7(40)
        };
        let short = View {
            height: 10,
            ..mockup_1(40)
        };
        let doc = MOCKUPS_40;
        for (n, v) in [
            (1, wrong_title),
            (1, wrong_age),
            (1, wrong_group),
            (1, wrong_memory),
            (6, wrong_status),
            (4, wrong_about),
            (3, wrong_dialog),
            (7, wrong_workspace),
            (1, short),
            (1, mockup_1(80)),
            (2, mockup_1(40)),
        ] {
            assert!(
                matches(&v, &mockup(doc, n)).is_err(),
                "a wrong view passed mockup {n}"
            );
        }
        // And a trailing line of junk after the screen is caught.
        let mut expected = mockup(doc, 7);
        expected.pop();
        assert!(matches(&mockup_7(40), &expected).is_err());
    }

    // ── Colour, against the table in docs/mockups.md ──────────────────────────────────

    fn all_mockup_frames() -> Vec<(View, Frame)> {
        let mut out = Vec::new();
        for (_, w) in SETS {
            for build in [
                mockup_1, mockup_2, mockup_3, mockup_4, mockup_5, mockup_6, mockup_7, mockup_9,
                mockup_10, mockup_11,
            ] {
                let v = build(w);
                let f = render(&v).unwrap();
                out.push((v, f));
            }
        }
        out
    }

    fn chars_styled(frame: &Frame, pred: impl Fn(&Style) -> bool) -> String {
        frame
            .lines
            .iter()
            .flatten()
            .filter(|s| pred(&s.style))
            .map(|s| s.text.as_str())
            .collect()
    }

    /// The style of the character at `col` on `line`.
    fn style_at(f: &Frame, line: usize, col: usize) -> Style {
        let mut at = 0;
        for s in &f.lines[line] {
            let n = s.text.chars().count();
            if col < at + n {
                return s.style;
            }
            at += n;
        }
        panic!("no column {col} on line {line}");
    }

    #[test]
    fn amber_is_spent_on_the_needs_you_heading_and_nothing_else() {
        for (v, f) in all_mockup_frames() {
            let amber = chars_styled(&f, |s| s.fg == Some(Colour::Amber));
            let heading = amber.starts_with("── Needs you · ") && amber.ends_with('─');
            assert!(
                amber.is_empty() || amber == "Needs you" || heading,
                "amber on {amber:?} at {}",
                v.width
            );
        }
        // The Needs-you heading's rule and name, the name bold: one line of amber.
        let f = render(&mockup_1(40)).unwrap();
        assert_eq!(f.lines[2][0].style, AMBER);
        assert_eq!(f.lines[2][1].text, "Needs you · 1");
        assert_eq!(f.lines[2][1].style, AMBER_BOLD);
        assert_eq!(
            chars_styled(&f, |s| s.fg == Some(Colour::Amber)),
            lines(&f)[2]
        );
        // And on the keys screen, where it is the key to the heading.
        let k = render(&mockup_7(40)).unwrap();
        assert_eq!(
            chars_styled(&k, |s| s.fg == Some(Colour::Amber)),
            "Needs you"
        );
        // Calibration: with nobody waiting there is no amber anywhere.
        let mut v = mockup_1(40);
        v.rows[0].wants_you = false;
        let f = render(&v).unwrap();
        assert_eq!(chars_styled(&f, |s| s.fg == Some(Colour::Amber)), "");
    }

    #[test]
    fn keys_are_blue_and_the_keys_screen_draws_each_group_as_its_heading() {
        let f = render(&mockup_1(40)).unwrap();
        assert_eq!(
            chars_styled(&f, |s| s.fg == Some(Colour::Blue)),
            "Enternc?s"
        );
        let hint = &f.lines[22];
        let blue: Vec<&str> = hint
            .iter()
            .filter(|s| s.style == BLUE)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(blue, ["Enter", "n", "c", "?"]);
        let k = render(&mockup_7(40)).unwrap();
        let first = |i: usize| k.lines[i][0].clone();
        for i in [2, 4, 5, 7, 8, 9] {
            assert_eq!(first(i).style, BLUE, "key on line {i}");
        }
        assert_eq!(first(11).style, AMBER_BOLD, "Needs you");
        for i in 12..17 {
            assert_eq!(first(i).style, BOLD, "group on line {i}");
        }
        assert_eq!(first(16).text, "Archived");
    }

    #[test]
    fn headings_are_bold_unread_titles_bold_attached_rows_dim_ages_dim() {
        let f = render(&mockup_1(40)).unwrap();
        for line in [5, 8, 13, 16] {
            assert_eq!(style_at(&f, line, 0), DIM, "heading rule on line {line}");
            assert_eq!(style_at(&f, line, 3), BOLD, "heading name on line {line}");
            assert_eq!(style_at(&f, line, 39), DIM, "heading rule on line {line}");
        }
        // Line 3 is the Needs-you row: title in the foreground, age dim. But it is the cursor.
        let f = render(&View {
            cursor: 99,
            ..mockup_1(40)
        })
        .unwrap();
        assert_eq!(style_at(&f, 3, 0), FG, "title");
        assert_eq!(style_at(&f, 3, 39), DIM, "age");
        assert_eq!(style_at(&f, 9, 0), BOLD, "unread title");
        assert_eq!(style_at(&f, 9, 39), DIM, "unread age");
        assert_eq!(style_at(&f, 10, 0), DIM, "attached title");
        assert_eq!(style_at(&f, 10, 39), DIM, "attached age");
        assert_eq!(style_at(&f, 11, 0), FG, "plain title");
        // Calibration: the same rows with nothing unread or attached draw plainly.
        let mut v = View {
            cursor: 99,
            ..mockup_1(40)
        };
        v.rows[RETIRE].unread = false;
        v.rows[3].attached = false;
        let f = render(&v).unwrap();
        assert_eq!(style_at(&f, 9, 0), FG);
        assert_eq!(style_at(&f, 10, 0), FG);
        // The header: the container's name, then everything after it dim.
        assert_eq!(f.lines[0][0].text, "infra-dev");
        assert_eq!(f.lines[0][0].style, FG);
        assert_eq!(f.lines[0][1].style, DIM);
        assert_eq!(f.lines[0].len(), 2);
    }

    #[test]
    fn the_cursor_is_reverse_across_its_session_and_nowhere_else() {
        for w in [40u16, 80] {
            let rows = listed();
            let items = layout(&rows);
            for (cursor, r) in rows.iter().enumerate() {
                let mut v = View {
                    cursor,
                    ..mockup_1(w)
                };
                let room = 24 - 3 - hints_len(w as usize);
                v.scroll = scroll_to(&items, cursor, 0, room);
                let f = render(&v).unwrap();
                let at = items.iter().position(|i| *i == Item::Row(cursor)).unwrap();
                let line = 2 + at - v.scroll;
                for (i, l) in f.lines.iter().enumerate() {
                    let rev = l.iter().filter(|s| s.style.reverse).count();
                    if i == line {
                        assert_eq!(rev, l.len(), "the whole cursor row is reverse");
                        let n: usize = l.iter().map(|s| s.text.chars().count()).sum();
                        assert_eq!(n, w as usize, "the cursor row spans the width");
                        let text: String = l.iter().map(|s| s.text.as_str()).collect();
                        assert!(text.trim_start().starts_with(&r.title), "{text:?}");
                    } else {
                        assert_eq!(rev, 0, "line {i} is not the cursor ({cursor} at {w})");
                    }
                }
            }
        }
    }

    #[test]
    fn rules_and_dialog_frames_are_dim_and_a_dialog_asks_in_bold() {
        for (_, f) in all_mockup_frames() {
            for span in f.lines.iter().flatten() {
                // Rules and frames are dim — except the Needs-you heading's rule, amber —
                // and reverse only where the cursor is on the Archived heading.
                if span.text.chars().any(|c| "─╭╮╰╯│".contains(c)) {
                    let plain = Style {
                        reverse: false,
                        ..span.style
                    };
                    assert!(plain == DIM || plain == AMBER, "{:?}", span.text);
                }
            }
        }
        for build in [mockup_3, mockup_4, mockup_5] {
            for w in [40u16, 80] {
                let f = render(&build(w)).unwrap();
                // The box's first body line: header, rule, heading, row, top border, then it.
                let question: Vec<&Span> = f.lines[5]
                    .iter()
                    .filter(|s| s.text.trim() != "" && s.text != "│")
                    .collect();
                assert_eq!(question.len(), 1);
                assert_eq!(question[0].style, BOLD);
                assert!(!question[0].text.chars().any(|c| c.is_ascii_digit()));
                // Dialog keys are keys you can press, so they are blue too.
                let keys = chars_styled(&f, |s| s.fg == Some(Colour::Blue));
                assert!(!keys.is_empty() && keys.chars().all(|c| "ynrcEsc".contains(c)));
            }
        }
    }

    // ── The ANSI form ─────────────────────────────────────────────────────────────────

    fn strip(ansi: &str) -> String {
        let mut out = String::new();
        let mut chars = ansi.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// A terminal as xterm and Termux behave, as far as a frame needs: printing in the last
    /// column leaves the cursor there with a wrap pending, the next printable character wraps
    /// first, and an erase in line erases from the cursor — so from the last column, after a
    /// full-width line. pyte draws such a line whole, which is how 0.3.2 shipped with the
    /// last character of every full line erased.
    fn terminal(ansi: &str, w: usize, h: usize) -> Vec<String> {
        let mut screen = vec![vec![' '; w]; h];
        let (mut row, mut col, mut pending) = (0usize, 0usize, false);
        let mut chars = ansi.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => {
                    assert_eq!(chars.next(), Some('['));
                    let mut params = String::new();
                    let fin = loop {
                        let c = chars.next().unwrap();
                        if c.is_ascii_alphabetic() {
                            break c;
                        }
                        params.push(c);
                    };
                    match (fin, params.as_str()) {
                        ('H', "") => (row, col, pending) = (0, 0, false),
                        ('K', "" | "0") => screen[row][col..].fill(' '),
                        ('K', "2") => screen[row].fill(' '),
                        ('m', _) => {}
                        other => panic!("the model does not know {other:?}"),
                    }
                }
                '\r' => (col, pending) = (0, false),
                '\n' => (row, pending) = (row + 1, false),
                c => {
                    if pending {
                        (row, col, pending) = (row + 1, 0, false);
                    }
                    screen[row][col] = c;
                    if col + 1 == w {
                        pending = true;
                    } else {
                        col += 1;
                    }
                }
            }
        }
        screen
            .into_iter()
            .map(|l| l.into_iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// How 0.3.2 wrote a frame: each line, then the erase. For calibration only.
    fn ansi_erase_after(f: &Frame) -> String {
        f.ansi()
            .replace(CLEAR_LINE, "")
            .replace("\r\n", "\x1b[K\r\n")
            + "\x1b[K"
    }

    #[test]
    fn a_full_width_line_keeps_its_last_column_on_a_real_terminal() {
        for (v, f) in all_mockup_frames() {
            let (w, h) = (v.width as usize, v.height as usize);
            let got = terminal(&f.ansi(), w, h);
            assert_eq!(got.join("\n"), f.plain(), "at {w}");
        }
        // Calibration: the old order loses `now` to `no` and `14m` to `14`, exactly as the
        // owner saw — so the model reproduces the bug, and the check above can fail.
        let f = render(&View {
            cursor: 99,
            ..mockup_1(40)
        })
        .unwrap();
        let old = terminal(&ansi_erase_after(&f), 40, 24);
        assert!(old[6].ends_with(" no"), "{:?}", old[6]);
        assert!(old[9].ends_with(" 14"), "{:?}", old[9]);
        assert_ne!(old.join("\n"), f.plain());
    }

    #[test]
    fn each_line_is_cleared_before_it_is_drawn_and_never_after() {
        for (v, f) in all_mockup_frames() {
            let a = f.ansi();
            assert!(a.starts_with("\x1b[H"));
            assert_eq!(a.matches("\r\n").count(), v.height as usize - 1);
            assert!(!a.ends_with('\n'), "no newline after the bottom line");
            for (i, line) in a["\x1b[H".len()..].split("\r\n").enumerate() {
                assert!(line.starts_with(CLEAR_LINE), "line {i}: {line:?}");
                let rest = &line[CLEAR_LINE.len()..];
                assert!(!rest.contains("K"), "an erase after content on line {i}");
            }
        }
    }

    #[test]
    fn ansi_is_a_full_redraw_that_reads_back_as_the_plain_text() {
        for (v, f) in all_mockup_frames() {
            let a = f.ansi();
            let back: Vec<String> = strip(&a)
                .split("\r\n")
                .map(|l| l.trim_end_matches(' ').to_string())
                .collect();
            assert_eq!(back.join("\n"), f.plain());
            // Every style that is set is reset before the next span: of all the sequences,
            // one is the home and one per line the clear; the rest pair off.
            let resets = a.matches("\x1b[0m").count();
            let all = a.matches("\x1b[").count();
            assert_eq!(all - 1 - v.height as usize, 2 * resets);
        }
        let f = render(&View {
            cursor: 99,
            ..mockup_1(40)
        })
        .unwrap()
        .ansi();
        assert!(f.contains("\x1b[1;38;5;214mNeeds you · 1\x1b[0m"));
        assert!(f.contains("\x1b[38;5;75mEnter\x1b[0m open"));
        assert!(f.contains("\x1b[1mretire the old tunnel\x1b[0m"));
        assert!(f.contains("\x1b[2mimmich upgrade"));
        let cursor = render(&View {
            cursor: RETIRE,
            ..mockup_1(40)
        })
        .unwrap()
        .ansi();
        assert!(cursor.contains("\x1b[1;7mretire the old tunnel\x1b[0m"));
        // The blank lines between the list and the footer cost a clear and a separator each.
        let keys = render(&mockup_7(40)).unwrap().ansi();
        assert!(keys.contains("\x1b[2K\r\n\x1b[2K\r\n\x1b[2K"));
        assert!(keys.ends_with(" shell"), "the hint line is the bottom line");
    }

    // ── The parts the mockups do not draw ─────────────────────────────────────────────

    fn box_text(f: &Frame) -> Vec<String> {
        lines(f)
            .into_iter()
            .filter(|l| l.contains('│'))
            .map(|l| l.trim().trim_matches('│').trim_end().to_string())
            .collect()
    }

    #[test]
    fn closing_an_offloaded_session_says_it_is_only_hidden() {
        let v = View {
            cursor: MOUNT,
            about: Some(MOUNT),
            dialog: Some(Dialog::Close {
                title: "mount guards on one".to_string(),
                running: false,
            }),
            ..mockup_1(40)
        };
        let f = render(&v).unwrap();
        check_shape(&f, &v).unwrap();
        assert_eq!(
            box_text(&f),
            [
                " Close this session?",
                "   mount guards on one",
                "",
                " Offloaded. Hides the row, not the",
                " conversation — claude --resume still",
                " finds it.",
                "",
                " y close    n keep",
            ]
        );
    }

    #[test]
    fn no_room_with_nothing_to_offload_can_only_go_back() {
        let v = View {
            dialog: Some(Dialog::NoRoom {
                used: 1000 * MIB,
                limit: 1024 * MIB,
                want: 250 * MIB,
                offer: None,
            }),
            ..mockup_4(40)
        };
        let f = render(&v).unwrap();
        assert_eq!(
            box_text(&f),
            [
                " No room for another claude",
                "",
                " 1000M of 1.0G used. A new one wants",
                " about 250M.",
                "",
                " Nothing can be offloaded right now.",
                " Close a session to make room.",
                "",
                " Esc back",
            ]
        );
    }

    #[test]
    fn a_resume_that_says_nothing_says_so_and_a_closed_one_is_left_closed() {
        let v = View {
            dialog: Some(Dialog::ResumeFailed {
                title: "fix the dns records".to_string(),
                session: "5b2d0c77".to_string(),
                killed_for_memory: false,
                output: vec![],
                closed: true,
            }),
            about: Some(6),
            ..mockup_1(40)
        };
        let f = render(&v).unwrap();
        assert!(lines(&f)[2].starts_with("── Closed · 7 ─"));
        assert_eq!(
            box_text(&f),
            [
                " This session did not resume",
                "   fix the dns records",
                "",
                " claude --resume 5b2d0c77 ended",
                "   and wrote nothing to stderr",
                "",
                " Left closed. Nothing was deleted;",
                " the transcript may be gone.",
                "",
                " r retry   c close it   Esc back",
            ]
        );
    }

    #[test]
    fn long_stderr_keeps_its_last_lines_and_the_keys_in_a_short_terminal() {
        let output: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let v = View {
            height: 16,
            dialog: Some(Dialog::ResumeFailed {
                title: "mount guards on one".to_string(),
                session: "0f9c4a1e".to_string(),
                killed_for_memory: false,
                output,
                closed: false,
            }),
            ..mockup_5(40)
        };
        let f = render(&v).unwrap();
        check_shape(&f, &v).unwrap();
        let p = lines(&f);
        assert!(p[15].starts_with('╰'), "the box closes on the last line");
        assert!(p[14].contains("r retry   c close it   Esc back"));
        assert!(p.iter().any(|l| l.contains("  line 40")));
        assert!(!p.iter().any(|l| l.contains("line 1 ")));
        assert_eq!(p[2].chars().next(), Some('╭'), "the context goes first");
    }

    #[test]
    fn a_long_title_is_cut_to_its_field_with_no_ellipsis() {
        // No spaces, so a field one column off cannot pass by trimming luck.
        let long = "0123456789".repeat(10);
        for (w, ind, field) in [(40u16, 0, 35), (80, 2, 73)] {
            let f = render(&view(w, vec![row("", &long, "3m")], 812)).unwrap();
            let want = format!("{:ind$}{}{:>5}", "", &long[..field], "3m");
            assert_eq!(lines(&f)[3], want, "at {w} columns");
        }
    }

    #[test]
    fn control_characters_from_outside_never_reach_the_terminal() {
        let mut v = view(40, vec![row("", "evil\x1b[2Jtitle\nnext", "1m")], 812);
        v.header.host = "host\x07".to_string();
        v.status = Some("bad\rstatus".to_string());
        let f = render(&v).unwrap();
        // The frame's own clears are `ESC[2K`; a title's `ESC[2J` must not survive.
        assert!(!f.ansi().contains("\x1b[2J"));
        for span in f.lines.iter().flatten() {
            assert!(!span.text.chars().any(char::is_control), "{:?}", span.text);
        }
        let cleaned = format!("{:<35}{:>5}", "evil [2Jtitle next", "1m");
        assert_eq!(lines(&f)[3], cleaned);
    }

    #[test]
    fn no_memory_ceiling_drops_that_part_of_the_header() {
        let mut v = mockup_1(40);
        v.header.memory = None;
        assert_eq!(lines(&render(&v).unwrap())[0], "infra-dev · 6 open");
        let mut v = mockup_2(40);
        v.header.memory = None;
        assert_eq!(lines(&render(&v).unwrap())[0], "infra-dev · nothing open");
    }

    #[test]
    fn every_frame_is_height_lines_none_wider_than_width() {
        let mut many = listed();
        for _ in 0..4 {
            many.extend(listed());
        }
        let long_ws = View {
            workspace: "/a/workspace/path/long/enough/to/wrap/at/forty".to_string(),
            ..mockup_7(40)
        };
        let builds: Vec<Box<dyn Fn(u16) -> View>> = vec![
            Box::new(mockup_1),
            Box::new(mockup_2),
            Box::new(mockup_3),
            Box::new(mockup_4),
            Box::new(mockup_5),
            Box::new(mockup_6),
            Box::new(mockup_7),
            Box::new(move |w| View {
                cursor: 17,
                scroll: 10,
                ..view(w, many.clone(), 812)
            }),
            Box::new(move |w| View {
                width: w,
                ..long_ws.clone()
            }),
            Box::new(|w| View {
                dialog: Some(Dialog::NoRoom {
                    used: 0,
                    limit: 0,
                    want: 0,
                    offer: None,
                }),
                ..mockup_2(w)
            }),
        ];
        for build in &builds {
            for w in 7..=120 {
                for h in 0..=26 {
                    let v = View {
                        height: h,
                        ..build(w)
                    };
                    let f = render(&v).unwrap();
                    if let Err(e) = check_shape(&f, &v) {
                        panic!("{e} at {w}x{h}");
                    }
                }
            }
        }
    }
}
