//! The phone client's conversation: what the relay's ops build, and how it is drawn — mockups
//! R1 to R8, which copy Claude Code 2.1.311's fullscreen renderer (`docs/mockups.md` § *The
//! remote view*).
//!
//! **Pure.** Ops in (`Convo::apply`), keys turned into wishes by the client (`client.rs`), and
//! a frame out (`draw`), with the clock passed in. So every screen is tested as text against
//! its mockup.
//!
//! Colours are Claude Code's own, read from its dark theme (the table in `docs/mockups.md`),
//! written as 24-bit colour; nothing is said by colour alone.

use crate::json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const ORANGE: u32 = 0xd7_7757;
pub const LAVENDER: u32 = 0xb1_b9f9;
pub const GREY: u32 = 0x99_9999;
pub const RULE: u32 = 0x88_8888;
pub const BAND: u32 = 0x37_3737;
pub const GREEN: u32 = 0x4e_ba65;
pub const YELLOW: u32 = 0xff_c107;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct St {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
}

const PLAIN: St = St {
    fg: None,
    bg: None,
    bold: false,
};

fn fg(c: u32) -> St {
    St {
        fg: Some(c),
        ..PLAIN
    }
}

fn bold(c: u32) -> St {
    St {
        fg: Some(c),
        bold: true,
        ..PLAIN
    }
}

/// One row of the screen: runs of text, each in one style.
pub type Line = Vec<(String, St)>;

fn line(text: impl Into<String>, st: St) -> Line {
    vec![(text.into(), st)]
}

pub fn plain(l: &Line) -> String {
    l.iter().map(|(t, _)| t.as_str()).collect()
}

// ── width and wrapping ──────────────────────────────────────────────────────

/// Cells a character takes: two for the wide East Asian and emoji ranges, none for a
/// combining mark, one otherwise. Claude Code's own glyphs are all one.
pub fn cw(c: char) -> usize {
    let u = c as u32;
    if (0x300..0x370).contains(&u) || (0x200b..0x2010).contains(&u) || u == 0xfe0f {
        return 0;
    }
    let wide = (0x1100..=0x115f).contains(&u)
        || (0x2e80..=0xa4cf).contains(&u)
        || (0xac00..=0xd7a3).contains(&u)
        || (0xf900..=0xfaff).contains(&u)
        || (0xfe30..=0xfe4f).contains(&u)
        || (0xff00..=0xff60).contains(&u)
        || (0xffe0..=0xffe6).contains(&u)
        || (0x1f300..=0x1f64f).contains(&u)
        || (0x1f900..=0x1f9ff).contains(&u);
    if wide { 2 } else { 1 }
}

pub fn width(s: &str) -> usize {
    s.chars().map(cw).sum()
}

/// Cut to `w` cells, with `…` in the last when anything was cut.
pub fn cut(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        if used + cw(c) + 1 > w {
            break;
        }
        out.push(c);
        used += cw(c);
    }
    out.push('…');
    out
}

/// Greedy word wrap of one paragraph to `w` cells; a word longer than a line is broken.
/// Spaces inside are kept as typed, but a break eats the space it falls on.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for word in s.split(' ') {
        let ww = width(word);
        if cur_w > 0 && cur_w + 1 + ww <= w {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
            continue;
        }
        if cur_w > 0 || !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        // A word wider than the line: broken at the width.
        let mut rest = word;
        while width(rest) > w {
            let mut taken = 0;
            let mut at = 0;
            for (i, c) in rest.char_indices() {
                if taken + cw(c) > w {
                    break;
                }
                taken += cw(c);
                at = i + c.len_utf8();
            }
            lines.push(rest[..at].to_string());
            rest = &rest[at..];
        }
        cur.push_str(rest);
        cur_w = width(rest);
    }
    lines.push(cur);
    lines
}

/// Every paragraph of `text` wrapped to `w`, the first line after `first` and the rest after
/// `rest`, both the same width. Claude Code keeps the last two columns clear of a message's
/// text (R1's `fail.`, R4's `days`); a tool's `⎿` line, one (R2's `-D`): see [`wrapped_by`].
fn wrapped(text: &str, first: &str, rest: &str, w: usize, st: St) -> Vec<Line> {
    wrapped_by(text, first, rest, w.saturating_sub(2), st)
}

fn wrapped_by(text: &str, first: &str, rest: &str, w: usize, st: St) -> Vec<Line> {
    let room = w.saturating_sub(width(first)).max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        for piece in wrap(para.trim_end(), room) {
            let lead = if out.is_empty() { first } else { rest };
            out.push(line(format!("{lead}{piece}"), st));
        }
    }
    if out.is_empty() {
        out.push(line(first.to_string(), st));
    }
    out
}

/// Claude Code's hint lines: whole segments joined by ` · `, a line ending ` ·` where it
/// breaks.
fn hints(segs: &[&str], indent: &str, w: usize) -> Vec<Line> {
    let room = w.saturating_sub(width(indent));
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for seg in segs {
        if cur.is_empty() {
            cur = seg.to_string();
        } else if width(&cur) + 3 + width(seg) <= room {
            cur = format!("{cur} · {seg}");
        } else {
            lines.push(format!("{cur} ·"));
            cur = seg.to_string();
        }
    }
    lines.push(cur);
    lines
        .into_iter()
        .map(|l| line(format!("{indent}{l}"), fg(GREY)))
        .collect()
}

// ── the conversation ────────────────────────────────────────────────────────

/// The tool a question is, drawn as its answers rather than as a call.
const ASK_TOOL: &str = "AskUserQuestion";

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub multi: bool,
    /// Label, description, preview.
    pub options: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Prompt(String),
    Text(String),
    Tool {
        name: String,
        arg: String,
        /// Whether it succeeded, its first lines, and how many lines in all.
        result: Option<(bool, Vec<String>, usize)>,
    },
    Perm {
        request_id: String,
        tool: String,
        arg: String,
        description: String,
        always: String,
        resolved: Option<String>,
    },
    Ask {
        request_id: String,
        questions: Vec<Question>,
        resolved: Option<String>,
        /// What was chosen, keyed by each question's text; empty from an older relay, which
        /// did not say.
        answers: Vec<(String, String)>,
    },
    Note(String),
    Compacted,
    /// `✻ Worked for 41s · done 1:12 AM`: the client's own, at the end of a turn it saw.
    Worked(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub session_id: String,
    pub model: String,
    pub mode: String,
    pub title: String,
    pub effort: String,
    pub cwd: String,
    pub version: String,
}

#[derive(Debug, Default)]
pub struct Convo {
    pub host: String,
    pub slot: String,
    pub run: String,
    /// Every op of the log's lines up to this has arrived (`n`).
    pub n: u64,
    pub entries: Vec<Entry>,
    /// Where each numbered item is in `entries`.
    index: HashMap<u64, usize>,
    pub state: String,
    pub meta: Meta,
    /// When the turn now running started, as this client saw it.
    pub working_since: Option<Instant>,
    /// Whether ops are arriving live: false until the first batch after opening, which is the
    /// log as it stood.
    live: bool,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// What an op came to, for the client to act on.
#[derive(Debug, PartialEq)]
pub enum Applied {
    Nothing,
    /// A turn ended that this client saw start: say how long it took.
    TurnEnded(Duration),
}

impl Convo {
    pub fn new(host: &str, slot: &str, run: &str) -> Convo {
        Convo {
            host: host.to_string(),
            slot: slot.to_string(),
            run: run.to_string(),
            state: "idle".to_string(),
            ..Convo::default()
        }
    }

    /// Something the client adds itself, such as the `❯ /model` it ran.
    pub fn local(&mut self, e: Entry) {
        self.entries.push(e);
    }

    fn at(&mut self, id: u64) -> Option<&mut Entry> {
        let i = *self.index.get(&id)?;
        self.entries.get_mut(i)
    }

    /// The prompt or question now waiting, if any: the newest unresolved one.
    pub fn waiting(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|e| {
            matches!(
                e,
                Entry::Perm { resolved: None, .. } | Entry::Ask { resolved: None, .. }
            )
        })
    }

    /// The last thing the assistant said, for `/copy`.
    pub fn last_text(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Text(t) => Some(t.as_str()),
            _ => None,
        })
    }

    pub fn apply(&mut self, v: &Value, now: Instant) -> Applied {
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        match s(v, "t").as_str() {
            "add" => {
                let e = match s(v, "k").as_str() {
                    "user" => Entry::Prompt(s(v, "x")),
                    "text" => Entry::Text(s(v, "x")),
                    "note" => Entry::Note(s(v, "x")),
                    "compact" => Entry::Compacted,
                    "tool" => Entry::Tool {
                        name: s(v, "n"),
                        arg: s(v, "a"),
                        result: None,
                    },
                    "perm" => Entry::Perm {
                        request_id: s(v, "r"),
                        tool: s(v, "n"),
                        arg: s(v, "a"),
                        description: s(v, "d"),
                        always: s(v, "al"),
                        resolved: None,
                    },
                    "ask" => Entry::Ask {
                        request_id: s(v, "r"),
                        questions: questions(v.get("q")),
                        resolved: None,
                        answers: Vec::new(),
                    },
                    // An item a later relay draws that this client does not know: skipped.
                    _ => return Applied::Nothing,
                };
                self.index.insert(id, self.entries.len());
                self.entries.push(e);
            }
            "app" => {
                if let Some(Entry::Text(t)) = self.at(id) {
                    t.push_str(&s(v, "x"));
                }
            }
            "set" => {
                if let Some(Entry::Text(t)) = self.at(id) {
                    *t = s(v, "x");
                }
            }
            "res" => {
                if let Some(Entry::Tool { result, .. }) = self.at(id) {
                    let preview = v
                        .get("p")
                        .and_then(Value::as_arr)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect();
                    let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(true);
                    let lines = v.get("l").and_then(Value::as_u64).unwrap_or(0) as usize;
                    *result = Some((ok, preview, lines));
                }
            }
            "done" => match self.at(id) {
                Some(Entry::Perm { resolved, .. }) => *resolved = Some(s(v, "x")),
                Some(Entry::Ask {
                    resolved, answers, ..
                }) => {
                    *resolved = Some(s(v, "x"));
                    if let Some(Value::Obj(m)) = v.get("a") {
                        *answers = m
                            .iter()
                            .filter_map(|(q, a)| a.as_str().map(|a| (q.clone(), a.to_string())))
                            .collect();
                    }
                }
                _ => {}
            },
            "st" => {
                let st = s(v, "s");
                let was = std::mem::replace(&mut self.state, st.clone());
                match st.as_str() {
                    "working" | "compacting" | "waiting" => {
                        if self.working_since.is_none() {
                            self.working_since = Some(now);
                        }
                    }
                    _ => {
                        let since = self.working_since.take();
                        if self.live && was != "idle" {
                            if let Some(t) = since {
                                return Applied::TurnEnded(now.saturating_duration_since(t));
                            }
                        }
                    }
                }
            }
            "meta" => {
                self.meta = Meta {
                    session_id: s(v, "sid"),
                    model: s(v, "m"),
                    mode: s(v, "pm"),
                    title: s(v, "ti"),
                    effort: s(v, "e"),
                    cwd: s(v, "cwd"),
                    version: s(v, "ver"),
                };
            }
            "n" => {
                self.n = v.get("n").and_then(Value::as_u64).unwrap_or(self.n);
                if !self.live {
                    // The log as it stood is in. A turn still running started before now,
                    // when exactly this client cannot say: it is timed from here.
                    self.live = true;
                    if self.working_since.is_some() {
                        self.working_since = Some(now);
                    }
                }
            }
            _ => {}
        }
        Applied::Nothing
    }
}

fn questions(v: Option<&Value>) -> Vec<Question> {
    v.and_then(Value::as_arr)
        .unwrap_or(&[])
        .iter()
        .map(|q| Question {
            question: s(q, "q"),
            header: s(q, "h"),
            multi: q.get("m").and_then(Value::as_bool).unwrap_or(false),
            options: q
                .get("o")
                .and_then(Value::as_arr)
                .unwrap_or(&[])
                .iter()
                .map(|o| {
                    let f = |i: usize| {
                        o.as_arr()
                            .and_then(|a| a.get(i))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    };
                    (f(0), f(1), f(2))
                })
                .collect(),
        })
        .collect()
}

// ── names as Claude Code says them ──────────────────────────────────────────

/// `claude-opus-5-5` → `Opus 5.5`; a date suffix and a context marker are dropped.
pub fn model_name(id: &str) -> String {
    let id = id.split('[').next().unwrap_or(id);
    let rest = id.strip_prefix("claude-").unwrap_or(id);
    let mut words: Vec<String> = Vec::new();
    let mut digits: Vec<&str> = Vec::new();
    for part in rest.split('-') {
        if part.len() >= 8 && part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
            digits.push(part);
        } else {
            let mut c = part.chars();
            if let Some(f) = c.next() {
                words.push(f.to_uppercase().chain(c).collect());
            }
        }
    }
    let mut out = words.join(" ");
    if !digits.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&digits.join("."));
    }
    if out.is_empty() { id.to_string() } else { out }
}

/// How a run of tool calls reads once collapsed: `Ran 2 shell commands, read 1 file`.
fn tools_summary(tools: &[(&str, &str)], running: bool) -> String {
    let mut counts: Vec<(&'static str, &'static str, &'static str, usize)> = Vec::new();
    for (name, _) in tools {
        let (done, doing, what) = match *name {
            "Bash" => ("Ran", "Running", "shell command"),
            "Read" => ("Read", "Reading", "file"),
            "Edit" | "MultiEdit" => ("Edited", "Editing", "file"),
            "Write" => ("Wrote", "Writing", "file"),
            "Grep" | "Glob" => ("Searched", "Searching", "pattern"),
            "WebFetch" => ("Fetched", "Fetching", "page"),
            "WebSearch" => ("Searched", "Searching", "web query"),
            "Task" | "Agent" => ("Ran", "Running", "agent"),
            _ => ("Used", "Using", "tool"),
        };
        match counts.iter_mut().find(|c| c.2 == what && c.0 == done) {
            Some(c) => c.3 += 1,
            None => counts.push((done, doing, what, 1)),
        }
    }
    let parts: Vec<String> = counts
        .iter()
        .enumerate()
        .map(|(i, (done, doing, what, n))| {
            let verb = if running { *doing } else { *done };
            let verb = if i == 0 {
                verb.to_string()
            } else {
                verb.to_lowercase()
            };
            let plural = if *n == 1 { "" } else { "s" };
            format!("{verb} {n} {what}{plural}")
        })
        .collect();
    let mut out = parts.join(", ");
    if running {
        out.push('…');
    }
    out
}

fn tool_line(name: &str, arg: &str) -> String {
    match name {
        "Bash" => format!("$ {arg}"),
        _ if arg.is_empty() => name.to_string(),
        _ => arg.to_string(),
    }
}

/// The permission dialog's title: Claude Code's for the common tools.
fn perm_title(tool: &str) -> String {
    match tool {
        "Bash" => "Bash command".into(),
        "Edit" | "MultiEdit" => "Edit file".into(),
        "Write" => "Create file".into(),
        "Read" => "Read file".into(),
        "WebFetch" => "Fetch".into(),
        other => other.to_string(),
    }
}

pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Claude Code's effort glyphs (read from 2.1.311).
pub fn effort_glyph(level: &str) -> &'static str {
    match level {
        "low" => "○",
        "medium" => "◐",
        "high" => "●",
        _ => "◉",
    }
}

/// The mode line, as Claude Code words it, with `/auto` where it says shift+tab.
pub fn mode_line(mode: &str) -> Option<(String, u32)> {
    let (glyph, words) = match mode {
        "auto" => ("⏵⏵", "auto mode on"),
        "acceptEdits" => ("⏵⏵", "accept edits on"),
        "bypassPermissions" => ("⏵⏵", "bypass permissions on"),
        "plan" => ("⏸", "plan mode on"),
        _ => return None,
    };
    Some((format!("{glyph} {words} (/auto to cycle)"), YELLOW))
}

/// The modes `/auto` steps through, in Claude Code's shift+tab order.
pub const MODES: [&str; 4] = ["default", "acceptEdits", "plan", "auto"];

pub fn next_mode(mode: &str) -> &'static str {
    let i = MODES.iter().position(|m| *m == mode).unwrap_or(0);
    MODES[(i + 1) % MODES.len()]
}

const VERBS: [&str; 12] = [
    "Garnishing",
    "Pondering",
    "Brewing",
    "Simmering",
    "Tinkering",
    "Noodling",
    "Percolating",
    "Whisking",
    "Conjuring",
    "Marinating",
    "Mulling",
    "Kneading",
];
const SPIN: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];

// ── what the client holds beside the conversation ───────────────────────────

/// A dialog the client has open over the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Claude's permission prompt, the cursor on option `sel`.
    Perm { sel: usize },
    /// A question: which one, the cursor, the answers so far, what is typed for "Type
    /// something.", and the multi-select ticks.
    Ask {
        q: usize,
        sel: usize,
        answers: Vec<(String, String)>,
        typing: Option<String>,
        ticked: Vec<bool>,
    },
    /// `/model`: the cursor, and the list once the relay has sent it.
    Model {
        sel: usize,
        list: Option<Vec<(String, String)>>,
    },
    /// `/effort`: the level the slider is on.
    Effort { sel: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Up,
    /// Down since then, and whether an Enter was refused meanwhile.
    Down {
        since: Instant,
        not_sent: bool,
    },
}

/// Everything the conversation screen draws that is the client's, not claude's.
#[derive(Debug, Clone)]
pub struct Ui {
    pub input: String,
    /// The cursor, in chars into `input`.
    pub cursor: usize,
    /// Rows scrolled back from the bottom; 0 follows the conversation.
    pub scroll: usize,
    pub dialog: Option<Dialog>,
    pub link: Link,
    /// `RAM 0.9G/2G`, from the container's latest snapshot.
    pub ram: String,
    /// A word in place of the footer's first line until the next key: `/copy`'s, an error.
    pub status: Option<String>,
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            input: String::new(),
            cursor: 0,
            scroll: 0,
            dialog: None,
            link: Link::Up,
            ram: String::new(),
            status: None,
        }
    }
}

impl Default for Ui {
    fn default() -> Ui {
        Ui::new()
    }
}

// ── drawing ─────────────────────────────────────────────────────────────────

/// The scrollable transcript, top to bottom, at width `w`, and where its last prompt starts.
pub fn transcript(c: &Convo, w: usize) -> (Vec<Line>, Option<usize>) {
    let mut out: Vec<Line> = vec![Vec::new()];
    let ver = if c.meta.version.is_empty() {
        String::new()
    } else {
        format!(" v{}", c.meta.version)
    };
    let model = if c.meta.model.is_empty() {
        String::new()
    } else {
        model_name(&c.meta.model)
    };
    let cwd = home_short(&c.meta.cwd);
    out.push(vec![
        (" ▐▛███▛█   ".into(), fg(ORANGE)),
        (
            "Claude Code".into(),
            St {
                bold: true,
                ..PLAIN
            },
        ),
        (ver, fg(GREY)),
    ]);
    out.push(vec![("▝▜██████▀  ".into(), fg(ORANGE)), (model, fg(GREY))]);
    let cwd = cut(&cwd, w.saturating_sub(13));
    out.push(vec![(" ▝▝   ▝▝   ".into(), fg(ORANGE)), (cwd, fg(GREY))]);
    let mut last_prompt = None;
    let waiting_perm = c.entries.iter().rev().find_map(|e| match e {
        Entry::Perm {
            resolved: None,
            description,
            ..
        } => Some(description.as_str()),
        _ => None,
    });
    let busy = matches!(c.state.as_str(), "working" | "waiting" | "compacting");
    let mut i = 0;
    while i < c.entries.len() {
        let e = &c.entries[i];
        match e {
            Entry::Tool { .. } => {
                let mut tools: Vec<(&str, &str)> = Vec::new();
                let mut open: Option<(&str, &str)> = None;
                while let Some(Entry::Tool { name, arg, result }) = c.entries.get(i) {
                    // Claude Code draws a question as what was answered, not as a tool call.
                    if name != ASK_TOOL {
                        tools.push((name, arg));
                        if result.is_none() {
                            open = Some((name, arg));
                        }
                    }
                    i += 1;
                    // Permission prompts between tool calls belong to the run.
                    while let Some(Entry::Perm { .. }) = c.entries.get(i) {
                        i += 1;
                    }
                }
                if tools.is_empty() {
                    continue;
                }
                let last = i >= c.entries.len();
                out.push(Vec::new());
                match open.filter(|_| last && busy) {
                    Some((name, arg)) => {
                        let head = match waiting_perm {
                            Some(d) if !d.is_empty() => d.to_string(),
                            _ => tools_summary(&tools, true),
                        };
                        out.extend(wrapped(&head, "● ", "  ", w, PLAIN));
                        out.extend(wrapped_by(
                            &tool_line(name, arg),
                            "  ⎿  ",
                            "     ",
                            w.saturating_sub(1),
                            fg(GREY),
                        ));
                    }
                    None => out.extend(wrapped(
                        &tools_summary(&tools, false),
                        "  ",
                        "  ",
                        w,
                        fg(GREY),
                    )),
                }
                continue;
            }
            Entry::Prompt(text) => {
                out.push(Vec::new());
                last_prompt = Some(out.len());
                let band = St {
                    bg: Some(BAND),
                    ..PLAIN
                };
                for l in wrapped(text, "❯ ", "  ", w, band) {
                    let pad = w.saturating_sub(width(&plain(&l)));
                    let mut l = l;
                    l.push((" ".repeat(pad), band));
                    out.push(l);
                }
            }
            Entry::Text(text) => {
                out.push(Vec::new());
                out.extend(wrapped(text.trim_end(), "● ", "  ", w, PLAIN));
            }
            Entry::Note(text) => {
                out.push(Vec::new());
                out.extend(wrapped(text, "  ⎿  ", "     ", w, fg(GREY)));
            }
            Entry::Compacted => {
                out.push(Vec::new());
                out.push(line("✻ Conversation compacted", fg(GREY)));
            }
            Entry::Worked(text) => {
                out.push(Vec::new());
                out.extend(wrapped(text, "✻ ", "  ", w, fg(GREY)));
            }
            Entry::Ask {
                questions,
                resolved: Some(how),
                answers,
                ..
            } if how == "answered" => {
                out.push(Vec::new());
                out.extend(wrapped(
                    "User answered Claude's questions:",
                    "● ",
                    "  ",
                    w,
                    PLAIN,
                ));
                for (n, q) in questions.iter().enumerate() {
                    let said = match answers.iter().find(|(t, _)| *t == q.question) {
                        Some((_, a)) => format!("{} → {a}", q.question),
                        None => q.question.clone(),
                    };
                    let first = if n == 0 { "  ⎿  · " } else { "     · " };
                    out.extend(wrapped(&said, first, "       ", w, fg(GREY)));
                }
            }
            Entry::Perm { .. } | Entry::Ask { .. } => {}
        }
        i += 1;
    }
    (out, last_prompt)
}

fn home_short(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.splitn(4, '/').collect();
    match parts.as_slice() {
        ["", "home", _user] => "~".to_string(),
        ["", "home", _user, rest] => format!("~/{rest}"),
        ["", "root"] => "~".to_string(),
        _ => cwd.to_string(),
    }
}

fn rule(w: usize, glyph: &str, st: St) -> Line {
    line(glyph.repeat(w), st)
}

/// The input box and footer: R1's last five rows.
fn input_box(c: &Convo, ui: &Ui, w: usize) -> (Vec<Line>, (usize, usize)) {
    let mut out = Vec::new();
    let title = c.meta.title.trim();
    if title.is_empty() || width(title) + 4 > w {
        out.push(rule(w, "─", fg(RULE)));
    } else {
        let left = w - width(title) - 3;
        out.push(vec![
            ("─".repeat(left), fg(RULE)),
            (format!(" {title} "), fg(GREY)),
            ("─".into(), fg(RULE)),
        ]);
    }
    // The prompt, wrapped as typed, with the cursor's place.
    let room = w.saturating_sub(2).max(1);
    let chars: Vec<char> = ui.input.chars().collect();
    let mut rows: Vec<String> = vec![String::new()];
    let mut cur = (0usize, 0usize);
    let mut col = 0;
    for (i, ch) in chars.iter().enumerate() {
        if i == ui.cursor {
            cur = (rows.len() - 1, col);
        }
        if *ch == '\n' || col + cw(*ch) > room {
            rows.push(String::new());
            col = 0;
            if *ch == '\n' {
                continue;
            }
        }
        rows.last_mut().expect("one row").push(*ch);
        col += cw(*ch);
    }
    if ui.cursor >= chars.len() {
        if col >= room {
            rows.push(String::new());
            col = 0;
        }
        cur = (rows.len() - 1, col);
    }
    let first = out.len();
    for (i, r) in rows.iter().enumerate() {
        let lead = if i == 0 { "❯ " } else { "  " };
        out.push(line(format!("{lead}{r}"), PLAIN));
    }
    let cursor = (first + cur.0, 2 + cur.1);
    out.push(rule(w, "─", fg(RULE)));
    let place = format!("{}/{}", c.host, c.slot);
    let first_foot = match (&ui.link, &ui.status) {
        (Link::Down { since, not_sent }, _) => {
            let secs = since.elapsed().as_secs();
            let tail = if *not_sent { " · not sent" } else { "" };
            format!("Reconnecting… ({secs}s){tail}")
        }
        (Link::Up, Some(word)) => word.clone(),
        (Link::Up, None) if ui.ram.is_empty() => place,
        (Link::Up, None) => format!("{place} · {}", ui.ram),
    };
    out.push(line(
        format!("  {}", cut(&first_foot, w.saturating_sub(2))),
        fg(GREY),
    ));
    let busy = matches!(c.state.as_str(), "working" | "compacting");
    if busy {
        out.push(line("  esc to interrupt", fg(GREY)));
    } else if let Some((m, colour)) = mode_line(&c.meta.mode) {
        out.push(line(
            format!("  {}", cut(&m, w.saturating_sub(2))),
            fg(colour),
        ));
    } else {
        out.push(Vec::new());
    }
    (out, cursor)
}

fn perm_dialog(e: &Entry, sel: usize, w: usize) -> Vec<Line> {
    let Entry::Perm {
        tool,
        arg,
        description,
        always,
        ..
    } = e
    else {
        return Vec::new();
    };
    let mut out = vec![rule(w, "─", fg(LAVENDER))];
    out.push(line(format!(" {}", perm_title(tool)), bold(LAVENDER)));
    if !description.is_empty() {
        out.extend(wrapped(description, " ", " ", w, PLAIN));
    }
    out.push(rule(w, "╌", fg(LAVENDER)));
    out.extend(wrapped(arg, " ", " ", w, PLAIN));
    out.push(rule(w, "╌", fg(LAVENDER)));
    out.push(line(" Do you want to proceed?", PLAIN));
    for (i, opt) in perm_options(always).iter().enumerate() {
        let n = i + 1;
        let (first, st) = if i == sel {
            (format!(" ❯ {n}. "), fg(LAVENDER))
        } else {
            (format!("   {n}. "), PLAIN)
        };
        out.extend(wrapped(opt, &first, "      ", w, st));
    }
    out.push(Vec::new());
    out.push(line(" Esc to cancel", fg(GREY)));
    out
}

/// The permission prompt's options: Yes, "don't ask again" when claude suggested a rule, No.
pub fn perm_options(always: &str) -> Vec<String> {
    let mut v = vec!["Yes".to_string()];
    if !always.is_empty() {
        v.push(
            always
                .strip_prefix("Yes, ")
                .map_or(always.to_string(), |r| {
                    let mut c = r.chars();
                    match c.next() {
                        Some(f) => format!("Yes, {}{}", f, c.as_str()),
                        None => always.to_string(),
                    }
                }),
        );
    }
    v.push("No".to_string());
    v
}

/// The options a question's list has: its own, then "Type something.", then "Submit" for a
/// multi-select. "Chat about this" is drawn apart, under the list.
pub fn ask_options(q: &Question) -> Vec<String> {
    let mut v: Vec<String> = q.options.iter().map(|o| o.0.clone()).collect();
    v.push("Type something.".into());
    if q.multi {
        v.push("Submit".into());
    }
    v
}

fn ask_dialog(q: &Question, d: &Dialog, w: usize, room: usize) -> Vec<Line> {
    let Dialog::Ask {
        sel,
        typing,
        ticked,
        ..
    } = d
    else {
        return Vec::new();
    };
    let mut head = vec![rule(w, "─", fg(LAVENDER))];
    head.push(line(format!(" ☐ {}", q.header), bold(LAVENDER)));
    head.push(Vec::new());
    head.extend(wrapped(&q.question, "", "", w, PLAIN));
    head.push(Vec::new());
    let opts = ask_options(q);
    let n_opts = opts.len();
    // Each option's block: its label line(s), then its description, indented five.
    let blocks: Vec<Vec<Line>> = opts
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let mark = if q.multi && i < q.options.len() {
                if ticked.get(i).copied().unwrap_or(false) {
                    "[✔] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let label = match (typing, i == q.options.len()) {
                (Some(t), true) if i == *sel => format!("{t}▏"),
                _ => label.clone(),
            };
            let n = i + 1;
            let (first, st) = if i == *sel {
                (format!("❯ {n}. {mark}"), fg(LAVENDER))
            } else {
                (format!("  {n}. {mark}"), PLAIN)
            };
            let mut b = wrapped(&label, &first, "     ", w, st);
            if let Some((_, desc, _)) = q.options.get(i) {
                if !desc.is_empty() {
                    b.extend(wrapped(desc, "     ", "     ", w, fg(GREY)));
                }
            }
            b
        })
        .collect();
    let preview = q
        .options
        .get(*sel)
        .map(|o| o.2.as_str())
        .filter(|p| !p.is_empty());
    let mut tail = Vec::new();
    if let Some(p) = preview {
        tail.push(rule(w, "╌", fg(LAVENDER)));
        for l in p.split('\n') {
            tail.push(line(format!(" {}", cut(l, w.saturating_sub(1))), PLAIN));
        }
        tail.push(rule(w, "╌", fg(LAVENDER)));
    }
    tail.push(rule(w, "─", fg(GREY)));
    let chat = n_opts + 1;
    let chat_sel = *sel == n_opts;
    tail.push(line(
        format!(
            "{}{chat}. Chat about this",
            if chat_sel { "❯ " } else { "  " }
        ),
        if chat_sel { fg(LAVENDER) } else { PLAIN },
    ));
    tail.push(Vec::new());
    tail.extend(hints(
        &["Enter to select", "↑/↓ to navigate", "Esc to cancel"],
        "",
        w,
    ));
    // The list scrolls around the cursor, with `↑ 1 more` and `↓ 2 more` at its edges.
    let fixed = head.len() + tail.len();
    let list_room = room.saturating_sub(fixed).max(3);
    let total: usize = blocks.iter().map(Vec::len).sum();
    let mut list = Vec::new();
    if total <= list_room {
        for b in &blocks {
            list.extend(b.iter().cloned());
        }
    } else {
        let focus = (*sel).min(blocks.len().saturating_sub(1));
        let (mut lo, mut hi) = (focus, focus + 1);
        let fits = |lo: usize, hi: usize| {
            let body: usize = blocks[lo..hi].iter().map(Vec::len).sum();
            body + usize::from(lo > 0) + usize::from(hi < blocks.len()) <= list_room
        };
        loop {
            let grew_down = hi < blocks.len() && fits(lo, hi + 1);
            if grew_down {
                hi += 1;
            }
            let grew_up = lo > 0 && fits(lo - 1, hi);
            if grew_up {
                lo -= 1;
            }
            if !grew_down && !grew_up {
                break;
            }
        }
        if lo > 0 {
            list.push(line(format!("     ↑ {lo} more"), fg(GREY)));
        }
        for b in &blocks[lo..hi] {
            let left = list_room.saturating_sub(list.len() + usize::from(hi < blocks.len()));
            list.extend(b.iter().take(left).cloned());
        }
        if hi < blocks.len() {
            list.push(line(format!("     ↓ {} more", blocks.len() - hi), fg(GREY)));
        }
    }
    let mut out = head;
    out.extend(list);
    out.extend(tail);
    out
}

fn model_dialog(
    c: &Convo,
    sel: usize,
    list: &Option<Vec<(String, String)>>,
    w: usize,
) -> Vec<Line> {
    let mut out = vec![rule(w, "▔", fg(LAVENDER))];
    out.push(line("   Select model", bold(LAVENDER)));
    out.push(line("   For this session only.", fg(GREY)));
    out.push(Vec::new());
    match list {
        None => out.push(line("   Loading models…", fg(GREY))),
        Some(list) => {
            const SHOWN: usize = 4;
            // One past the cursor stays in view, as Claude Code's list scrolls.
            let lo = sel
                .saturating_sub(SHOWN - 2)
                .min(list.len().saturating_sub(SHOWN));
            let hi = (lo + SHOWN).min(list.len());
            for (i, (value, name)) in list.iter().enumerate().take(hi).skip(lo) {
                let current = is_current(&c.meta.model, value, name);
                let mark = if i == sel {
                    "❯"
                } else if i == lo && lo > 0 {
                    "↑"
                } else {
                    " "
                };
                let st = if i == sel { fg(LAVENDER) } else { PLAIN };
                let mut l = vec![(format!("   {mark} {}.  {name}", i + 1), st)];
                if current {
                    l.push((" ✔".into(), fg(GREEN)));
                }
                out.push(l);
            }
            if hi < list.len() {
                out.push(line(
                    format!("      … +{} models", list.len() - hi),
                    fg(GREY),
                ));
            }
        }
    }
    out.push(Vec::new());
    out.extend(hints(&["Enter to select", "Esc to cancel"], "   ", w));
    out
}

/// Whether a list entry is the model running: its alias or its name in the model's id.
pub fn is_current(model: &str, value: &str, name: &str) -> bool {
    if model.is_empty() {
        return false;
    }
    if model == value {
        return true;
    }
    let slug = name.to_lowercase().replace([' ', '.'], "-");
    !slug.is_empty() && model.contains(&slug)
}

fn effort_dialog(sel: usize, w: usize) -> Vec<Line> {
    let level = EFFORTS[sel.min(EFFORTS.len() - 1)];
    let tag = format!(" {} {level} · /effort ", effort_glyph(level));
    let left = w.saturating_sub(width(&tag) + 1);
    let mut out = vec![vec![
        ("▔".repeat(left), fg(LAVENDER)),
        (tag, fg(LAVENDER)),
        ("▔".into(), fg(LAVENDER)),
    ]];
    out.push(line("   Effort", bold(LAVENDER)));
    out.push(Vec::new());
    out.push(line(
        format!("   Faster{}Smarter", " ".repeat(20)),
        fg(GREY),
    ));
    // The labels, and the slider's mark under the chosen one.
    let mut labels = String::new();
    let mut at = 0;
    for (i, l) in EFFORTS.iter().enumerate() {
        if i > 0 {
            labels.push_str("  ");
        }
        if i == sel {
            at = width(&labels) + (width(l) / 2).saturating_sub(1);
        }
        labels.push_str(l);
    }
    let track = 34;
    let mut slider = String::new();
    for i in 0..track {
        slider.push(if i == at { '▲' } else { '─' });
    }
    out.push(vec![("   ".into(), PLAIN), (slider, fg(GREY))]);
    out.push(line(format!("   {labels}"), PLAIN));
    out.push(Vec::new());
    out.extend(hints(
        &["←/→ to adjust", "Enter to confirm", "Esc to cancel"],
        "   ",
        w,
    ));
    out
}

/// A frame: exactly `h` rows, and where the terminal's cursor goes when the prompt has it.
pub struct Frame {
    pub lines: Vec<Line>,
    pub cursor: Option<(usize, usize)>,
}

/// What a frame is made of: the transcript, what goes under it, the rows it has, and the
/// line it starts at when not scrolled back.
struct Parts {
    body: Vec<Line>,
    bottom: Vec<Line>,
    spinner: Vec<Line>,
    cursor: Option<(usize, usize)>,
    ask_top: bool,
    room: usize,
    start: usize,
}

fn parts(c: &Convo, ui: &Ui, w: usize, h: usize, now: Instant) -> Parts {
    let (body, last_prompt) = transcript(c, w);
    // The bottom: a dialog, or the input box.
    let waiting = c.waiting();
    let mut cursor = None;
    let mut ask_top = false;
    let bottom: Vec<Line> = match (&ui.dialog, waiting) {
        (Some(Dialog::Model { sel, list }), _) => model_dialog(c, *sel, list, w),
        (Some(Dialog::Effort { sel }), _) => effort_dialog(*sel, w),
        (Some(d @ Dialog::Ask { q, .. }), Some(Entry::Ask { questions, .. })) => {
            ask_top = true;
            match questions.get(*q) {
                Some(qq) => ask_dialog(qq, d, w, h.saturating_sub(3)),
                None => Vec::new(),
            }
        }
        (Some(Dialog::Perm { sel }), Some(e @ Entry::Perm { .. })) => perm_dialog(e, *sel, w),
        _ => {
            let (b, cur) = input_box(c, ui, w);
            cursor = Some(cur);
            b
        }
    };
    let spinner: Vec<Line> = match (c.state.as_str(), &cursor) {
        ("working" | "compacting", Some(_)) => {
            let since = c.working_since.unwrap_or(now);
            let el = now.saturating_duration_since(since);
            let frame = SPIN[(el.as_millis() / 120) as usize % SPIN.len()];
            let words = if c.state == "compacting" {
                "Compacting conversation".to_string()
            } else {
                let seed = c.entries.len() % VERBS.len();
                VERBS[seed].to_string()
            };
            vec![
                vec![
                    (format!("{frame} "), fg(ORANGE)),
                    (format!("{words}… ({}s)", el.as_secs()), fg(ORANGE)),
                ],
                Vec::new(),
            ]
        }
        _ => Vec::new(),
    };
    let room = h.saturating_sub(bottom.len() + spinner.len());
    let room = if ask_top { room.min(3) } else { room };
    // Claude Code puts a prompt just sent at the top and keeps it there while its turn runs,
    // a question waits or a dialog is open (R2 to R6); an idle screen that fits is drawn from
    // the top, logo and all (R1).
    let pin = c.state != "idle" || ui.dialog.is_some() || cursor.is_none();
    let total = body.len();
    let start = match last_prompt {
        Some(p) if pin && total - p <= room => p,
        _ if total <= room => 0,
        Some(p) if total - p <= room => p,
        _ => total - room,
    };
    Parts {
        body,
        bottom,
        spinner,
        cursor,
        ask_top,
        room,
        start,
    }
}

/// Draw the conversation at `w` by `h`.
pub fn draw(c: &Convo, ui: &Ui, w: usize, h: usize, now: Instant) -> Frame {
    let Parts {
        body,
        bottom,
        spinner,
        cursor,
        ask_top,
        room,
        start,
    } = parts(c, ui, w, h, now);
    let mut top: Vec<Line> = Vec::new();
    let total = body.len();
    if ui.scroll == 0 {
        top.extend(body.iter().skip(start).take(room).cloned());
    } else {
        // Scrolled back: the prompt this part answers pinned on top, the jump line under it.
        // The top moves back from where it sat at the bottom, one line a step: measured from
        // the end instead, a prompt drawn at the top with room to spare under it jumped by
        // all that room on the first step.
        let content = room.saturating_sub(3);
        let start = start.saturating_sub(ui.scroll);
        let end = (start + content).min(total);
        // The prompt's first line, and `…` when it ran on.
        let pinned = body[..start.min(total)]
            .iter()
            .rposition(|l| plain(l).starts_with("❯ "))
            .map(|i| {
                let first = plain(&body[i]).trim_end().to_string();
                let more = body
                    .get(i + 1)
                    .is_some_and(|l| l.first().is_some_and(|r| r.1.bg == Some(BAND)));
                if more { format!("{first}…") } else { first }
            })
            .unwrap_or_default();
        top.push(line(cut(&pinned, w), PLAIN));
        top.extend(body[start..end].iter().cloned());
        while top.len() < room.saturating_sub(2) {
            top.push(Vec::new());
        }
        let jump = "Jump to bottom (ctrl+End) ↓";
        let pad = w.saturating_sub(width(jump) - 1) / 2;
        top.push(line(format!("{}{jump}", " ".repeat(pad)), fg(GREY)));
        top.push(Vec::new());
    }
    while top.len() < room {
        top.push(Vec::new());
    }
    let mut lines = top;
    let at = lines.len() + spinner.len();
    lines.extend(spinner);
    lines.extend(bottom);
    if ask_top {
        while lines.len() < h {
            lines.push(Vec::new());
        }
    }
    lines.truncate(h);
    Frame {
        lines,
        cursor: cursor.map(|(r, col)| (at + r, col)),
    }
}

/// The furthest back the transcript scrolls at this size.
pub fn max_scroll(c: &Convo, ui: &Ui, w: usize, h: usize) -> usize {
    parts(c, ui, w, h, Instant::now()).start
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn feed(c: &mut Convo, ops: &[&str]) {
        let now = Instant::now();
        for o in ops {
            c.apply(&parse(o).unwrap(), now);
        }
    }

    fn text(f: &Frame) -> Vec<String> {
        f.lines
            .iter()
            .map(|l| plain(l).trim_end().to_string())
            .collect()
    }

    /// The mockup's rows, as `docs/mockups.md` has them.
    fn mockup(name: &str) -> Vec<String> {
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/mockups.md"))
            .unwrap();
        let at = doc
            .find(&format!("### Mockup {name} "))
            .unwrap_or_else(|| panic!("{name} is in the mockups"));
        let body = &doc[at..];
        let start = body.find("```\n").unwrap() + 4;
        let end = start + body[start..].find("```").unwrap();
        body[start..end]
            .trim_end_matches('\n')
            .split('\n')
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    fn meta(title: &str, mode: &str) -> String {
        format!(
            r#"{{"t":"meta","sid":"s","m":"claude-opus-5-5","pm":"{mode}","ti":"{title}","e":"","cwd":"/home/dev/workspace","ver":"2.1.311"}}"#
        )
    }

    fn convo() -> Convo {
        let mut c = Convo::new("claude-sessions", "claude-3", "run");
        c.apply(&parse(r#"{"t":"n","n":0}"#).unwrap(), Instant::now());
        c
    }

    #[test]
    fn r2_working_puts_the_new_prompt_on_top_and_the_spinner_over_the_box() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("fix the door", "auto"),
                r#"{"t":"add","id":1,"k":"user","x":"Run the tests and tell me if any fail."}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Bash","a":"cargo test"}"#,
                r#"{"t":"res","id":2,"ok":true,"p":["ok"],"l":1}"#,
                r#"{"t":"add","id":3,"k":"text","x":"All 478 tests pass; 4 are ignored."}"#,
            ],
        );
        c.local(Entry::Worked("Worked for 41s · done 1:12 AM".into()));
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":4,"k":"user","x":"now run clippy"}"#,
                r#"{"t":"st","s":"working"}"#,
                r#"{"t":"add","id":5,"k":"tool","n":"Bash","a":"cargo clippy --all-targets -- -D warnings"}"#,
            ],
        );
        let mut ui = Ui::new();
        ui.ram = "RAM 1.2G/2G".into();
        let now = c.working_since.unwrap() + Duration::from_secs(12);
        let got = text(&draw(&c, &ui, 40, 24, now));
        let want = mockup("R2");
        // The spinner's words are a choice among Claude Code's; the rest is the mockup's.
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            if w.starts_with('✢') {
                assert!(g.ends_with("… (12s)"), "row {i}: {g:?}");
                continue;
            }
            assert_eq!(g, w, "row {i}");
        }
        assert_eq!(got.len(), 24);
    }

    #[test]
    fn r1_idle_draws_the_logo_block_the_collapsed_tools_and_the_mode_line() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("fix the door", "auto"),
                r#"{"t":"add","id":1,"k":"user","x":"Run the tests and tell me if any fail."}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Bash","a":"cargo test"}"#,
                r#"{"t":"res","id":2,"ok":true,"p":["ok"],"l":1}"#,
                r#"{"t":"add","id":3,"k":"text","x":"All 478 tests pass; 4 are ignored."}"#,
            ],
        );
        c.local(Entry::Worked("Worked for 41s · done 1:12 AM".into()));
        let mut ui = Ui::new();
        ui.ram = "RAM 0.9G/2G".into();
        ui.input = "now run clippy".into();
        ui.cursor = ui.input.chars().count();
        let f = draw(&c, &ui, 40, 24, Instant::now());
        let got = text(&f);
        let mut want = mockup("R1");
        // The plan is the account's, which claude's lines never say: the model line stops
        // at the model.
        want[2] = "▝▜██████▀  Opus 5.5".into();
        assert_eq!(got, want);
        assert_eq!(f.cursor, Some((20, 16)), "after what is typed");
        // Nothing said by colour alone: the mode is in words, in Claude's yellow.
        assert_eq!(f.lines[23][0].1.fg, Some(YELLOW));
    }

    #[test]
    fn r3_a_permission_prompt_with_claudes_dont_ask_again_rule() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("", "default"),
                r#"{"t":"add","id":1,"k":"user","x":"make a scratch file"}"#,
                r#"{"t":"st","s":"working"}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Bash","a":"touch x"}"#,
                r#"{"t":"add","id":3,"k":"perm","r":"p1","n":"Bash","a":"touch x","d":"Create empty file named x","al":"Yes, and don't ask again for touch commands in this project"}"#,
                r#"{"t":"st","s":"waiting"}"#,
            ],
        );
        let mut ui = Ui::new();
        ui.dialog = Some(Dialog::Perm { sel: 0 });
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        let mut want = mockup("R3");
        // The transcript's line is the request's own description, as claude sent it.
        want[2] = "● Create empty file named x".into();
        assert_eq!(got, want);
    }

    #[test]
    fn r4_long_options_scroll_around_the_cursor() {
        let mut c = convo();
        let q = r#"{"t":"add","id":2,"k":"ask","r":"q1","q":[{"q":"Where should the hub keep a session's log between reconnects?","h":"Log store","m":false,"o":[["In the relay","Gone with it."],["On disk beside the registry","Survives a hub restart. One write per line, fsynced at the end of each turn; files older than 7 days are pruned by the offloader."],["In memory only","Fastest and nothing to prune, but a hub restart loses the history."],["In the holder","Already there."]]}]}"#;
        feed(
            &mut c,
            &[
                &meta("", "default"),
                r#"{"t":"add","id":1,"k":"user","x":"where should the hub keep logs?"}"#,
                q,
                r#"{"t":"st","s":"waiting"}"#,
            ],
        );
        let mut ui = Ui::new();
        ui.dialog = Some(Dialog::Ask {
            q: 0,
            sel: 1,
            answers: vec![],
            typing: None,
            ticked: vec![],
        });
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        assert_eq!(got, mockup("R4"));
    }

    #[test]
    fn an_answered_question_stays_in_the_transcript_as_what_was_chosen() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("", "default"),
                r#"{"t":"add","id":1,"k":"user","x":"pick a colour"}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"AskUserQuestion","a":""}"#,
                r#"{"t":"add","id":3,"k":"ask","r":"q1","q":[{"q":"Red or blue?","h":"Colour","m":false,"o":[["Red","r"],["Blue","b"]]}]}"#,
                r#"{"t":"done","id":3,"x":"answered","a":{"Red or blue?":"Blue"}}"#,
                r#"{"t":"res","id":2,"ok":true,"p":["User has answered"],"l":1}"#,
                r#"{"t":"st","s":"idle"}"#,
            ],
        );
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        let at = got
            .iter()
            .position(|l| l == "● User answered Claude's questions:")
            .expect("the answer is drawn");
        assert_eq!(got[at + 1], "  ⎿  · Red or blue? → Blue");
        assert!(
            !got.iter().any(|l| l.contains("Used 1 tool")),
            "the question is not a tool call: {got:?}"
        );
        // One withdrawn, or still waiting, draws nothing in the transcript.
        let mut c = convo();
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":3,"k":"ask","r":"q1","q":[{"q":"Red or blue?","h":"Colour","m":false,"o":[["Red","r"]]}]}"#,
                r#"{"t":"done","id":3,"x":"withdrawn"}"#,
            ],
        );
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        assert!(!got.iter().any(|l| l.contains("answered")), "{got:?}");
    }

    #[test]
    fn scrolling_back_from_a_prompt_on_top_moves_a_line_a_step() {
        let mut c = convo();
        let numbers: Vec<String> = (1..=30).map(|n| n.to_string()).collect();
        let add = format!(
            r#"{{"t":"add","id":2,"k":"text","x":"{}"}}"#,
            numbers.join("\\n")
        );
        feed(
            &mut c,
            &[
                &meta("", "default"),
                r#"{"t":"add","id":1,"k":"user","x":"count"}"#,
                &add,
                r#"{"t":"add","id":3,"k":"user","x":"again"}"#,
                r#"{"t":"add","id":4,"k":"text","x":"ok"}"#,
                r#"{"t":"st","s":"idle"}"#,
            ],
        );
        let mut ui = Ui::new();
        let bottom = text(&draw(&c, &ui, 40, 24, Instant::now()));
        assert!(
            bottom[0].starts_with("❯ again"),
            "the prompt on top: {bottom:?}"
        );
        ui.scroll = 1;
        let back = text(&draw(&c, &ui, 40, 24, Instant::now()));
        // Under the pinned line, the line above the prompt, then the prompt: not a screen of
        // the count, which is where measuring from the end put it.
        assert_eq!(back[1], "");
        assert!(back[2].starts_with("❯ again"), "{back:?}");
        assert_eq!(max_scroll(&c, &ui, 40, 24), {
            let (body, last) = transcript(&c, 40);
            assert!(body.len() > 1);
            last.unwrap()
        });
    }

    #[test]
    fn r5_and_r6_model_and_effort() {
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        c.entries.clear();
        c.local(Entry::Prompt("/model".into()));
        let list: Vec<(String, String)> = [
            ("default", "Default"),
            ("opus", "Opus 5.5"),
            ("fable", "Fable 5.1"),
            ("sonnet", "Sonnet 5.5"),
            ("haiku", "Haiku 5.5"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .chain((0..8).map(|i| (format!("m{i}"), format!("Model {i}"))))
        .collect();
        let mut ui = Ui::new();
        ui.dialog = Some(Dialog::Model {
            sel: 3,
            list: Some(list),
        });
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        let mut want = mockup("R5");
        // The logo block is on top until the conversation has more than the screen holds.
        want.splice(
            0..1,
            transcript(&c, 40)
                .0
                .iter()
                .map(|l| plain(l).trim_end().to_string()),
        );
        want.truncate(24);
        let tail = 11;
        assert_eq!(got[24 - tail..], mockup("R5")[24 - tail..]);

        c.entries.clear();
        c.local(Entry::Prompt("/effort".into()));
        ui.dialog = Some(Dialog::Effort { sel: 1 });
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        assert_eq!(got[15..], mockup("R6")[15..]);
    }

    #[test]
    fn r7_scrolled_back_pins_the_prompt_and_offers_the_way_down() {
        let mut c = convo();
        let numbers: Vec<String> = (1..=60).map(|n| n.to_string()).collect();
        let add = format!(
            r#"{{"t":"add","id":2,"k":"text","x":"{}"}}"#,
            numbers.join("\\n")
        );
        feed(
            &mut c,
            &[
                &meta("fix the door", "auto"),
                r#"{"t":"add","id":1,"k":"user","x":"Write the numbers 1 to 60, one per line."}"#,
                &add,
            ],
        );
        let mut ui = Ui::new();
        ui.ram = "RAM 0.9G/2G".into();
        // Four rows back from the bottom: the mockup has 53 at the bottom of 60, three rows
        // going to the pinned prompt and the jump line.
        ui.scroll = 4;
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        let want = mockup("R7");
        assert_eq!(got[0], want[0]);
        assert_eq!(got[17..], want[17..]);
        assert_eq!(got[16], "  53");
    }

    #[test]
    fn r8_a_dropped_link_says_so_in_words_and_what_was_not_sent() {
        let mut c = convo();
        feed(&mut c, &[&meta("fix the door", "auto")]);
        let mut ui = Ui::new();
        ui.link = Link::Down {
            since: Instant::now() - Duration::from_secs(8),
            not_sent: true,
        };
        let f = draw(&c, &ui, 40, 24, Instant::now());
        assert_eq!(text(&f)[22], "  Reconnecting… (8s) · not sent");
    }

    #[test]
    fn a_turn_seen_live_is_timed_and_one_replayed_is_not() {
        let t0 = Instant::now();
        let mut c = Convo::new("h", "s", "r");
        // The log as it stood: a whole turn, then the first batch's mark.
        for o in [r#"{"t":"st","s":"working"}"#, r#"{"t":"st","s":"idle"}"#] {
            assert_eq!(c.apply(&parse(o).unwrap(), t0), Applied::Nothing);
        }
        c.apply(&parse(r#"{"t":"n","n":5}"#).unwrap(), t0);
        c.apply(&parse(r#"{"t":"st","s":"working"}"#).unwrap(), t0);
        assert_eq!(
            c.apply(
                &parse(r#"{"t":"st","s":"idle"}"#).unwrap(),
                t0 + Duration::from_secs(41)
            ),
            Applied::TurnEnded(Duration::from_secs(41))
        );
    }

    #[test]
    fn names_modes_and_wrapping_as_claude_code_says_them() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_name("claude-sonnet-5-5[1m]"), "Sonnet 5.5");
        assert_eq!(next_mode("default"), "acceptEdits");
        assert_eq!(next_mode("auto"), "default");
        assert_eq!(next_mode("bypassPermissions"), "acceptEdits");
        assert!(is_current("claude-opus-5-5", "opus", "Opus 5.5"));
        assert!(!is_current("claude-opus-5-5", "fable", "Fable 5.1"));
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(cut("Write the numbers", 10), "Write the…");
        assert_eq!(
            tools_summary(&[("Bash", "a"), ("Read", "b"), ("Bash", "c")], false),
            "Ran 2 shell commands, read 1 file"
        );
    }
}
