//! The menu's renderer: a [`View`] in, a [`Frame`] out, and nothing else — no terminal, no
//! clock, no registry. That is what lets it be tested against the approved screens.
//!
//! **The screens in `docs/mockups.md` are binding** (owner, 2026-10-01), character for
//! character, at 40 and at 80 columns; the tests at the bottom of this file read the fenced
//! blocks out of both mockup files rather than keeping copies, so a change to the approved
//! screens breaks the build until the renderer follows it. Every layout number below is
//! measured off those screens, and each says which one.
//!
//! Every glyph drawn here is ASCII, box drawing, `·` or `—` — the set the mockups use, which
//! was checked against the common monospace fonts on 2026-10-01 — so one `char` is one
//! column, and widths are counted in chars. Text that comes from outside (titles, the host,
//! the workspace, claude's stderr) has its control characters replaced before it is counted:
//! a title is the owner's first prompt, and an escape sequence in it must not reach the
//! terminal as one.

use crate::fmt::human;
use crate::ui::{Colour, Dialog, Frame, Row, Screen, Span, Style, TooNarrow, View};

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
/// Spent on `!` and nothing else: the colour table in `docs/mockups.md`.
const AMBER: Style = Style {
    fg: Some(Colour::Amber),
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

/// The marks field: three columns, ASCII only, so the age right-aligned after it never moves
/// (`docs/mockups.md`, on why the marks column stays single-byte).
const MARKS_WIDTH: usize = 3;
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
        (Some(dialog), _) => dialog_screen(view, dialog, w, h),
        (None, Screen::Keys) => keys_screen(view, w),
        (None, Screen::List) => list_screen(view, w, h),
    };
    // Content flows from the top and the rest of the screen is blank — the mockups are
    // shorter than 24 lines and none of them pads between the list and the hint line.
    lines.truncate(h);
    lines.resize_with(h, Line::default);
    Ok(Frame {
        lines: lines.into_iter().map(|l| l.cut(w)).collect(),
    })
}

impl Frame {
    /// The text alone: styles dropped, trailing spaces trimmed, lines joined with `\n`. What
    /// a pipe or a screen reader would get, and what the tests compare against the mockups.
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

    /// A full redraw: home the cursor, then each line with its styling and a clear to the end
    /// of the line, so whatever the last frame left there goes. No newline after the last
    /// line — on the bottom row it would scroll the screen.
    ///
    /// Trailing blanks are not sent unless they are reverse video: `ESC[K` clears them anyway,
    /// and on a metered link they are bytes that draw nothing.
    pub fn ansi(&self) -> String {
        let mut out = String::from("\x1b[H");
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            let ink = line
                .iter()
                .rposition(|s| s.style.reverse || s.text.chars().any(|c| c != ' '));
            if let Some(last) = ink {
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
            }
            out.push_str("\x1b[K");
        }
        out
    }
}

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
    let mut rest = match view.rows.len() {
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

/// `count` rows of the list from `from`, numbered as the owner sees them.
fn rows(view: &View, w: usize, from: usize, count: usize) -> Vec<Line> {
    let digits = view.rows.len().to_string().len();
    // Number, space, marks, space, title, age: the title takes whatever the rest leaves —
    // 29 at 40 columns and 69 at 80 with single-digit numbering, as mockups 1 and 6 draw.
    let title_w = w.saturating_sub(digits + 1 + MARKS_WIDTH + 1 + AGE_WIDTH);
    view.rows
        .iter()
        .enumerate()
        .skip(from)
        .take(count)
        .map(|(i, r)| row(i + 1, r, digits, title_w, i == view.cursor))
        .collect()
}

fn row(n: usize, r: &Row, digits: usize, title_w: usize, cursor: bool) -> Line {
    // The cursor is reverse video across the whole row (owner, 2026-10-02): a monochrome
    // terminal shows it too, and the glyphs and colours inside it still read.
    let st = |s: Style| Style {
        reverse: cursor,
        ..s
    };
    let marks = [
        (r.wants_you, "!", AMBER),
        (r.unread, "*", BOLD),
        (r.timer, "t", BLUE),
        (r.attached, "@", DIM),
        (r.offloaded, "z", DIM),
        (r.unregistered, "u", DIM),
    ];
    let mut line = Line::default();
    line.push(&format!("{n:>digits$}"), st(DIM))
        .push(" ", st(FG));
    // Four marks can be true at once (`!*t@`) and the field is three wide; the field is the
    // one width the mockups make load-bearing, so the last in the approved order gives way.
    for (_, mark, style) in marks.iter().filter(|m| m.0).take(MARKS_WIDTH) {
        line.push(mark, st(*style));
    }
    line.pad(digits + 1 + MARKS_WIDTH + 1, st(FG));
    // Cut, with no ellipsis: `…` is outside the glyph set the fonts were measured for.
    let title: String = r.title.chars().take(title_w).collect();
    line.push(&title, st(FG))
        .pad(digits + 1 + MARKS_WIDTH + 1 + title_w, st(FG));
    let age: String = r.age.chars().take(AGE_WIDTH).collect();
    line.push(&format!("{age:>AGE_WIDTH$}"), st(DIM));
    line
}

// ── Screens ─────────────────────────────────────────────────────────────────────────────

/// Mockups 1, 2 and 6: header, rule, the rows that fit, rule, the status line if there is
/// one, the hint line.
fn list_screen(view: &View, w: usize, h: usize) -> Vec<Line> {
    let mut out = vec![header(view), rule(w)];
    let hints = if view.rows.is_empty() {
        out.extend(empty_body(view, w));
        hint_lines(&HINTS_EMPTY, GAP, w)
    } else {
        let tail = 1 + usize::from(view.status.is_some()) + hints_len(w);
        let room = h.saturating_sub(out.len() + tail);
        out.extend(rows(view, w, view.scroll, room));
        hint_lines(&HINTS, GAP, w)
    };
    out.push(rule(w));
    if let Some(status) = &view.status {
        out.push(text(status, FG));
    }
    out.extend(hints);
    out
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

/// Mockup 7, on `?`. Keys are blue, as everywhere a key can be pressed; each mark is drawn in
/// the colour it has in the list, so the screen is also the colour key.
fn keys_screen(view: &View, w: usize) -> Vec<Line> {
    let mut head = text(&view.header.host, FG);
    head.push(" · keys and marks", DIM);
    let mut out = vec![head, rule(w)];
    let ws = &view.workspace;
    let new = format!("new session in {ws}");
    let shell = format!("a shell in {ws}");
    let keys: [(&str, &str); 6] = [
        ("Enter", "open the row (resume if z)"),
        ("n", &new),
        ("c", "close the row"),
        ("s", &shell),
        ("Esc", "quit the launcher"),
        ("?", "this"),
    ];
    for (key, what) in keys {
        keyed(&mut out, 0, (key, BLUE), 7, &[what], w);
    }
    out.push(blank());
    // The `t` line breaks after "not" at 80 columns as well as at 40: the approved screens
    // break it there, so it is two segments rather than one wrapped sentence.
    let marks: [(&str, Style, &[&str]); 6] = [
        ("!", AMBER, &["wants you: a prompt is waiting"]),
        ("*", BOLD, &["unread: it finished while away"]),
        (
            "t",
            BLUE,
            &["a timer is pending; not", "offloaded until it fires"],
        ),
        ("@", DIM, &["attached somewhere else too"]),
        ("z", DIM, &["offloaded: Enter resumes it"]),
        ("u", DIM, &["not started by claude-sessions"]),
    ];
    for (mark, style, what) in marks {
        keyed(&mut out, 0, (mark, style), 3, what, w);
    }
    out.push(rule(w));
    let hints: &[Hint] = if view.rows.is_empty() {
        &HINTS_EMPTY
    } else {
        &HINTS
    };
    out.extend(hint_lines(hints, GAP, w));
    out
}

/// Mockups 3 to 5: header, rule, two rows from `scroll` for context, the box, and nothing
/// after it — no closing rule and no hint line, because the box carries its own keys.
fn dialog_screen(view: &View, dialog: &Dialog, w: usize, h: usize) -> Vec<Line> {
    let box_w = w.min(DIALOG_WIDTH);
    // Header and rule above, the box's own two borders around the body.
    let room = h.saturating_sub(4);
    let body = dialog_body(dialog, box_w.saturating_sub(4), room);
    // In a short terminal the context rows go first; the question never does.
    let context = 2
        .min(view.rows.len().saturating_sub(view.scroll))
        .min(room.saturating_sub(body.len()));
    let mut out = vec![header(view), rule(w)];
    out.extend(rows(view, w, view.scroll, context));
    out.extend(boxed(body, w, box_w));
    out
}

/// What a dialog says, wrapped to `tw` — 36 columns in a 40-column box, one space of padding
/// each side. The first line is the question, in bold: a question is not a warning, so it is
/// not amber. Indented lines (a title, claude's stderr) wrap with their indent.
fn dialog_body(dialog: &Dialog, tw: usize, room: usize) -> Vec<Line> {
    let mut out = Vec::new();
    match dialog {
        Dialog::Close {
            row,
            title,
            running,
        } => {
            paragraph(&mut out, &format!("Close slot {row}?"), 0, tw, BOLD);
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
                Some((row, age, title)) => {
                    let ask = format!("Offload slot {row}, idle {age}?");
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
                    let none = "Nothing can be offloaded right now. Close a slot to make room.";
                    paragraph(&mut out, none, 0, tw, FG);
                    out.push(blank());
                    out.extend(hint_lines(&[("Esc", "back")], GAP, tw));
                }
            }
        }
        Dialog::ResumeFailed {
            row,
            session,
            status,
            output,
        } => {
            paragraph(&mut out, &format!("Slot {row} did not resume"), 0, tw, BOLD);
            out.push(blank());
            let cmd = format!("claude --resume {session} exited {status}");
            paragraph(&mut out, &cmd, 0, tw, FG);
            let mut said = Vec::new();
            for l in output {
                paragraph(&mut said, l, 2, tw, FG);
            }
            let mut tail = vec![blank()];
            let left = "Left offloaded. Nothing was deleted; the transcript may be gone.";
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
    use crate::ui::{Header, RowKey};

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

    fn row(marks: &str, title: &str, age: &str) -> Row {
        Row {
            key: RowKey::Slot(title.to_string()),
            wants_you: marks.contains('!'),
            unread: marks.contains('*'),
            timer: marks.contains('t'),
            attached: marks.contains('@'),
            offloaded: marks.contains('z'),
            unregistered: marks.contains('u'),
            title: title.to_string(),
            age: age.to_string(),
        }
    }

    /// The six slots of mockup 1.
    fn six() -> Vec<Row> {
        vec![
            row("!", "permission: write hosts/one", "2m"),
            row("*", "retire the old tunnel", "14m"),
            row("*t", "loop: watch the base build", "31m"),
            row("@", "immich upgrade", "now"),
            row("z", "mount guards on one", "2d"),
            row("u", "claude", "5h"),
        ]
    }

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
            screen: Screen::List,
            dialog: None,
            status: None,
        }
    }

    // The View each mockup depicts, at a given width.

    fn mockup_1(w: u16) -> View {
        view(w, six(), 812)
    }
    fn mockup_2(w: u16) -> View {
        view(w, vec![], 812)
    }
    fn mockup_3(w: u16) -> View {
        View {
            cursor: 1,
            dialog: Some(Dialog::Close {
                row: 2,
                title: "retire the old tunnel".to_string(),
                running: true,
            }),
            ..view(w, six(), 812)
        }
    }
    fn mockup_4(w: u16) -> View {
        View {
            scroll: 4,
            dialog: Some(Dialog::NoRoom {
                used: 892 * MIB,
                limit: 1024 * MIB,
                want: 250 * MIB,
                offer: Some((5, "2d".to_string(), "mount guards on one".to_string())),
            }),
            ..view(w, six(), 892)
        }
    }
    fn mockup_5(w: u16) -> View {
        View {
            cursor: 4,
            scroll: 4,
            dialog: Some(Dialog::ResumeFailed {
                row: 5,
                session: "0f9c4a1e".to_string(),
                status: 1,
                output: vec!["No conversation found with that session id".to_string()],
            }),
            ..view(w, six(), 812)
        }
    }
    fn mockup_6(w: u16) -> View {
        let mut rows = six();
        rows[1] = row("", "retire the old tunnel", "now");
        rows[3].age = "12m".to_string();
        View {
            cursor: 1,
            status: Some("detached from 2 · it is still running".to_string()),
            ..view(w, rows, 1024)
        }
    }
    fn mockup_7(w: u16) -> View {
        View {
            screen: Screen::Keys,
            ..view(w, six(), 812)
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

    /// The comparison every mockup test makes: the screen's lines first, then blank to the
    /// bottom. Returns rather than asserts so the calibration test can watch it fail.
    fn matches(v: &View, expected: &[&str]) -> Result<(), String> {
        let frame = render(v).map_err(|e| format!("{e:?}"))?;
        check_shape(&frame, v)?;
        let got = lines(&frame);
        if got.len() < expected.len() {
            return Err("the frame is shorter than the mockup".to_string());
        }
        for (i, (g, e)) in got.iter().zip(expected).enumerate() {
            if g != e {
                return Err(format!("line {i}:\n  got  {g:?}\n  want {e:?}"));
            }
        }
        if let Some(i) = got[expected.len()..].iter().position(|l| !l.is_empty()) {
            return Err(format!("line {} should be blank", expected.len() + i));
        }
        Ok(())
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
    fn mockup_1_the_list_every_mark_mixed() {
        assert_mockup(1, mockup_1);
    }

    #[test]
    fn mockup_2_nothing_open() {
        assert_mockup(2, mockup_2);
    }

    #[test]
    fn mockup_3_closing_a_live_slot() {
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
    fn mockup_6_back_from_a_slot_with_a_status_line() {
        assert_mockup(6, mockup_6);
    }

    #[test]
    fn mockup_7_the_keys() {
        assert_mockup(7, mockup_7);
    }

    /// Mockup 8's blocks: `(width, hint lines)` for each `at N columns` it shows.
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

    // ── Calibration: the comparison must be able to fail ──────────────────────────────

    #[test]
    fn the_mockup_comparison_fails_on_a_wrong_view() {
        // The parse finds real screens, not empty ones that would match anything.
        assert_eq!(mockup(MOCKUPS_40, 1).len(), 11);
        assert_eq!(mockup(MOCKUPS_80, 7).len(), 18);
        assert!(mockup(MOCKUPS_40, 5)[0].starts_with("infra-dev · 6 open"));

        let wrong_title = {
            let mut v = mockup_1(40);
            v.rows[1].title = "retire the new tunnel".to_string();
            v
        };
        let wrong_age = {
            let mut v = mockup_1(40);
            v.rows[0].age = "3m".to_string();
            v
        };
        let wrong_mark = {
            let mut v = mockup_1(40);
            v.rows[2].timer = false;
            v
        };
        let wrong_memory = view(40, six(), 813);
        let wrong_status = View {
            status: None,
            ..mockup_6(40)
        };
        let wrong_scroll = View {
            scroll: 3,
            ..mockup_4(40)
        };
        let wrong_dialog = View {
            dialog: Some(Dialog::Close {
                row: 2,
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
            (1, wrong_mark),
            (1, wrong_memory),
            (6, wrong_status),
            (4, wrong_scroll),
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
        let mut expected = mockup(doc, 1);
        expected.pop();
        assert!(matches(&mockup_1(40), &expected).is_err());
    }

    // ── Colour, against the table in docs/mockups.md ──────────────────────────────────

    fn all_mockup_frames() -> Vec<(View, Frame)> {
        let mut out = Vec::new();
        for (_, w) in SETS {
            for build in [
                mockup_1, mockup_2, mockup_3, mockup_4, mockup_5, mockup_6, mockup_7,
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

    #[test]
    fn amber_is_spent_on_the_wants_you_mark_and_nothing_else() {
        for (v, f) in all_mockup_frames() {
            let amber = chars_styled(&f, |s| s.fg == Some(Colour::Amber));
            assert!(
                amber.chars().all(|c| c == '!'),
                "amber on {amber:?} at {}",
                v.width
            );
        }
        // Mockup 1 at 40: one slot wants you, so one amber character on the whole screen.
        // (The colour table's prose says "two amber characters" in a full list of six; the
        // approved screen has one `!`, and the screen is what is binding.)
        let f = render(&mockup_1(40)).unwrap();
        assert_eq!(chars_styled(&f, |s| s.fg == Some(Colour::Amber)), "!");
        let a = f.ansi();
        assert_eq!(a.matches("38;5;214").count(), 1);
    }

    #[test]
    fn keys_and_the_timer_mark_are_blue() {
        let f = render(&mockup_1(40)).unwrap();
        // Row 3's `t`, then the hint line's keys in order.
        assert_eq!(
            chars_styled(&f, |s| s.fg == Some(Colour::Blue)),
            "tEnternc?s"
        );
        // The hint line: keys blue, descriptions in the foreground.
        let hint = &f.lines[9];
        let blue: Vec<&str> = hint
            .iter()
            .filter(|s| s.style == BLUE)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(blue, ["Enter", "n", "c", "?"]);
        assert!(
            hint.iter()
                .filter(|s| s.text.contains("open"))
                .all(|s| s.style == FG)
        );
        // The keys screen: every key blue and each mark in its list colour.
        let k = render(&mockup_7(40)).unwrap();
        let first = |i: usize| k.lines[i][0].clone();
        for i in 2..8 {
            assert_eq!(first(i).style, BLUE, "key on line {i}");
        }
        assert_eq!(first(9).style, AMBER);
        assert_eq!(first(10).style, BOLD);
        assert_eq!(first(11).style, BLUE);
        for i in [13, 14, 15] {
            assert_eq!(first(i).style, DIM, "mark on line {i}");
        }
    }

    #[test]
    fn marks_numbers_and_ages_take_their_table_styles() {
        let f = render(&View {
            cursor: 99,
            ..mockup_1(40)
        })
        .unwrap();
        let style_of = |line: usize, col: usize| {
            let mut at = 0;
            for s in &f.lines[line] {
                let n = s.text.chars().count();
                if col < at + n {
                    return s.style;
                }
                at += n;
            }
            panic!("no column {col} on line {line}");
        };
        assert_eq!(style_of(2, 0), DIM, "row number");
        assert_eq!(style_of(2, 2), AMBER, "!");
        assert_eq!(style_of(3, 2), BOLD, "*");
        assert_eq!(style_of(4, 3), BLUE, "t");
        assert_eq!(style_of(5, 2), DIM, "@");
        assert_eq!(style_of(6, 2), DIM, "z");
        assert_eq!(style_of(7, 2), DIM, "u");
        assert_eq!(style_of(2, 6), FG, "title");
        assert_eq!(style_of(2, 39), DIM, "age");
        // The header: the container's name, then everything after it dim.
        assert_eq!(f.lines[0][0].text, "infra-dev");
        assert_eq!(f.lines[0][0].style, FG);
        assert_eq!(f.lines[0][1].style, DIM);
        assert_eq!(f.lines[0].len(), 2);
    }

    #[test]
    fn the_cursor_row_is_reverse_across_the_full_width_and_nowhere_else() {
        for w in [40u16, 80] {
            for cursor in 0..6 {
                let f = render(&View {
                    cursor,
                    ..mockup_1(w)
                })
                .unwrap();
                for (i, line) in f.lines.iter().enumerate() {
                    let rev = line.iter().filter(|s| s.style.reverse).count();
                    if i == 2 + cursor {
                        assert_eq!(rev, line.len(), "the whole cursor row is reverse");
                        let n: usize = line.iter().map(|s| s.text.chars().count()).sum();
                        assert_eq!(n, w as usize, "the cursor row spans the width");
                    } else {
                        assert_eq!(rev, 0, "line {i} is not the cursor");
                    }
                }
            }
        }
        // Scrolled, the cursor is still the row it names.
        let mut rows = six();
        rows.extend(six());
        let f = render(&View {
            height: 8,
            scroll: 3,
            cursor: 4,
            ..view(40, rows, 812)
        })
        .unwrap();
        let p = lines(&f);
        assert!(p[2].starts_with(" 4 @"), "{:?}", p[2]);
        assert!(f.lines[3].iter().all(|s| s.style.reverse));
        assert!(p[3].starts_with(" 5 z"));
    }

    #[test]
    fn rules_and_dialog_frames_are_dim_and_a_dialog_asks_in_bold() {
        for (_, f) in all_mockup_frames() {
            for span in f.lines.iter().flatten() {
                if span.text.chars().any(|c| "─╭╮╰╯│".contains(c)) {
                    assert_eq!(span.style, DIM, "{:?}", span.text);
                }
            }
        }
        for build in [mockup_3, mockup_4, mockup_5] {
            for w in [40u16, 80] {
                let f = render(&build(w)).unwrap();
                // The box's first body line is header, rule, two rows, top border on.
                let question: Vec<&Span> = f.lines[5]
                    .iter()
                    .filter(|s| s.text.trim() != "" && s.text != "│")
                    .collect();
                assert_eq!(question.len(), 1);
                assert_eq!(question[0].style, BOLD);
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

    #[test]
    fn ansi_is_a_full_redraw_that_reads_back_as_the_plain_text() {
        for (v, f) in all_mockup_frames() {
            let a = f.ansi();
            assert!(a.starts_with("\x1b[H"));
            assert_eq!(a.matches("\r\n").count(), v.height as usize - 1);
            assert!(!a.ends_with('\n'), "no newline after the bottom line");
            for line in a.split("\r\n") {
                assert!(line.ends_with("\x1b[K"), "{line:?}");
            }
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
        assert!(f.contains("\x1b[38;5;214m!\x1b[0m"));
        assert!(f.contains("\x1b[38;5;75mEnter\x1b[0m open"));
        assert!(f.contains("\x1b[1m*\x1b[0m"));
        assert!(f.contains("\x1b[2m1\x1b[0m"));
        let cursor = render(&View {
            cursor: 1,
            ..mockup_1(40)
        })
        .unwrap()
        .ansi();
        assert!(cursor.contains("\x1b[2;7m2\x1b[0m"));
        assert!(cursor.contains("\x1b[1;7m*\x1b[0m"));
        // An idle screen's blank lines cost three bytes and a separator each.
        assert!(f.ends_with("\x1b[K\r\n\x1b[K"));
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
    fn closing_an_offloaded_slot_says_it_is_only_hidden() {
        let v = View {
            dialog: Some(Dialog::Close {
                row: 5,
                title: "mount guards on one".to_string(),
                running: false,
            }),
            ..mockup_4(40)
        };
        let f = render(&v).unwrap();
        check_shape(&f, &v).unwrap();
        assert_eq!(
            box_text(&f),
            [
                " Close slot 5?",
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
                " Close a slot to make room.",
                "",
                " Esc back",
            ]
        );
    }

    #[test]
    fn long_stderr_keeps_its_last_lines_and_the_keys_in_a_short_terminal() {
        let output: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let v = View {
            height: 16,
            dialog: Some(Dialog::ResumeFailed {
                row: 5,
                session: "0f9c4a1e".to_string(),
                status: 1,
                output,
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
        assert!(
            !p.iter().any(|l| l.contains("mount guards")),
            "rows go first"
        );
    }

    #[test]
    fn ten_rows_or_more_widen_the_number_and_narrow_the_title() {
        let mut rows = six();
        rows.extend(six());
        let f = render(&view(40, rows, 812)).unwrap();
        let p = lines(&f);
        assert_eq!(p[2], " 1 !   permission: write hosts/one    2m");
        assert_eq!(p[13], "12 u   claude                         5h");
        assert_eq!(p[0], "infra-dev · 12 open · 812M of 1.0G");
    }

    #[test]
    fn a_long_title_is_cut_to_its_field_with_no_ellipsis() {
        // No spaces, so a field one column off cannot pass by trimming luck.
        let long = "0123456789".repeat(10);
        for (w, field) in [(40u16, 29), (80, 69)] {
            let f = render(&view(w, vec![row("", &long, "3m")], 812)).unwrap();
            let want = format!("1     {}{:>5}", &long[..field], "3m");
            assert_eq!(lines(&f)[2], want, "at {w} columns");
        }
    }

    #[test]
    fn the_list_scrolls_to_what_fits_above_the_rule_status_and_hints() {
        let mut rows = six();
        rows.extend(six());
        rows.extend(six());
        let v = View {
            height: 12,
            scroll: 2,
            status: Some("2 ended".to_string()),
            ..view(40, rows, 812)
        };
        let f = render(&v).unwrap();
        let p = lines(&f);
        // header, rule, six rows, rule, status, two hint lines: twelve exactly.
        assert!(p[2].starts_with(" 3 *t"));
        assert!(p[7].starts_with(" 8 *"));
        assert!(p[8].starts_with('─'));
        assert_eq!(p[9], "2 ended");
        assert_eq!(p[11], "s shell");
    }

    #[test]
    fn four_marks_keep_the_first_three_and_the_age_does_not_move() {
        let f = render(&view(40, vec![row("!*t@", "busy", "1m")], 812)).unwrap();
        assert_eq!(lines(&f)[2], format!("1 !*t {:<29}{:>5}", "busy", "1m"));
    }

    #[test]
    fn control_characters_from_outside_never_reach_the_terminal() {
        let mut v = view(40, vec![row("", "evil\x1b[2Jtitle\nnext", "1m")], 812);
        v.header.host = "host\x07".to_string();
        v.status = Some("bad\rstatus".to_string());
        let f = render(&v).unwrap();
        assert!(!f.ansi().contains("\x1b[2J"));
        for span in f.lines.iter().flatten() {
            assert!(!span.text.chars().any(char::is_control), "{:?}", span.text);
        }
        let cleaned = format!("1     {:<29}{:>5}", "evil [2Jtitle next", "1m");
        assert_eq!(lines(&f)[2], cleaned);
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
        let mut many = six();
        for _ in 0..4 {
            many.extend(six());
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
