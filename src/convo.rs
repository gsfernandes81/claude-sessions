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
/// Claude Code's dark theme beyond the colours above (`claude` 2.1.311's `error`,
/// `autoAccept` and `planMode`): a failure, accept-edits mode, plan mode.
pub const RED: u32 = 0xff_6b80;
pub const PURPLE: u32 = 0xaf_87ff;
pub const TEAL: u32 = 0x48_968c;
/// The rules around the prompt while an agent is in view, Claude Code's `0891b2`.
pub const AGENT_TEAL: u32 = 0x08_91b2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct St {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    /// Claude's markdown (`markdown.rs`): emphasis, a first-level heading, struck text.
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

pub const PLAIN: St = St {
    fg: None,
    bg: None,
    bold: false,
    italic: false,
    underline: false,
    strike: false,
};

pub(crate) fn fg(c: u32) -> St {
    St {
        fg: Some(c),
        ..PLAIN
    }
}

pub(crate) fn bold(c: u32) -> St {
    St {
        fg: Some(c),
        bold: true,
        ..PLAIN
    }
}

/// One row of the screen: runs of text, each in one style.
pub type Line = Vec<(String, St)>;

pub(crate) fn line(text: impl Into<String>, st: St) -> Line {
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
    /// The server's summary of the prose between tool calls (a narration thinking block):
    /// drawn as text, with Claude Code's grey `· summary` after it, and left out of `/copy`,
    /// which takes text blocks alone.
    Narration(String),
    Tool {
        name: String,
        arg: String,
        /// Whether it succeeded, its first lines, and how many lines in all.
        result: Option<(bool, Vec<String>, usize)>,
        /// claude's summary of the step it was one of, and the step's first item, which
        /// the step's calls share: drawn as one line in place of counting them.
        summary: Option<(String, u64)>,
    },
    /// A subagent or workflow: its type or `Workflow`, its description, the lines under it
    /// as the relay last sent them, and `running`, `completed`, `failed` or `stopped`.
    Agent {
        name: String,
        arg: String,
        lines: Vec<String>,
        state: String,
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
    /// A message another agent sent: who from, what it said, and whether Claude Code puts its
    /// words on the collapsed line (a peer session's).
    Message {
        from: String,
        body: String,
        inline: bool,
    },
    Compacted,
    /// `✻ Worked for 41s · done 1:12 AM`: the client's own, at the end of a turn it saw.
    Worked(String),
}

/// One of claude's background tasks, as the relay's `tasks` op lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Task {
    pub id: String,
    /// `shell`, `monitor`, `agent`, `workflow`, `remote`, or a kind a later claude has.
    pub kind: String,
    pub description: String,
    /// A shell's or monitor's command, a subagent's type.
    pub detail: String,
    /// A subagent's prompt, the start of it.
    pub prompt: String,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    /// `running`, `completed`, `failed`, `killed` or `stopped`.
    pub status: String,
    /// The transcript item a subagent or workflow is drawn as.
    pub item: Option<u64>,
    /// A subagent's model id, tokens and tool calls, and its latest call; a workflow's agents
    /// done and in all; whether a workflow has a script `s save` can keep.
    pub model: String,
    pub tokens: u64,
    pub tools: u64,
    pub activity: String,
    pub agents_done: u64,
    pub agents: u64,
    pub saveable: bool,
}

impl Task {
    pub fn running(&self) -> bool {
        self.status == "running"
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub session_id: String,
    pub model: String,
    /// The model as chosen, `default` or an alias, where the server knows it (`ch`): what
    /// `/model` marks, since `model` is only what it resolved to.
    pub chosen: String,
    pub mode: String,
    pub title: String,
    pub effort: String,
    pub cwd: String,
    pub version: String,
    /// claude's slash commands, without their `/`: coloured in the prompt (`slash_runs`).
    pub commands: Vec<String>,
    /// The account's 5-hour and weekly usage, drawn at the footer's right (`usage`).
    pub five_hour: Option<crate::sdk::Window>,
    pub seven_day: Option<crate::sdk::Window>,
}

#[derive(Debug, Default)]
pub struct Convo {
    pub host: String,
    pub slot: String,
    pub run: String,
    /// Every op of the log's lines up to this has arrived (`n`).
    pub n: u64,
    pub entries: Vec<Entry>,
    /// claude's background tasks, as the relay last listed them.
    pub tasks: Vec<Task>,
    /// How many background tasks' notices have arrived: the tasks view says how many came
    /// while it was open, as Claude Code's does.
    pub notices: u64,
    /// The agent in view instead of the conversation, Claude Code's foregrounded agent: its
    /// task id, and its own conversation as the relay sends it (`fgo`).
    pub viewing: Option<(String, Box<Convo>)>,
    /// Where each numbered item is in `entries`.
    index: HashMap<u64, usize>,
    pub state: String,
    pub meta: Meta,
    /// When the turn now running started, as this client saw it.
    pub working_since: Option<Instant>,
    /// Whether ops are arriving live: false until the first batch after opening, which is the
    /// log as it stood.
    live: bool,
    /// Every tool call drawn on its own with the first lines of its result, rather than a run
    /// of them summed up: Ctrl+O in Claude Code, `/expand` here too.
    pub expanded: bool,
    /// Prompts sent and not yet echoed by claude, and when each was sent: drawn at once under
    /// the transcript, the spinner where `❯` goes, until claude's own line for it arrives.
    pub pending: Vec<Pending>,
    /// Prompts this view sent with a paste's stand-in in them: the text claude got, trimmed,
    /// to the prompt as it stood in the input box, which the transcript draws instead, as
    /// Claude Code's does. Only this client's memory: another device, or this one restarted,
    /// draws the paste whole.
    pub shown: HashMap<String, String>,
    /// Entries a rewind took out of the transcript: a prompt an interrupt caught unanswered,
    /// and its note. By position, moved on when older entries are put in front.
    pub hidden: std::collections::HashSet<usize>,
    /// The relay has more before the first entry (`more`), and it has been asked for and not
    /// yet come (`old`).
    pub more: bool,
    pub asking: bool,
    /// Counts every op applied and every stretch put in front: with the width and what the
    /// client changes itself, what the transcript's layout is cached under.
    changes: u64,
    layout: std::cell::RefCell<Option<(LayoutKey, std::rc::Rc<Vec<Line>>)>>,
}

/// What the transcript's layout depends on, besides the time.
#[derive(Debug, Clone, PartialEq)]
struct LayoutKey {
    changes: u64,
    w: usize,
    expanded: bool,
    entries: usize,
    hidden: usize,
    state: String,
}

/// A prompt sent and not yet echoed.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub text: String,
    pub since: Instant,
    /// Sent while a turn ran, so it waits behind it: Claude Code offers send-now under it.
    pub queued: bool,
    /// The uuid it went to claude with, which `back` names and Up withdraws it by.
    pub uuid: String,
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
    /// An interrupt caught a prompt before anything answered it, with nothing waiting
    /// behind: it is out of the transcript, and its text goes back to the input, as Claude
    /// Code hands it back.
    Back(String),
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

    /// A prompt just sent: shown at once, turning, until claude echoes it.
    /// Returns the uuid to send it with: the one claude's lifecycle lines will name.
    pub fn sent(&mut self, text: &str, now: Instant) -> String {
        let queued = self.state != "idle" || !self.pending.is_empty();
        let uuid = crate::launch::fresh_session_id();
        self.pending.push(Pending {
            text: text.to_string(),
            since: now,
            queued,
            uuid: uuid.clone(),
        });
        uuid
    }

    /// A prompt sent as `full` that stood in the input box as `typed`, its pastes collapsed:
    /// drawn as `typed`, waiting and once claude has it.
    pub fn collapsed(&mut self, full: &str, typed: &str) {
        if full.trim() != typed.trim() {
            self.changes += 1;
            self.shown
                .insert(full.trim().to_string(), typed.trim().to_string());
        }
    }

    /// How a prompt is drawn: as it stood in the input box, if this view sent it so.
    pub fn display<'a>(&'a self, text: &'a str) -> &'a str {
        self.shown.get(text.trim()).map_or(text, String::as_str)
    }

    /// Up in an empty prompt with prompts waiting behind the turn: they leave the queue, as
    /// Claude Code takes them back to edit. Their texts, and the uuids to withdraw them by.
    pub fn unqueue(&mut self) -> (Vec<String>, Vec<String>) {
        let (back, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| p.queued);
        self.pending = keep;
        back.into_iter().map(|p| (p.text, p.uuid)).unzip()
    }

    /// `older`'s entries in front of these.
    fn prepend(&mut self, older: Convo) {
        self.changes += 1;
        let k = older.entries.len();
        for at in self.index.values_mut() {
            *at += k;
        }
        self.hidden = self
            .hidden
            .iter()
            .map(|h| h + k)
            .chain(older.hidden)
            .collect();
        self.index.extend(older.index);
        let mut entries = older.entries;
        entries.append(&mut self.entries);
        self.entries = entries;
    }

    /// Something the client adds itself, such as the `❯ /model` it ran.
    pub fn local(&mut self, e: Entry) {
        self.entries.push(e);
    }

    /// The entry drawn for item `id`.
    pub fn entry(&self, id: u64) -> Option<&Entry> {
        self.index.get(&id).and_then(|&i| self.entries.get(i))
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
    /// Claude's responses, oldest first: each turn's text blocks joined by a blank line, as
    /// Claude Code's `/copy` takes a response, tool calls between them left out.
    pub fn responses(&self) -> Vec<String> {
        let mut out: Vec<Vec<&str>> = Vec::new();
        let mut open = false;
        for e in &self.entries {
            match e {
                Entry::Prompt(_) => open = false,
                Entry::Text(t) if !t.trim().is_empty() => {
                    if !open {
                        out.push(Vec::new());
                        open = true;
                    }
                    if let Some(g) = out.last_mut() {
                        g.push(t.trim_end());
                    }
                }
                _ => {}
            }
        }
        out.into_iter().map(|g| g.join("\n\n")).collect()
    }

    pub fn apply(&mut self, v: &Value, now: Instant) -> Applied {
        self.changes += 1;
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        match s(v, "t").as_str() {
            "add" => {
                let e = match s(v, "k").as_str() {
                    "user" => {
                        let x = s(v, "x");
                        if let Some(i) = self.pending.iter().position(|p| p.text.trim() == x.trim())
                        {
                            self.pending.remove(i);
                        }
                        Entry::Prompt(x)
                    }
                    "text" if v.get("sm").and_then(Value::as_bool) == Some(true) => {
                        Entry::Narration(s(v, "x"))
                    }
                    "text" => Entry::Text(s(v, "x")),
                    "note" => {
                        if v.get("tn").and_then(Value::as_bool) == Some(true) {
                            self.notices += 1;
                        }
                        Entry::Note(s(v, "x"))
                    }
                    "compact" => Entry::Compacted,
                    "msg" => Entry::Message {
                        from: s(v, "n"),
                        body: s(v, "x"),
                        inline: v.get("i").and_then(Value::as_bool) == Some(true),
                    },
                    "tool" => Entry::Tool {
                        name: s(v, "n"),
                        arg: s(v, "a"),
                        result: None,
                        summary: None,
                    },
                    "agent" => Entry::Agent {
                        name: s(v, "n"),
                        arg: s(v, "a"),
                        lines: Vec::new(),
                        state: "running".into(),
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
            "tasks" => {
                let num = |t: &Value, k: &str| t.get(k).and_then(Value::as_u64);
                self.tasks = v
                    .get("l")
                    .and_then(Value::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(|t| Task {
                        id: s(t, "i"),
                        kind: s(t, "k"),
                        description: s(t, "d"),
                        detail: s(t, "c"),
                        prompt: s(t, "p"),
                        started_ms: num(t, "t0"),
                        ended_ms: num(t, "t1"),
                        status: s(t, "st"),
                        item: num(t, "it"),
                        model: s(t, "m"),
                        tokens: num(t, "tk").unwrap_or(0),
                        tools: num(t, "tc").unwrap_or(0),
                        activity: s(t, "ac"),
                        agents_done: num(t, "ad").unwrap_or(0),
                        agents: num(t, "an").unwrap_or(0),
                        saveable: t.get("sv").and_then(Value::as_bool) == Some(true),
                    })
                    .filter(|t| !t.id.is_empty())
                    .collect();
            }
            "sum" => {
                if let Some(Entry::Tool { summary, .. }) = self.at(id) {
                    let group = v.get("g").and_then(Value::as_u64).unwrap_or(id);
                    *summary = Some((s(v, "x"), group));
                }
            }
            "prog" => {
                if let Some(Entry::Agent { lines, state, .. }) = self.at(id) {
                    *lines = v
                        .get("p")
                        .and_then(Value::as_arr)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect();
                    *state = s(v, "st");
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
            "back" => {
                let text = s(v, "x");
                let queued = v.get("q").and_then(Value::as_bool) == Some(true);
                // Never echoed: it is still on the phone, spinning. By its uuid where the relay
                // says it, by its text from one that does not.
                let uuid = s(v, "u");
                let mine = |p: &Pending| {
                    if uuid.is_empty() {
                        p.text.trim() == text.trim()
                    } else {
                        p.uuid == uuid
                    }
                };
                if let Some(i) = self.pending.iter().position(mine) {
                    self.pending.remove(i);
                    return if self.live && !queued {
                        Applied::Back(text)
                    } else {
                        Applied::Nothing
                    };
                }
                if queued {
                    return Applied::Nothing;
                }
                // Echoed, and nothing answered it: what came after it is notes alone.
                let Some(p) = self
                    .entries
                    .iter()
                    .rposition(|e| matches!(e, Entry::Prompt(_)))
                else {
                    return Applied::Nothing;
                };
                let answered = self.entries[p + 1..]
                    .iter()
                    .any(|e| !matches!(e, Entry::Note(_) | Entry::Worked(_)));
                if answered
                    || !matches!(&self.entries[p], Entry::Prompt(t) if t.trim() == text.trim())
                {
                    return Applied::Nothing;
                }
                self.hidden.extend(p..self.entries.len());
                return if self.live {
                    Applied::Back(text)
                } else {
                    Applied::Nothing
                };
            }
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
                    chosen: s(v, "ch"),
                    mode: s(v, "pm"),
                    title: s(v, "ti"),
                    effort: s(v, "e"),
                    cwd: s(v, "cwd"),
                    version: s(v, "ver"),
                    commands: v
                        .get("sc")
                        .and_then(Value::as_arr)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect(),
                    five_hour: window(v.get("u5")),
                    seven_day: window(v.get("u7")),
                };
            }
            // Prompts on their way that this view did not see sent, or no longer remembers:
            // drawn as its own would be, until claude echoes them.
            "wait" => {
                for p in v.get("p").and_then(Value::as_arr).unwrap_or(&[]) {
                    let uuid = s(p, "u");
                    if self.pending.iter().any(|q| q.uuid == uuid) {
                        continue;
                    }
                    self.pending.push(Pending {
                        text: s(p, "x"),
                        since: now,
                        queued: p.get("q").and_then(Value::as_bool) == Some(true),
                        uuid,
                    });
                }
            }
            "more" => {
                self.more = v.get("m").and_then(Value::as_bool) == Some(true);
                self.asking = false;
            }
            // The stretch before the first entry: drawn above it, the view staying where it is.
            "old" => {
                let mut older = Convo::new(&self.host, &self.slot, &self.run);
                for op in v.get("o").and_then(Value::as_arr).unwrap_or(&[]) {
                    older.apply(op, now);
                }
                self.prepend(older);
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

/// One step of a run of tool calls: claude's summary of it and its first item, or `None`
/// for calls not summarised, which are counted.
struct Step<'a> {
    summary: Option<(&'a str, u64)>,
    tools: Vec<(&'a str, &'a str)>,
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

/// The assistant's text as Claude Code draws it: `● `, then its markdown, kept clear of the
/// last two columns as `wrapped` keeps them.
fn assistant_text(text: &str, w: usize) -> Vec<Line> {
    crate::markdown::render(text, w.saturating_sub(4))
        .into_iter()
        .enumerate()
        .map(|(n, l)| {
            let lead = if n == 0 { "● " } else { "  " };
            let mut row: Line = vec![(lead.into(), PLAIN)];
            row.extend(l);
            row
        })
        .collect()
}

/// A tool call expanded, as Claude Code's Ctrl+O transcript draws one: `● Bash(ls)`, then
/// the first lines of what it said and how many more, grey; a failure says so in words.
fn expanded_tool(
    name: &str,
    arg: &str,
    result: Option<&(bool, Vec<String>, usize)>,
    w: usize,
) -> Vec<Line> {
    // As Claude Code marks a call: green once it worked, red when it failed.
    let dot = match result {
        None => PLAIN,
        Some((true, ..)) => fg(GREEN),
        Some((false, ..)) => fg(RED),
    };
    let mut out = titled(name, arg, dot, w);
    let under_st = if dot == fg(RED) { fg(RED) } else { fg(GREY) };
    let mut under: Vec<String> = Vec::new();
    match result {
        None => under.push("Running…".into()),
        Some((ok, preview, lines)) => {
            if !ok {
                under.push("Error".into());
            }
            under.extend(preview.iter().cloned());
            if *lines > preview.len() {
                under.push(format!("… +{} lines", lines - preview.len()));
            }
            if under.is_empty() {
                under.push("(No output)".into());
            }
        }
    }
    for (i, text) in under.iter().enumerate() {
        let lead = if i == 0 { "  ⎿  " } else { "     " };
        out.extend(wrapped_by(
            text,
            lead,
            "     ",
            w.saturating_sub(1),
            under_st,
        ));
    }
    out
}

/// `● Name(arg)`, wrapped, the dot in `dot` and the name in bold, as Claude Code titles a
/// tool call.
fn titled(name: &str, arg: &str, dot: St, w: usize) -> Vec<Line> {
    let head = if arg.is_empty() {
        name.to_string()
    } else {
        format!("{name}({arg})")
    };
    let mut out = wrapped(&head, "● ", "  ", w, PLAIN);
    if let Some(l) = out.first_mut() {
        let text = plain(l);
        let body = text.strip_prefix("● ").unwrap_or(&text).to_string();
        let (n, rest) = body.split_at(name.len().min(body.len()));
        let mut row: Line = vec![
            ("● ".into(), dot),
            (
                n.to_string(),
                St {
                    bold: true,
                    ..PLAIN
                },
            ),
        ];
        if !rest.is_empty() {
            row.push((rest.to_string(), PLAIN));
        }
        *l = row;
    }
    out
}

/// A subagent or workflow as Claude Code draws one: `● Explore(what it was asked)`, and
/// under it what it is doing or how it ended. The dot is green once it has finished well,
/// red when it failed or was stopped, which the line under it says in words too.
fn agent_block(name: &str, arg: &str, lines: &[String], state: &str, w: usize) -> Vec<Line> {
    let dot = match state {
        "completed" => fg(GREEN),
        "running" => PLAIN,
        _ => fg(RED),
    };
    let mut out = titled(name, arg, dot, w);
    for (i, text) in lines.iter().enumerate() {
        let lead = if i == 0 { "  ⎿  " } else { "     " };
        out.extend(wrapped_by(
            text,
            lead,
            "     ",
            w.saturating_sub(1),
            fg(GREY),
        ));
    }
    out
}

/// A message another agent sent, as Claude Code draws one (2.1.311): collapsed, one grey
/// line, `› Message from @Explore (ctrl+o to expand)`, a peer session's words on it after a
/// colon; expanded, `› Message from Explore` and what it said under it.
fn message_block(from: &str, body: &str, inline: bool, expanded: bool, w: usize) -> Vec<Line> {
    if expanded {
        let mut out = wrapped_by(&format!("› Message from {from}"), "", "", w, fg(GREY));
        out.extend(wrapped_by(body, "  ", "  ", w, PLAIN));
        return out;
    }
    let said = if inline {
        let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
        format!(": {flat}")
    } else {
        String::new()
    };
    let head = format!("› Message from @{from}");
    let hint = " (ctrl+o to expand)";
    // The words are cut to keep the line to one row, as Claude Code keeps it to a line.
    let said = cut(&said, w.saturating_sub(width(&head) + width(hint)));
    let said = if said == "…" { String::new() } else { said };
    wrapped_by(&format!("{head}{said}{hint}"), "", "", w, fg(GREY))
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

/// `/effort`'s stops: `auto`, the model's own default, which is where a session with no
/// effort set sits, then the levels.
pub const EFFORT_CHOICES: [&str; 6] = ["auto", "low", "medium", "high", "xhigh", "max"];

/// Where `/effort` starts: the session's effort, or `auto` when none is set.
pub fn effort_index(meta: &Meta) -> usize {
    EFFORT_CHOICES
        .iter()
        .position(|e| *e == meta.effort)
        .unwrap_or(0)
}

/// The session's own entry in `/model`'s list: the alias it was given when that is known,
/// else the entry its model resolves to. One entry at most, so `/model` ticks one.
pub fn current_model(meta: &Meta, list: &[(String, String)]) -> Option<usize> {
    if !meta.chosen.is_empty() {
        if let Some(i) = list.iter().position(|(v, _)| *v == meta.chosen) {
            return Some(i);
        }
    }
    list.iter().position(|(v, d)| is_current(&meta.model, v, d))
}

/// Claude Code's effort glyphs (read from 2.1.311).
pub fn effort_glyph(level: &str) -> &'static str {
    match level {
        "low" => "○",
        "medium" => "◐",
        "high" => "●",
        _ => "◉",
    }
}

/// The mode line, as Claude Code words it, with `/mode` where it says shift+tab; the default
/// mode is `manual mode on`, grey and with no hint, as 2.1.311 draws it (driven 2026-11-07).
pub fn mode_line(mode: &str) -> Option<(String, u32)> {
    // Each in the colour Claude Code gives it: auto `warning`, accept edits `autoAccept`,
    // bypass `error`, plan `planMode`, manual its `inactive` grey.
    let (glyph, words, colour) = match mode {
        "default" => return Some(("⏸ manual mode on".into(), MANUAL_GREY)),
        "auto" => ("⏵⏵", "auto mode on", YELLOW),
        "acceptEdits" => ("⏵⏵", "accept edits on", PURPLE),
        "bypassPermissions" => ("⏵⏵", "bypass permissions on", RED),
        "plan" => ("⏸", "plan mode on", TEAL),
        _ => return None,
    };
    Some((format!("{glyph} {words}{MODE_HINT}"), colour))
}

/// What the mode line says after a mode other than manual, where Claude Code says
/// `(shift+tab to cycle)`.
pub const MODE_HINT: &str = " (/mode to change)";

/// Claude Code's grey for `manual mode on`, a shade off its secondary text's.
const MANUAL_GREY: u32 = 0x94_9494;

/// `/mode`'s choices, in Claude Code's shift+tab order: the mode claude takes, the name the
/// picker gives it, and the word `/mode WORD` takes for it.
pub const MODES: [(&str, &str, &str); 4] = [
    ("default", "Manual", "manual"),
    ("acceptEdits", "Accept edits", "accept"),
    ("plan", "Plan", "plan"),
    ("auto", "Auto", "auto"),
];

/// The mode `/mode WORD` names: the picker's word, its name, or claude's own.
pub fn mode_named(word: &str) -> Option<&'static str> {
    let w = word.to_lowercase();
    MODES.iter().find_map(|(mode, name, short)| {
        (w == *short || w == name.to_lowercase() || w == mode.to_lowercase()).then_some(*mode)
    })
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
    /// `/mode`: the cursor in `MODES`.
    Mode { sel: usize },
    /// `/copy` of a response with code blocks: what to copy, each a label and its text, the
    /// whole response first.
    Copy {
        sel: usize,
        choices: Vec<(String, String)>,
    },
    /// `/tasks`, or Down then Enter on the pill: claude's background tasks (`tasks.rs`).
    Tasks(crate::tasks::View),
    /// `/btw`: the side question, its answer once it has come (or why there is none), and
    /// how far the answer is scrolled.
    Btw {
        question: String,
        answer: Option<Result<String, String>>,
        top: usize,
    },
}

/// A response's fenced code blocks, each its language (empty when the fence names none) and
/// its body. An unclosed fence runs to the end, as markdown has it.
pub fn code_blocks(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut open: Option<(String, String, Vec<&str>)> = None;
    for l in text.lines() {
        let t = l.trim_start();
        let fence = ["```", "~~~"].into_iter().find(|f| t.starts_with(f));
        match (&mut open, fence) {
            (None, Some(f)) => {
                open = Some((f.to_string(), t[3..].trim().to_string(), Vec::new()));
            }
            (Some((f, _, _)), Some(g)) if *f == g && t.trim_end() == g => {
                if let Some((_, lang, body)) = open.take() {
                    out.push((lang, body.join("\n")));
                }
            }
            (Some((_, _, body)), _) => body.push(l),
            (None, None) => {}
        }
    }
    if let Some((_, lang, body)) = open {
        out.push((lang, body.join("\n")));
    }
    out
}

/// What `/copy` offers for a response: the whole of it, then each code block, labelled by
/// its language and first line.
pub fn copy_choices(response: &str) -> Vec<(String, String)> {
    let mut out = vec![("Full response".to_string(), response.to_string())];
    for (lang, body) in code_blocks(response) {
        let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let lang = if lang.is_empty() { "code" } else { &lang };
        out.push((format!("{lang}: {}", first.trim()), body));
    }
    out
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

/// What Down from an empty prompt has focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Prompt,
    /// The tasks pill on the mode line.
    Pill,
    /// A row of the agent strip: 0 `main`, then its agents.
    Strip(usize),
}

impl Ui {
    /// The wall clock, epoch milliseconds.
    pub fn now_ms(&self) -> u64 {
        self.wall.unwrap_or_else(crate::clock::now)
    }
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
    /// The status line an ssh session's Claude Code shows, `RAM: 1.2G / 2.0G, Load: 2.3,
    /// claude-sessions`: the same figures, from the hub's and the containers' snapshots.
    pub load: Option<crate::statusline::Figures>,
    /// A word in place of the footer's first line until the next key: `/copy`'s, an error.
    pub status: Option<String>,
    /// Ctrl+X was pressed: the first half of a chord, Ctrl+X Ctrl+S being send-now.
    pub chord: bool,
    /// When Left was pressed on an empty prompt: a second within `LEFT_AGAIN`, no other key
    /// between, goes home, as Ctrl+\\ does, Claude Code's double Left to its agents view.
    pub left: Option<std::time::Instant>,
    /// Pastes standing in the prompt as `[Pasted text #N +L lines]`, by number, as Claude
    /// Code's are: put back whole when the prompt is sent.
    pub pastes: Vec<(u32, String)>,
    /// claude's slash commands with what each does, for the suggestion list (`suggestions`).
    pub catalog: Vec<Command>,
    /// The suggestion list's selected entry, and whether Esc put the list away until the
    /// prompt next changes.
    pub suggest: usize,
    pub suggest_off: bool,
    /// What Down from an empty prompt has focused, as in Claude Code: the tasks pill, or a
    /// row of the agent strip under the footer.
    pub focus: Focus,
    /// A word on the right above the prompt until the next key: `Shell details dismissed`.
    pub toast: Option<String>,
    /// The wall clock, epoch milliseconds, for a test to fix; the real one otherwise.
    pub wall: Option<u64>,
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            input: String::new(),
            cursor: 0,
            scroll: 0,
            dialog: None,
            link: Link::Up,
            load: None,
            status: None,
            chord: false,
            left: None,
            pastes: Vec::new(),
            catalog: Vec::new(),
            suggest: 0,
            suggest_off: false,
            focus: Focus::Prompt,
            toast: None,
            wall: None,
        }
    }
}

impl Default for Ui {
    fn default() -> Ui {
        Ui::new()
    }
}

// ── drawing ─────────────────────────────────────────────────────────────────

/// A prompt on its band, after `lead`: `❯ ` once claude has it, the spinner until then.
fn prompt_lines(text: &str, lead: &str, w: usize) -> Vec<Line> {
    let band = St {
        bg: Some(BAND),
        ..PLAIN
    };
    wrapped(text, lead, "  ", w, band)
        .into_iter()
        .map(|mut l| {
            let pad = w.saturating_sub(width(&plain(&l)));
            l.push((" ".repeat(pad), band));
            l
        })
        .collect()
}

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
    // Seen only at the top of what has come: the rest is on its way as it is scrolled to.
    if c.more {
        out.push(Vec::new());
        out.push(line("  Loading earlier messages…", fg(GREY)));
    }
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
        if c.hidden.contains(&i) {
            i += 1;
            continue;
        }
        let e = &c.entries[i];
        match e {
            Entry::Tool {
                name, arg, result, ..
            } if c.expanded => {
                i += 1;
                if name == ASK_TOOL {
                    continue;
                }
                out.push(Vec::new());
                out.extend(expanded_tool(name, arg, result.as_ref(), w));
                continue;
            }
            Entry::Tool { .. } => {
                // The run's steps in order: those claude summarised, a line each, and those
                // it has not (yet), counted, as before summaries.
                let mut steps: Vec<Step> = Vec::new();
                let mut open: Option<(&str, &str)> = None;
                while let Some(Entry::Tool {
                    name,
                    arg,
                    result,
                    summary,
                }) = c.entries.get(i)
                {
                    // Claude Code draws a question as what was answered, not as a tool call.
                    if name != ASK_TOOL {
                        if result.is_none() {
                            open = Some((name, arg));
                        }
                        let key = summary.as_ref().map(|(x, g)| (x.as_str(), *g));
                        match steps.last_mut() {
                            Some(st) if st.summary == key => st.tools.push((name, arg)),
                            _ => steps.push(Step {
                                summary: key,
                                tools: vec![(name, arg)],
                            }),
                        }
                    }
                    i += 1;
                    // Permission prompts between tool calls belong to the run.
                    while let Some(Entry::Perm { .. }) = c.entries.get(i) {
                        i += 1;
                    }
                }
                if steps.is_empty() {
                    continue;
                }
                let last = i >= c.entries.len();
                out.push(Vec::new());
                // Still running: the calls not yet summarised are said as under way, with
                // the one running under them.
                let running = open
                    .filter(|_| last && busy)
                    .map(|call| match steps.last() {
                        Some(st) if st.summary.is_none() => (call, steps.len() - 1),
                        _ => (call, 0),
                    });
                let done = running.map_or(steps.len(), |(_, from)| from);
                for st in &steps[..done] {
                    let head = match st.summary {
                        Some((x, _)) => x.to_string(),
                        None => tools_summary(&st.tools, false),
                    };
                    out.extend(wrapped(&head, "  ", "  ", w, fg(GREY)));
                }
                if let Some(((name, arg), from)) = running {
                    let tools: Vec<(&str, &str)> = steps[from..]
                        .iter()
                        .flat_map(|st| st.tools.clone())
                        .collect();
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
                continue;
            }
            Entry::Prompt(text) => {
                out.push(Vec::new());
                last_prompt = Some(out.len());
                out.extend(prompt_lines(c.display(text), "❯ ", w));
            }
            Entry::Text(text) => {
                out.push(Vec::new());
                out.extend(assistant_text(text, w));
            }
            // As Claude Code 2.1.311 draws one for a model without the hint turned off:
            // `· summary` in its `subtle` grey after the text (a no-break space there, a plain
            // one here: the same on a terminal).
            Entry::Narration(text) => {
                out.push(Vec::new());
                let mut rows = assistant_text(text, w);
                const HINT: &str = " · summary";
                match rows.last_mut() {
                    Some(last) if width(&plain(last)) + width(HINT) <= w.saturating_sub(2) => {
                        last.push((HINT.into(), fg(GREY)));
                    }
                    _ => rows.push(vec![("  ".into(), PLAIN), (HINT[1..].into(), fg(GREY))]),
                }
                out.extend(rows);
            }
            // Under what it interrupted, as Claude Code draws it, and in its words.
            Entry::Note(text) if text == "Interrupted" => {
                out.extend(wrapped(
                    "Interrupted · What should Claude do instead?",
                    "  ⎿  ",
                    "     ",
                    w,
                    fg(RED),
                ));
            }
            Entry::Note(text) => {
                out.push(Vec::new());
                // An error, or a turn interrupted, in Claude Code's `error` red; anything
                // else a local command said, grey.
                let st = if text.starts_with("Error") || text.starts_with("Interrupted") {
                    fg(RED)
                } else {
                    fg(GREY)
                };
                out.extend(wrapped(text, "  ⎿  ", "     ", w, st));
            }
            Entry::Agent {
                name,
                arg,
                lines,
                state,
            } => {
                out.push(Vec::new());
                out.extend(agent_block(name, arg, lines, state, w));
            }
            Entry::Message { from, body, inline } => {
                out.push(Vec::new());
                out.extend(message_block(from, body, *inline, c.expanded, w));
            }
            Entry::Compacted => {
                out.push(Vec::new());
                out.push(line("✻ Conversation compacted", fg(GREY)));
            }
            Entry::Worked(text) => {
                out.push(Vec::new());
                // `· 2 shells, 1 monitor still running` while they run, as Claude Code
                // rewrites every turn's end line as tasks end.
                let text = match crate::tasks::still_running(&c.tasks) {
                    Some(r) => format!("{text} · {r}"),
                    None => text.clone(),
                };
                out.extend(wrapped(&text, "✻ ", "  ", w, fg(GREY)));
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

pub(crate) fn rule(w: usize, glyph: &str, st: St) -> Line {
    line(glyph.repeat(w), st)
}

/// The ssh sessions' status line (`statusline.rs`), its figures in its words: a figure past
/// its first threshold yellow and past its second red, as there; the words say it all
/// without them. After the conversation's `container/slot` where that fits (R1 at 80), else
/// as an ssh session prints it, else its figures alone, so a phone keeps the load in view.
fn status_line(place: &str, f: &crate::statusline::Figures, w: usize) -> Line {
    use crate::statusline::Level;
    let paint = |lvl: Level| match lvl {
        Level::High => fg(RED),
        Level::Warn => fg(YELLOW),
        Level::Calm => fg(GREY),
    };
    let (ram, rl) = f.ram();
    let (load, ll) = f.load();
    let figures = width(&ram) + 2 + width(&load);
    let room = w.saturating_sub(3);
    let (before, after) = if width(place) + 3 + figures <= room {
        (format!("{place} · "), String::new())
    } else if figures + 2 + width(f.place()) <= room {
        (String::new(), format!(", {}", f.place()))
    } else {
        (String::new(), String::new())
    };
    let parts = [
        (format!("  {before}"), fg(GREY)),
        (ram, paint(rl)),
        (", ".into(), fg(GREY)),
        (load, paint(ll)),
        (after, fg(GREY)),
    ];
    let mut out: Line = Vec::new();
    let mut left = w.saturating_sub(1);
    for (t, st) in parts {
        let t = if width(&t) > left { cut(&t, left) } else { t };
        left -= width(&t).min(left);
        if !t.is_empty() {
            out.push((t, st));
        }
    }
    out
}

/// A slash command as the suggestion list shows it: its name without the `/`, what it does,
/// and the argument hint drawn after it once chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub about: String,
    pub hint: String,
}

/// The commands this client serves that claude's list may not have, said as the phone does
/// them.
const CLIENT_ABOUT: [(&str, &str); 14] = [
    ("model", "Set the AI model for Claude Code"),
    ("effort", "Set the effort level"),
    ("rename", "Rename the conversation"),
    (
        "copy",
        "Copy Claude's last response to clipboard (or /copy N for the Nth-latest)",
    ),
    ("exit", "End the session"),
    (
        "mode",
        "Choose the permission mode (shift+tab in Claude Code)",
    ),
    ("home", "Back to the home; the session keeps running"),
    ("disconnect", "Back to the home; the session keeps running"),
    ("expand", "Show every tool call with its result (ctrl+o)"),
    ("quit", "End the session"),
    ("btw", "Ask a side question without interrupting"),
    (
        "tasks",
        "View and manage everything running in the background",
    ),
    (
        "remote-control",
        "Control this session from your phone or claude.ai/code",
    ),
    (
        "rc",
        "Control this session from your phone or claude.ai/code",
    ),
];

/// What the suggestion list offers for the prompt now: while its first word is a `/` and the
/// start of a name, the command of that exact name, then claude's commands and this client's
/// that start with it, then those that have it further in, each in claude's order. Nothing once a space is typed, in a dialog, or
/// after Esc.
pub fn suggestions(ui: &Ui) -> Vec<Command> {
    let Some(query) = ui.input.strip_prefix('/') else {
        return Vec::new();
    };
    if ui.suggest_off || ui.dialog.is_some() || query.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut all = ui.catalog.clone();
    for (name, about) in CLIENT_ABOUT {
        if !all.iter().any(|c| c.name == name) {
            all.push(Command {
                name: name.into(),
                about: about.into(),
                hint: String::new(),
            });
        }
    }
    let q = query.to_lowercase();
    let (starts, rest): (Vec<Command>, Vec<Command>) = all
        .into_iter()
        .filter(|c| c.name.to_lowercase().contains(&q))
        .partition(|c| c.name.to_lowercase().starts_with(&q));
    // A name typed whole comes first: claude's `auto-mode-setup` and `autocompact`, two lines
    // each on a phone, otherwise filled the list and hid `/auto` under them.
    let (exact, starts): (Vec<Command>, Vec<Command>) =
        starts.into_iter().partition(|c| c.name.to_lowercase() == q);
    exact.into_iter().chain(starts).chain(rest).collect()
}

/// The suggestion list, as Claude Code draws it over the prompt: the name in a column 40% of
/// the width (cut from the left when long), what it does beside it in two lines at most, the
/// chosen one `❯` and lavender, the rest grey; five lines at most, scrolled to the chosen one.
fn suggest_lines(list: &[Command], sel: usize, w: usize) -> Vec<Line> {
    const MOST: usize = 5;
    let col = (w * 2 / 5).max(8);
    let room = w.saturating_sub(4 + col + 2).max(8);
    let entry = |c: &Command, chosen: bool| -> Vec<Line> {
        let st = if chosen { fg(LAVENDER) } else { fg(GREY) };
        let mut name = format!("/{}", c.name);
        // Two spaces at least before what it does: a longer name keeps its end.
        if width(&name) > col - 2 {
            let keep = col - 3;
            let tail: String = name.chars().skip(name.chars().count() - keep).collect();
            name = format!("…{tail}");
        }
        let lead = if chosen { "  ❯ " } else { "    " };
        let pad = " ".repeat(col - width(&name));
        let about = c.about.split_whitespace().collect::<Vec<_>>().join(" ");
        let lines = wrap(&about, room);
        let first = lines.first().cloned().unwrap_or_default();
        let rest = lines[1.min(lines.len())..].join(" ");
        let mut out = vec![line(format!("{lead}{name}{pad}{first}"), st)];
        if !rest.is_empty() {
            out.push(line(
                format!("{}{}", " ".repeat(4 + col), cut(&rest, room)),
                st,
            ));
        }
        out
    };
    let sel = sel.min(list.len().saturating_sub(1));
    // From the chosen one back, as many whole entries as fit; then on down while they fit.
    let mut start = sel;
    let mut used = entry(&list[sel], true).len();
    while start > 0 {
        let n = entry(&list[start - 1], false).len();
        if used + n > MOST {
            break;
        }
        used += n;
        start -= 1;
    }
    let mut out = Vec::new();
    for (i, c) in list.iter().enumerate().skip(start) {
        let e = entry(c, i == sel);
        if out.len() + e.len() > MOST {
            break;
        }
        out.extend(e);
    }
    out
}

/// The slash commands this client serves itself (`client::submit`); claude's own come in its
/// meta.
pub const CLIENT_COMMANDS: [&str; 14] = [
    "model",
    "effort",
    "mode",
    "rename",
    "exit",
    "quit",
    "home",
    "disconnect",
    "expand",
    "btw",
    "copy",
    "rc",
    "remote-control",
    "tasks",
];

/// Where `chars` names a slash command: `/` at the start or after whitespace, then a name
/// claude or this client knows, up to whitespace. A `/` before an unknown name is text.
pub fn slash_runs(chars: &[char], commands: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i] == '/' && (i == 0 || chars[i - 1].is_whitespace());
        if !starts {
            i += 1;
            continue;
        }
        let end = (i + 1..chars.len())
            .find(|&k| chars[k].is_whitespace())
            .unwrap_or(chars.len());
        let name: String = chars[i + 1..end].iter().collect();
        if !name.is_empty()
            && (commands.contains(&name) || CLIENT_COMMANDS.contains(&name.as_str()))
        {
            out.push(i..end);
        }
        i = end;
    }
    out
}

/// Typed text wrapped at words to `room` cells, as an editor soft-wraps: a row breaks after
/// the spaces before a word that does not fit, the spaces hanging at its end, so no row but
/// the first of a line starts with one; a word wider than a row is broken; a newline ends a
/// row. Returns each row's `[start, end)` in `chars`, and the cursor's row and column — on the
/// next row when it sits where a row was wrapped, and on a row of its own after a full one.
pub fn soft_wrap(
    chars: &[char],
    cursor: usize,
    room: usize,
) -> (Vec<(usize, usize)>, (usize, usize)) {
    let room = room.max(1);
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut col = 0;
    // Where this row may break: just after its last run of spaces.
    let mut brk: Option<usize> = None;
    loop {
        match chars.get(i) {
            None => {
                rows.push((start, i));
                break;
            }
            Some('\n') => {
                rows.push((start, i));
                i += 1;
                start = i;
                col = 0;
                brk = None;
            }
            Some(' ') => {
                i += 1;
                col += 1;
                brk = Some(i);
            }
            Some(&c) if col + cw(c) > room && i > start => {
                let at = brk.filter(|&b| b > start && b <= i).unwrap_or(i);
                rows.push((start, at));
                start = at;
                col = chars[start..i].iter().map(|&c| cw(c)).sum();
                brk = None;
            }
            Some(&c) => {
                i += 1;
                col += cw(c);
            }
        }
    }
    let cursor = cursor.min(chars.len());
    let last = rows.len() - 1;
    let mut at = (last, 0);
    for (r, &(a, b)) in rows.iter().enumerate() {
        let ends_line = r == last || chars.get(b) == Some(&'\n');
        if cursor >= a && (cursor < b || (cursor == b && ends_line)) {
            let col: usize = chars[a..cursor].iter().map(|&c| cw(c)).sum();
            at = (r, col.min(room));
            break;
        }
    }
    // A cursor after a full row starts the next one, as typing there would.
    if at.1 >= room {
        let (_, b) = rows[at.0];
        if at.0 == last && cursor == b {
            rows.push((b, b));
            at = (at.0 + 1, 0);
        } else {
            at.1 = room - 1;
        }
    }
    (rows, at)
}

/// The link is down: said after the status line where both fit, the figures whole, and in its
/// place where they do not — at 40 columns, where the figures alone take most of the line.
fn reconnecting_line(
    place: &str,
    load: Option<&crate::statusline::Figures>,
    down: &str,
    w: usize,
) -> Line {
    let said = |mut l: Line| {
        l.push((down.to_string(), PLAIN));
        l
    };
    if let Some(f) = load {
        let room = w.saturating_sub(width(down) + 3);
        let figures = status_line(place, f, room);
        let (ram, _) = f.ram();
        let (load, _) = f.load();
        let whole = width(&plain(&figures)) >= 2 + width(&ram) + 2 + width(&load)
            && !plain(&figures).ends_with('…');
        if whole {
            let mut l = figures;
            l.push((" · ".into(), fg(GREY)));
            return said(l);
        }
    }
    said(vec![("  ".into(), PLAIN)])
}

/// What the prompt says, taking nothing, while an agent is in view: Claude Code's prompt there
/// messages the agent (`Message @general-purpose…`), and headless claude refuses to
/// (`send_task_message`, 2.1.311).
pub const AGENT_READ_ONLY: &str = "headless claude can't message agents";

/// A line cut to `w` cells with no ellipsis.
fn clip(l: Line, w: usize) -> Line {
    let mut used = 0;
    let mut out = Vec::new();
    for (t, st) in l {
        let mut keep = String::new();
        for ch in t.chars() {
            if used + cw(ch) > w {
                break;
            }
            used += cw(ch);
            keep.push(ch);
        }
        if !keep.is_empty() {
            out.push((keep, st));
        }
    }
    out
}

/// A usage-limit window from the meta op, `{p, r}`; anything else is no reading.
fn window(v: Option<&Value>) -> Option<crate::sdk::Window> {
    let v = v?;
    Some(crate::sdk::Window {
        pct: u8::try_from(v.get("p")?.as_u64()?).ok()?.min(100),
        resets_at: v.get("r")?.as_u64()?,
    })
}

/// The footer's usage, `5h 12% · 7d 72%`: each window claude has read and that has not
/// reset since, so an old reading is not drawn as now's.
/// The session's model and effort, for the footer: `Opus 5.5 ● high`, or the model alone
/// while the effort is the model's own. `None` until the model is known, as resolved or as
/// chosen.
fn settings(m: &Meta) -> Option<String> {
    // Before claude's first `init` the model is known only as chosen.
    let model = if m.model.is_empty() {
        &m.chosen
    } else {
        &m.model
    };
    if model.is_empty() {
        return None;
    }
    let name = model_name(model);
    Some(if m.effort.is_empty() {
        name
    } else {
        format!("{name} {} {}", effort_glyph(&m.effort), m.effort)
    })
}

fn usage(m: &Meta, now_ms: u64) -> Option<String> {
    let parts: Vec<String> = [("5h", m.five_hour), ("7d", m.seven_day)]
        .into_iter()
        .filter_map(|(k, w)| {
            w.filter(|w| w.resets_at.saturating_mul(1000) > now_ms)
                .map(|w| format!("{k} {}%", w.pct))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The input box and footer: R1's last five rows. A prompt taller than half the screen shows
/// that many rows, scrolled to keep the cursor in them, `↑`/`↓` leading a row with more past it.
fn input_box(c: &Convo, ui: &Ui, w: usize, h: usize) -> (Vec<Line>, (usize, usize)) {
    let mut out = Vec::new();
    let now_ms = ui.now_ms();
    let viewing = c.viewing.as_ref().map(|(id, _)| id.as_str());
    let agent = viewing.and_then(|id| c.tasks.iter().find(|t| t.id == id));
    // `Shell details dismissed`, on the right over the prompt, until the next key.
    if let Some(t) = &ui.toast {
        let t = cut(t, w.saturating_sub(2));
        let pad = w.saturating_sub(width(&t) + 2);
        out.push(vec![(" ".repeat(pad), PLAIN), (t, fg(GREY))]);
    }
    let title = c.meta.title.trim();
    if let Some(a) = agent {
        // An agent in view: Claude Code's teal rules, its description on the top one.
        let label = cut(&a.description, w.saturating_sub(6));
        let left = w.saturating_sub(width(&label) + 3);
        out.push(vec![
            ("─".repeat(left), fg(AGENT_TEAL)),
            (
                format!(" {label} "),
                St {
                    fg: Some(0x00_0000),
                    bg: Some(AGENT_TEAL),
                    ..PLAIN
                },
            ),
            ("─".into(), fg(AGENT_TEAL)),
        ]);
    } else if title.is_empty() || width(title) + 4 > w {
        out.push(rule(w, "─", fg(RULE)));
    } else {
        let left = w - width(title) - 3;
        out.push(vec![
            ("─".repeat(left), fg(RULE)),
            (format!(" {title} "), fg(GREY)),
            ("─".into(), fg(RULE)),
        ]);
    }
    // The prompt, soft-wrapped as Claude Code's is, with the cursor's place. With an agent
    // in view it takes nothing (`AGENT_READ_ONLY`), and what was typed for the conversation
    // waits for it.
    let room = w.saturating_sub(2).max(1);
    let chars: Vec<char> = if agent.is_some() {
        Vec::new()
    } else {
        ui.input.chars().collect()
    };
    let (rows, cur) = soft_wrap(&chars, ui.cursor.min(chars.len()), room);
    // A slash command claude or this client knows, anywhere in the prompt, in Claude Code's
    // lavender (its `permission` colour, read off its screen on 2026-11-02).
    let slash = slash_runs(&chars, &c.meta.commands);
    let first = out.len();
    let shown = (h / 2).max(1);
    let top = (cur.0 + 1).saturating_sub(shown);
    let end = (top + shown).min(rows.len());
    for (i, &(a, b)) in rows.iter().enumerate().take(end).skip(top) {
        let mut l: Line = match i {
            0 => vec![("❯ ".into(), PLAIN)],
            _ if i == top => vec![("↑ ".into(), fg(GREY))],
            _ if i + 1 == end && end < rows.len() => vec![("↓ ".into(), fg(GREY))],
            _ => vec![("  ".into(), PLAIN)],
        };
        // Spaces hanging past the edge are not drawn: they are the break.
        let mut used = 0;
        for (k, &ch) in chars[a..b].iter().enumerate() {
            used += cw(ch);
            if used > room {
                break;
            }
            let st = if slash.iter().any(|r| r.contains(&(a + k))) {
                fg(LAVENDER)
            } else {
                PLAIN
            };
            match l.last_mut() {
                Some((t, s)) if *s == st => t.push(ch),
                _ => l.push((ch.to_string(), st)),
            }
        }
        out.push(l);
    }
    // A command chosen from the list, a space after it: its argument hint, grey, as Claude
    // Code shows it.
    if let Some(cmd) = ui.input.strip_prefix('/').and_then(|r| r.strip_suffix(' ')) {
        let hint = ui
            .catalog
            .iter()
            .find(|k| k.name == cmd)
            .map(|k| k.hint.as_str());
        if let (Some(hint), Some(l)) = (hint.filter(|h| !h.is_empty()), out.last_mut()) {
            let room = w.saturating_sub(width(&plain(l)) + 2);
            l.push((format!(" {}", cut(hint, room)), fg(GREY)));
        }
    }
    if agent.is_some() {
        if let Some(l) = out.last_mut() {
            l.push((cut(AGENT_READ_ONLY, w.saturating_sub(3)), fg(GREY)));
        }
    } else if chars.is_empty() && c.pending.iter().any(|p| p.queued) {
        // What Claude Code says in an empty prompt with prompts waiting behind the turn.
        if let Some(l) = out.last_mut() {
            l.push(("Press up to edit queued messages".into(), fg(GREY)));
        }
    }
    let cursor = (first + cur.0 - top, 2 + cur.1);
    out.push(rule(
        w,
        "─",
        fg(if agent.is_some() { AGENT_TEAL } else { RULE }),
    ));
    let place = format!("{}/{}", c.host, c.slot);
    let first_foot = match (&ui.link, &ui.status, &ui.load) {
        (Link::Down { since, not_sent }, _, _) => {
            let gone = since.elapsed();
            let frame = (gone.as_millis() / 120) as usize % crate::ui::SPINNER.len();
            let tail = if *not_sent { " · not sent" } else { "" };
            let down = format!(
                "{} Reconnecting… ({}s){tail}",
                crate::ui::SPINNER[frame],
                gone.as_secs()
            );
            out.push(reconnecting_line(&place, ui.load.as_ref(), &down, w));
            None
        }
        (Link::Up, Some(word), _) => Some(word.clone()),
        (Link::Up, None, None) => Some(place.clone()),
        (Link::Up, None, Some(f)) => {
            out.push(status_line(&place, f, w));
            None
        }
    };
    if let Some(t) = first_foot {
        out.push(line(
            format!("  {}", cut(&t, w.saturating_sub(2))),
            fg(GREY),
        ));
    }
    let pill = crate::tasks::pill(&c.tasks);
    let subagents = crate::tasks::subagents_running(&c.tasks);
    // The mode line, then claude's background shells after ` · ` in cyan, as Claude Code
    // puts them, and `/tasks to see subagents` while one runs, since the pill counts shells.
    // Claude Code drops its `(shift+tab to cycle)` while a pill shows.
    let usage = usage(&c.meta, now_ms);
    let mut foot: Line = Vec::new();
    if let Focus::Strip(i) = ui.focus {
        // The strip has the focus: the mode line says its keys, as Claude Code's does.
        foot.push((
            format!(
                "  {}",
                crate::tasks::strip_hint(&c.tasks, viewing, i, now_ms)
            ),
            fg(GREY),
        ));
    } else if let Some((m, colour)) = mode_line(&c.meta.mode) {
        // The hint goes too when the usage would not fit beside it.
        let crowded = usage.as_ref().is_some_and(|u| {
            pill.is_none() && !subagents && 2 + width(&m) + 2 + width(u) > w.saturating_sub(2)
        });
        let m = if pill.is_some() || subagents || crowded {
            m.replace(MODE_HINT, "")
        } else {
            m
        };
        foot.push((format!("  {m}"), fg(colour)));
    }
    let mut extra: Vec<(String, St)> = Vec::new();
    if let Some(p) = pill {
        let st = if ui.focus == Focus::Pill {
            St {
                fg: Some(0x00_0000),
                bg: Some(crate::tasks::CYAN),
                ..PLAIN
            }
        } else {
            fg(crate::tasks::CYAN)
        };
        extra.push((p, st));
    }
    if subagents {
        extra.push(("/tasks to see subagents".into(), fg(GREY)));
    }
    for (i, run) in extra.into_iter().enumerate() {
        if i > 0 || !foot.is_empty() {
            foot.push((" · ".into(), fg(GREY)));
        } else {
            foot.push(("  ".into(), PLAIN));
        }
        foot.push(run);
    }
    // Cut hard, two columns short of the edge, as Claude Code's is at 40 columns.
    let strip_focus = match ui.focus {
        Focus::Strip(i) => Some(i),
        _ => None,
    };
    if strip_focus.is_some() {
        foot.truncate(1);
    }
    let mut foot = clip(foot, w.saturating_sub(2));
    // At the right, two columns short of the edge as the rest is: the model and effort, then
    // the usage (owner, 2026-11-07). The model and effort go first when both do not fit.
    let right: Vec<String> = [settings(&c.meta), usage.clone()]
        .into_iter()
        .flatten()
        .collect();
    let used = width(&plain(&foot));
    let room = w.saturating_sub(2).saturating_sub(used);
    let fits = |t: &str| room >= width(t) + 2;
    let both = right.join(" · ");
    let shown = if !right.is_empty() && fits(&both) {
        Some(both)
    } else {
        usage.filter(|u| fits(u))
    };
    if let Some(r) = shown {
        foot.push((" ".repeat(room - width(&r)), PLAIN));
        foot.push((r, fg(GREY)));
    }
    out.push(foot);
    // The agent strip under it while agents run.
    out.extend(crate::tasks::strip(
        &c.tasks,
        viewing,
        strip_focus,
        w.saturating_sub(2),
        now_ms,
    ));
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
    // Claude Code's own picker says "Your pick becomes the default for new sessions";
    // the holder saves it so (`holder::save_default_model`).
    out.push(line("   The default for new sessions too.", fg(GREY)));
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
            let own = current_model(&c.meta, list);
            for (i, (_, name)) in list.iter().enumerate().take(hi).skip(lo) {
                let current = own == Some(i);
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

/// `/btw`'s panel: the question, then the answer as markdown, scrolled when it is taller
/// than the screen leaves room for.
fn btw_dialog(
    question: &str,
    answer: &Option<Result<String, String>>,
    top: usize,
    w: usize,
    h: usize,
) -> Vec<Line> {
    let mut out = vec![rule(w, "▔", fg(LAVENDER))];
    out.extend(wrapped(
        &format!("/btw {question}"),
        "   ",
        "   ",
        w,
        bold(LAVENDER),
    ));
    out.push(Vec::new());
    let body: Vec<Line> = match answer {
        None => vec![line("   Answering…", fg(GREY))],
        Some(Err(why)) => wrapped(why, "   ", "   ", w, fg(GREY)),
        Some(Ok(text)) => crate::markdown::render(text, w.saturating_sub(5))
            .into_iter()
            .map(|l| {
                let mut row: Line = vec![("   ".into(), PLAIN)];
                row.extend(l);
                row
            })
            .collect(),
    };
    // What the transcript keeps above: a few rows, as a dialog leaves on a phone.
    let room = h.saturating_sub(out.len() + 5).max(3);
    let top = top.min(body.len().saturating_sub(room));
    if top > 0 {
        out.push(line(format!("   ↑ {top} more"), fg(GREY)));
    }
    let mut shown = room - usize::from(top > 0);
    if body.len() > top + shown {
        shown -= 1;
    }
    let below = body.len().saturating_sub(top + shown);
    out.extend(body.into_iter().skip(top).take(shown));
    if below > 0 {
        out.push(line(format!("   ↓ {below} more"), fg(GREY)));
    }
    out.push(Vec::new());
    out.extend(hints(&["Space, Enter or Esc to dismiss"], "   ", w));
    out
}

fn copy_dialog(sel: usize, choices: &[(String, String)], w: usize) -> Vec<Line> {
    const SHOWN: usize = 5;
    let mut out = vec![rule(w, "▔", fg(LAVENDER))];
    out.push(line("   Copy to clipboard", bold(LAVENDER)));
    out.push(Vec::new());
    let lo = sel
        .saturating_sub(SHOWN - 2)
        .min(choices.len().saturating_sub(SHOWN));
    let hi = (lo + SHOWN).min(choices.len());
    for (i, (label, _)) in choices.iter().enumerate().take(hi).skip(lo) {
        let mark = if i == sel { "❯" } else { " " };
        let st = if i == sel { fg(LAVENDER) } else { PLAIN };
        let text = format!("   {mark} {}. {label}", i + 1);
        out.push(line(cut(&text, w.saturating_sub(1)), st));
    }
    if hi < choices.len() {
        out.push(line(
            format!("      … +{} more", choices.len() - hi),
            fg(GREY),
        ));
    }
    out.push(Vec::new());
    out.extend(hints(&["Enter to copy", "Esc to cancel"], "   ", w));
    out
}

/// `/mode`: the four modes as `/model` lists its models, the session's own ticked.
fn mode_dialog(c: &Convo, sel: usize, w: usize) -> Vec<Line> {
    let mut out = vec![rule(w, "▔", fg(LAVENDER))];
    out.push(line("   Permission mode", bold(LAVENDER)));
    out.push(Vec::new());
    for (i, (mode, name, _)) in MODES.iter().enumerate() {
        let mark = if i == sel { "❯" } else { " " };
        let st = if i == sel { fg(LAVENDER) } else { PLAIN };
        let mut l = vec![(format!("   {mark} {}.  {name}", i + 1), st)];
        if *mode == c.meta.mode {
            l.push((" ✔".into(), fg(GREEN)));
        }
        out.push(l);
    }
    out.push(Vec::new());
    out.extend(hints(&["Enter to select", "Esc to cancel"], "   ", w));
    out
}

/// Where `/mode` opens: on the session's own mode, or the first.
pub fn mode_index(meta: &Meta) -> usize {
    MODES
        .iter()
        .position(|(m, _, _)| *m == meta.mode)
        .unwrap_or(0)
}

fn effort_dialog(sel: usize, w: usize) -> Vec<Line> {
    let level = EFFORT_CHOICES[sel.min(EFFORT_CHOICES.len() - 1)];
    let tag = if level == "auto" {
        " auto · /effort ".to_string()
    } else {
        format!(" {} {level} · /effort ", effort_glyph(level))
    };
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
    for (i, l) in EFFORT_CHOICES.iter().enumerate() {
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
/// The transcript laid out, from the cache while nothing it depends on has changed: scrolling
/// a long conversation laid it all out twice a wheel step, 50 ms each on a Pi, and Termux
/// sends a step for every line a finger travels (owner, 2026-11-02: "stutter when flick
/// scrolling in long conversations").
fn laid_out(c: &Convo, w: usize) -> std::rc::Rc<Vec<Line>> {
    let key = LayoutKey {
        changes: c.changes,
        w,
        expanded: c.expanded,
        entries: c.entries.len(),
        hidden: c.hidden.len(),
        state: c.state.clone(),
    };
    if let Some((k, lines)) = c.layout.borrow().as_ref() {
        if *k == key {
            return std::rc::Rc::clone(lines);
        }
    }
    let lines = std::rc::Rc::new(transcript(c, w).0);
    *c.layout.borrow_mut() = Some((key, std::rc::Rc::clone(&lines)));
    lines
}

/// The transcript's lines, then the prompts on their way under them.
struct Body {
    laid: std::rc::Rc<Vec<Line>>,
    extra: Vec<Line>,
}

impl Body {
    fn len(&self) -> usize {
        self.laid.len() + self.extra.len()
    }

    fn get(&self, i: usize) -> Option<&Line> {
        self.laid
            .get(i)
            .or_else(|| self.extra.get(i.checked_sub(self.laid.len())?))
    }

    fn at(&self, i: usize) -> &Line {
        self.get(i).expect("in range")
    }

    /// Lines `a..b`, cut to what there is, copied.
    fn slice(&self, a: usize, b: usize) -> Vec<Line> {
        (a..b.min(self.len())).map(|i| self.at(i).clone()).collect()
    }
}

struct Parts {
    body: Body,
    bottom: Vec<Line>,
    spinner: Vec<Line>,
    cursor: Option<(usize, usize)>,
    ask_top: bool,
    room: usize,
    start: usize,
}

fn parts(c: &Convo, ui: &Ui, w: usize, h: usize, now: Instant) -> Parts {
    // The agent in view, Claude Code's foregrounded one, in place of the conversation; the
    // footer, dialogs and prompts waiting on the owner stay the conversation's.
    let shown: &Convo = c.viewing.as_ref().map_or(c, |(_, a)| a.as_ref());
    let mut body = Body {
        laid: laid_out(shown, w),
        extra: Vec::new(),
    };
    for p in &shown.pending {
        let frame = now.saturating_duration_since(p.since).as_millis() / 120;
        let glyph = crate::ui::SPINNER[frame as usize % crate::ui::SPINNER.len()];
        body.extra.push(Vec::new());
        body.extra
            .extend(prompt_lines(c.display(&p.text), &format!("{glyph} "), w));
        // One waiting behind a turn can go now, as Claude Code says under it.
        if p.queued {
            body.extra
                .push(line("  ctrl+x ctrl+s to send now", fg(GREY)));
        }
    }
    // The bottom: a dialog, or the input box.
    let waiting = c.waiting();
    let mut cursor = None;
    // Whether the prompt is drawn, rather than a dialog: the spinner goes over it.
    let mut prompt = false;
    let mut ask_top = false;
    let bottom: Vec<Line> = match (&ui.dialog, waiting) {
        (Some(Dialog::Model { sel, list }), _) => model_dialog(c, *sel, list, w),
        (Some(Dialog::Effort { sel }), _) => effort_dialog(*sel, w),
        (Some(Dialog::Mode { sel }), _) => mode_dialog(c, *sel, w),
        (Some(Dialog::Copy { sel, choices }), _) => copy_dialog(*sel, choices, w),
        (Some(Dialog::Tasks(v)), _) => crate::tasks::draw(c, v, w, h, ui.now_ms()),
        (
            Some(Dialog::Btw {
                question,
                answer,
                top,
            }),
            _,
        ) => btw_dialog(question, answer, *top, w, h),
        (Some(d @ Dialog::Ask { q, .. }), Some(Entry::Ask { questions, .. })) => {
            ask_top = true;
            match questions.get(*q) {
                Some(qq) => ask_dialog(qq, d, w, h.saturating_sub(3)),
                None => Vec::new(),
            }
        }
        (Some(Dialog::Perm { sel }), Some(e @ Entry::Perm { .. })) => perm_dialog(e, *sel, w),
        _ => {
            // The suggestion list over the prompt, as Claude Code puts it.
            let list = if c.viewing.is_some() {
                Vec::new()
            } else {
                suggestions(ui)
            };
            let mut b = if list.is_empty() {
                Vec::new()
            } else {
                suggest_lines(&list, ui.suggest, w)
            };
            let (boxed, cur) = input_box(c, ui, w, h);
            prompt = true;
            // No cursor in a prompt that takes nothing.
            if c.viewing.is_none() {
                cursor = Some((cur.0 + b.len(), cur.1));
            }
            b.extend(boxed);
            b
        }
    };
    let spinner: Vec<Line> = match (shown.state.as_str(), prompt) {
        ("working" | "compacting", true) => {
            let c = shown;
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
    // The transcript's end sits on the input: a prompt just sent goes under what came before
    // rather than to the top of an emptied screen, as Claude Code's fullscreen mode draws it
    // (owner, 2026-11-01: "stop pushing the entire transcript off screen"). One that fits is
    // drawn from the top, logo and all (R1).
    let total = body.len();
    let start = total.saturating_sub(room);
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
        top.extend(body.slice(start, start + room));
    } else {
        // Scrolled back: the prompt this part answers pinned on top, the jump line under it.
        // The top moves back from where it sat at the bottom, one line a step: measured from
        // the end instead, a prompt drawn at the top with room to spare under it jumped by
        // all that room on the first step.
        let content = room.saturating_sub(3);
        let start = start.saturating_sub(ui.scroll);
        let end = (start + content).min(total);
        // The prompt's first line, and `…` when it ran on.
        let pinned = (0..start.min(total))
            .rev()
            .find(|&i| plain(body.at(i)).starts_with("❯ "))
            .map(|i| {
                let first = plain(body.at(i)).trim_end().to_string();
                let more = body
                    .get(i + 1)
                    .is_some_and(|l| l.first().is_some_and(|r| r.1.bg == Some(BAND)));
                if more { format!("{first}…") } else { first }
            })
            .unwrap_or_default();
        top.push(line(cut(&pinned, w), PLAIN));
        top.extend(body.slice(start, end));
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

    #[test]
    fn an_agents_message_is_one_grey_line_until_ctrl_o_shows_what_it_said() {
        // As Claude Code 2.1.311 draws a subagent's hand-back, captured at 80 columns on
        // 2026-11-04: `› Message from @general-purpose (ctrl+o to expand)`, grey.
        let mut c = Convo::new("h", "claude-3", "r");
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"msg","n":"general-purpose","x":"All 25 files read.\n\n1. adduser: GPL-2+"}"#,
                r#"{"t":"add","id":2,"k":"msg","n":"infra","x":"Pinned\n0.16.0.","i":true}"#,
            ],
        );
        let body = |c: &Convo, w: usize| -> Vec<(String, St)> {
            let (lines, _) = transcript(c, w);
            lines[4..]
                .iter()
                .map(|l| {
                    (
                        plain(l).trim_end().to_string(),
                        l.first().map_or(PLAIN, |r| r.1),
                    )
                })
                .collect()
        };
        let grey = fg(GREY);
        assert_eq!(
            body(&c, 80),
            [
                (String::new(), PLAIN),
                (
                    "› Message from @general-purpose (ctrl+o to expand)".into(),
                    grey
                ),
                (String::new(), PLAIN),
                (
                    "› Message from @infra: Pinned 0.16.0. (ctrl+o to expand)".into(),
                    grey
                ),
            ]
        );
        // At 40 the line wraps as Claude Code's does; a peer's words are cut to keep its line
        // to one row, and left out where nothing of them fits.
        let narrow: Vec<String> = body(&c, 40).into_iter().map(|(t, _)| t).collect();
        assert_eq!(
            narrow,
            [
                "",
                "› Message from @general-purpose (ctrl+o",
                "to expand)",
                "",
                "› Message from @infra (ctrl+o to expand)",
            ]
        );
        c.expanded = true;
        assert_eq!(
            body(&c, 80),
            [
                (String::new(), PLAIN),
                ("› Message from general-purpose".into(), grey),
                ("  All 25 files read.".into(), PLAIN),
                (String::new(), PLAIN),
                ("  1. adduser: GPL-2+".into(), PLAIN),
                (String::new(), PLAIN),
                ("› Message from infra".into(), grey),
                ("  Pinned".into(), PLAIN),
                ("  0.16.0.".into(), PLAIN),
            ]
        );
    }

    #[test]
    fn an_agent_is_drawn_on_its_own_with_its_progress_under_it() {
        let mut c = Convo::new("h", "claude-3", "r");
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"tool","n":"Read","a":"a.txt"}"#,
                r#"{"t":"res","id":1,"ok":true,"p":["x"],"l":1}"#,
                r#"{"t":"add","id":2,"k":"agent","n":"Explore","a":"Count lines in .txt files"}"#,
                r#"{"t":"prog","id":2,"p":["Glob(*.txt)","Bash(wc -l -- a.txt b.txt err.txt)","+1 more tool use"],"st":"running"}"#,
            ],
        );
        let body: Vec<String> = transcript(&c, 40).0.iter().map(plain).collect();
        let at = body
            .iter()
            .position(|l| l.starts_with("● Explore"))
            .expect("drawn");
        assert_eq!(
            &body[at..],
            [
                "● Explore(Count lines in .txt files)",
                "  ⎿  Glob(*.txt)",
                "     Bash(wc -l -- a.txt b.txt err.txt)",
                "     +1 more tool use",
            ]
        );
        assert!(
            body.iter().any(|l| l.contains("Read 1 file")),
            "the tool before it is still summed up on its own: {body:?}"
        );
        feed(
            &mut c,
            &[
                r#"{"t":"prog","id":2,"p":["Done (3 tool uses · 21.6k tokens · 7.6s)"],"st":"completed"}"#,
            ],
        );
        let lines = transcript(&c, 40).0;
        let head = lines
            .iter()
            .find(|l| plain(l).starts_with("● Explore"))
            .unwrap();
        assert_eq!(head[0].1, fg(GREEN), "a green dot once it is done");
        assert!(head[1].1.bold, "its name in bold");
        let body: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(
            body[body.len() - 2..],
            ["  ⎿  Done (3 tool uses · 21.6k tokens ·", "     7.6s)"]
        );
    }

    #[test]
    fn expanded_every_tool_call_shows_its_result_and_collapsed_they_are_summed() {
        let mut c = Convo::new("h", "claude-3", "r");
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"tool","n":"Bash","a":"ls"}"#,
                r#"{"t":"res","id":1,"ok":true,"p":["a","b","c"],"l":5}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Read","a":"x.rs"}"#,
                r#"{"t":"res","id":2,"ok":false,"p":["no such file"],"l":1}"#,
            ],
        );
        // Under the logo block's four lines, blank lines aside.
        let body = |c: &Convo| -> Vec<String> {
            let t = transcript(c, 40).0;
            t.iter()
                .skip(4)
                .map(plain)
                .filter(|l| !l.is_empty())
                .collect()
        };
        assert_eq!(body(&c), ["  Ran 1 shell command, read 1 file"]);
        c.expanded = true;
        assert_eq!(
            body(&c),
            [
                "● Bash(ls)",
                "  ⎿  a",
                "     b",
                "     c",
                "     … +2 lines",
                "● Read(x.rs)",
                "  ⎿  Error",
                "     no such file",
            ]
        );
    }

    #[test]
    fn background_shells_show_on_the_mode_line_and_the_turns_end_line() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("", "auto"),
                r#"{"t":"tasks","l":[{"i":"a","k":"shell","d":"x","st":"running"},{"i":"b","k":"shell","d":"y","st":"running"},{"i":"m","k":"monitor","d":"z","st":"running"}]}"#,
            ],
        );
        c.local(Entry::Worked("Worked for 1s · done 3:43 AM".into()));
        let mut ui = Ui::new();
        let f = draw(&c, &ui, 80, 24, Instant::now());
        let t = text(&f);
        let left = "  ⏵⏵ auto mode on · 2 shells, 1 monitor";
        assert_eq!(
            t[23],
            format!(
                "{left}{}Opus 5.5",
                " ".repeat(78 - width(left) - width("Opus 5.5"))
            )
        );
        let last = &f.lines[23];
        let shells = last.iter().find(|(t, _)| t.contains("shells")).unwrap();
        assert_eq!(shells.1.fg, Some(crate::tasks::CYAN));
        assert!(
            t.iter()
                .any(|l| l == "✻ Worked for 1s · done 3:43 AM · 2 shells, 1 monitor still running"),
            "{t:?}"
        );
        // At 40 it is cut hard at the edge, as Claude Code's is.
        let t = text(&draw(&c, &ui, 40, 24, Instant::now()));
        assert_eq!(t[23], "  ⏵⏵ auto mode on · 2 shells, 1 monito");
        // Focused by Down, the pill is drawn inverse: its colour behind it.
        ui.focus = Focus::Pill;
        let f = draw(&c, &ui, 80, 24, Instant::now());
        let pill = f.lines[23]
            .iter()
            .find(|(t, _)| t.contains("shells"))
            .unwrap();
        assert_eq!(pill.1.bg, Some(crate::tasks::CYAN));
        // A subagent running adds the way to see it; the turn's line says only the shells.
        feed(
            &mut c,
            &[r#"{"t":"tasks","l":[{"i":"g","k":"agent","d":"survey","st":"running"}]}"#],
        );
        let t = text(&draw(&c, &Ui::new(), 80, 24, Instant::now()));
        // And the agent strip under it: `main`, then the agent.
        let left = "  ⏵⏵ auto mode on · /tasks to see subagents";
        assert_eq!(
            t[20],
            format!(
                "{left}{}Opus 5.5",
                " ".repeat(78 - width(left) - width("Opus 5.5"))
            )
        );
        assert_eq!(t[21], "");
        assert_eq!(t[22], "  ● main");
        assert_eq!(t[23], "  ◯ agent  survey");
        assert!(
            t.iter().any(|l| l == "✻ Worked for 1s · done 3:43 AM"),
            "{t:?}"
        );
    }

    #[test]
    fn an_agent_in_view_takes_the_screen_with_claude_codes_teal_rules() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("", "auto"),
                r#"{"t":"add","id":1,"k":"text","x":"the conversation"}"#,
                r#"{"t":"tasks","l":[{"i":"a","k":"agent","d":"slow reader","c":"general-purpose","st":"running"}]}"#,
            ],
        );
        let mut a = Convo::new("h", "s", "r");
        feed(&mut a, &[r#"{"t":"add","id":1,"k":"user","x":"Read it."}"#]);
        c.viewing = Some(("a".into(), Box::new(a)));
        let f = draw(&c, &Ui::new(), 40, 24, Instant::now());
        let t = text(&f);
        assert!(t.iter().any(|l| l.contains("Read it.")), "{t:?}");
        assert!(!t.iter().any(|l| l.contains("the conversation")), "{t:?}");
        assert_eq!(t[16], "────────────────────────── slow reader ─");
        assert_eq!(f.lines[16][1].1.bg, Some(AGENT_TEAL));
        assert_eq!(t[17], "❯ headless claude can't message agents");
        assert_eq!(f.cursor, None, "a prompt that takes nothing has no cursor");
        assert_eq!(f.lines[18][0].1.fg, Some(AGENT_TEAL));
        assert_eq!(t[22..], ["  ◯ main", "  ● general-purpose  slow reader"]);
    }

    #[test]
    fn modes_failures_and_errors_take_claude_codes_own_colours() {
        let colour = |m: &str| mode_line(m).map(|(_, c)| c);
        assert_eq!(colour("auto"), Some(YELLOW));
        assert_eq!(colour("acceptEdits"), Some(PURPLE));
        assert_eq!(colour("plan"), Some(TEAL));
        assert_eq!(colour("bypassPermissions"), Some(RED));
        assert_eq!(colour("default"), Some(MANUAL_GREY));

        let mut c = Convo::new("h", "claude-3", "r");
        c.expanded = true;
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"agent","n":"Workflow","a":"audit"}"#,
                r#"{"t":"prog","id":1,"p":["Failed (0 tool uses · 0.1s)"],"st":"failed"}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Read","a":"x"}"#,
                r#"{"t":"res","id":2,"ok":false,"p":["gone"],"l":1}"#,
                r#"{"t":"add","id":3,"k":"note","x":"Error: error_max_turns"}"#,
                r#"{"t":"add","id":4,"k":"note","x":"Compacted"}"#,
            ],
        );
        let t = transcript(&c, 40).0;
        let find = |p: &str| t.iter().find(|l| plain(l).starts_with(p)).unwrap();
        assert_eq!(find("● Workflow")[0].1, fg(RED), "a failed agent's dot");
        assert_eq!(find("● Read")[0].1, fg(RED), "a failed call's dot");
        assert_eq!(find("  ⎿  Error")[0].1, fg(RED));
        assert_eq!(find("  ⎿  Error: error_max_turns")[0].1, fg(RED));
        assert_eq!(
            find("  ⎿  Compacted")[0].1,
            fg(GREY),
            "a plain note stays grey"
        );
    }

    #[test]
    fn code_blocks_are_found_by_their_fences_and_offered_after_the_whole() {
        let text = "Try:\n\n```rust\nfn main() {}\n```\n\nor\n\n~~~\nmake\n~~~\n\n```sh\nunclosed";
        assert_eq!(
            code_blocks(text),
            [
                ("rust".to_string(), "fn main() {}".to_string()),
                (String::new(), "make".to_string()),
                ("sh".to_string(), "unclosed".to_string()),
            ]
        );
        let labels: Vec<String> = copy_choices(text).into_iter().map(|(l, _)| l).collect();
        assert_eq!(
            labels,
            [
                "Full response",
                "rust: fn main() {}",
                "code: make",
                "sh: unclosed"
            ]
        );
    }

    #[test]
    fn a_long_btw_answer_scrolls_inside_its_panel() {
        let answer: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let d = btw_dialog("q", &Some(Ok(answer.join("\n\n"))), 0, 40, 24);
        let t: Vec<String> = d.iter().map(plain).collect();
        assert_eq!(t[1], "   /btw q");
        assert!(
            t.iter().any(|l| l.contains("↓") && l.contains("more")),
            "{t:?}"
        );
        assert!(
            d.len() < 24,
            "it leaves the screen some transcript: {}",
            d.len()
        );
        let d = btw_dialog("q", &Some(Ok(answer.join("\n\n"))), 5, 40, 24);
        let t: Vec<String> = d.iter().map(plain).collect();
        assert!(t.iter().any(|l| l.contains("↑ 5 more")), "{t:?}");
        assert!(
            t.iter()
                .any(|l| l.contains("Space, Enter or Esc to dismiss"))
        );
    }

    fn text(f: &Frame) -> Vec<String> {
        f.lines
            .iter()
            .map(|l| plain(l).trim_end().to_string())
            .collect()
    }

    /// The mockup's rows, as `docs/mockups.md` has them.
    /// What the status line reads in the mockups: so much of 2G, a load of 0.7 on four cores.
    fn figures(used_g: f64, total_g: f64, load: u64) -> crate::statusline::Figures {
        let g = |x: f64| (x * (1u64 << 30) as f64) as u64;
        crate::statusline::Figures {
            used: Some(g(used_g)),
            total: Some(g(total_g)),
            load: Some(load),
            cores: Some(4),
            place: "claude-sessions".into(),
        }
    }

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

    /// The limits' resets in `meta_used`, in epoch seconds, and a wall clock before both.
    const FIVE_HOUR_RESETS: u64 = 1_793_990_000;
    const WEEK_RESETS: u64 = 1_794_006_000;
    const BEFORE_RESET_MS: u64 = 1_793_980_000_000;

    /// `meta` with the usage limits the relay sends: so much of 5 hours and of the week.
    fn meta_used(title: &str, mode: &str, five_hour: u8, week: u8) -> String {
        meta(title, mode).replacen(
            r#""t":"meta","#,
            &format!(
                r#""t":"meta","u5":{{"p":{five_hour},"r":{FIVE_HOUR_RESETS}}},"u7":{{"p":{week},"r":{WEEK_RESETS}}},"#
            ),
            1,
        )
    }

    fn convo() -> Convo {
        let mut c = Convo::new("claude-sessions", "claude-3", "run");
        c.apply(&parse(r#"{"t":"n","n":0}"#).unwrap(), Instant::now());
        c
    }

    #[test]
    fn a_step_claude_summarised_is_one_line_and_ctrl_o_shows_its_commands() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"user","x":"look around"}"#,
                r#"{"t":"st","s":"working"}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Bash","a":"ls /"}"#,
                r#"{"t":"add","id":3,"k":"tool","n":"Read","a":"/etc/hostname"}"#,
                r#"{"t":"res","id":2,"ok":true,"p":["bin"],"l":1}"#,
                r#"{"t":"res","id":3,"ok":true,"p":["box"],"l":1}"#,
                r#"{"t":"add","id":4,"k":"tool","n":"Bash","a":"uname -a"}"#,
                r#"{"t":"sum","id":2,"x":"Listed root, read hostname","g":2}"#,
                r#"{"t":"sum","id":3,"x":"Listed root, read hostname","g":2}"#,
            ],
        );
        let body = |c: &Convo| -> Vec<String> { transcript(c, 40).0.iter().map(plain).collect() };
        // The summarised step a line; the call still running as before, under way.
        let b = body(&c);
        let at = b
            .iter()
            .position(|l| l == "  Listed root, read hostname")
            .unwrap_or_else(|| panic!("{b:#?}"));
        assert_eq!(
            b[at + 1..at + 3],
            ["● Running 1 shell command…", "  ⎿  $ uname -a"]
        );
        // Done, with the next step summarised too, and one call not yet: a line each.
        feed(
            &mut c,
            &[
                r#"{"t":"res","id":4,"ok":true,"p":["Linux"],"l":1}"#,
                r#"{"t":"add","id":5,"k":"tool","n":"Bash","a":"date"}"#,
                r#"{"t":"res","id":5,"ok":true,"p":["Fri"],"l":1}"#,
                r#"{"t":"sum","id":4,"x":"Checked kernel info","g":4}"#,
                r#"{"t":"st","s":"idle"}"#,
            ],
        );
        let b = body(&c);
        let at = b
            .iter()
            .position(|l| l == "  Listed root, read hostname")
            .unwrap();
        assert_eq!(
            b[at..],
            [
                "  Listed root, read hostname",
                "  Checked kernel info",
                "  Ran 1 shell command"
            ]
        );
        // Ctrl+O: the commands themselves.
        c.expanded = true;
        let b = body(&c);
        for call in ["● Bash(ls /)", "● Read(/etc/hostname)", "● Bash(uname -a)"] {
            assert!(b.iter().any(|l| l == call), "{call}: {b:#?}");
        }
        assert!(!b.iter().any(|l| l.contains("Listed root")), "{b:#?}");
    }

    #[test]
    fn narration_is_drawn_as_text_with_a_grey_summary_hint_and_is_not_copied() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"user","x":"fix it"}"#,
                r#"{"t":"add","id":2,"k":"text","sm":true,"x":"Checking the relay next."}"#,
                r#"{"t":"add","id":3,"k":"tool","n":"Bash","a":"ls"}"#,
                r#"{"t":"res","id":3,"ok":true,"p":["a"],"l":1}"#,
                r#"{"t":"add","id":4,"k":"text","sm":true,"x":"Reading every file in src/ now"}"#,
                r#"{"t":"add","id":5,"k":"text","x":"Fixed."}"#,
            ],
        );
        let lines = transcript(&c, 40).0;
        let body: Vec<String> = lines.iter().map(plain).collect();
        let at = body
            .iter()
            .position(|l| l == "● Checking the relay next. · summary")
            .unwrap_or_else(|| panic!("{body:#?}"));
        let hint = lines[at].last().unwrap();
        assert_eq!((hint.0.as_str(), hint.1), (" · summary", fg(GREY)));
        // It breaks a run of tool calls, as text does.
        assert_eq!(body[at + 2], "  Ran 1 shell command");
        // Too long for the last line: the hint goes on a line of its own.
        let end = body.iter().position(|l| l == "● Fixed.").unwrap();
        assert_eq!(body[end - 2], "  · summary", "{body:#?}");
        // /copy takes text blocks alone, as Claude Code's does.
        assert_eq!(c.responses(), ["Fixed."]);
    }

    #[test]
    fn r2_working_puts_the_new_prompt_under_the_last_turn_and_the_spinner_over_the_box() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta_used("fix the door", "auto", 12, 72),
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
        ui.load = Some(figures(1.2, 2.0, 70));
        ui.wall = Some(BEFORE_RESET_MS);
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
    fn the_usage_sits_at_the_footers_right_and_a_window_that_has_reset_is_not_drawn() {
        let mut c = convo();
        feed(&mut c, &[&meta_used("fix the door", "auto", 12, 72)]);
        let mut ui = Ui::new();
        ui.wall = Some(BEFORE_RESET_MS);
        let last = |c: &Convo, ui: &Ui, w: usize| {
            text(&draw(c, ui, w, 24, Instant::now()))
                .last()
                .unwrap()
                .clone()
        };
        // At 40 the mode line's hint gives way to the usage, and the model, not fitting beside
        // it, goes first; at 80 all of it fits, the model before the usage.
        assert_eq!(last(&c, &ui, 40), "  ⏵⏵ auto mode on      5h 12% · 7d 72%");
        assert_eq!(
            last(&c, &ui, 80),
            format!(
                "  ⏵⏵ auto mode on (/mode to change){}Opus 5.5 · 5h 12% · 7d 72%",
                " ".repeat(17)
            )
        );
        // From the 5-hour reset, only the week is drawn, and now the model fits beside it.
        ui.wall = Some(FIVE_HOUR_RESETS * 1000);
        assert_eq!(
            last(&c, &ui, 40),
            format!("  ⏵⏵ auto mode on{}Opus 5.5 · 7d 72%", " ".repeat(4))
        );
        feed(&mut c, &[&meta_used("fix the door", "default", 12, 72)]);
        assert_eq!(last(&c, &ui, 40), "  ⏸ manual mode on   Opus 5.5 · 7d 72%");
        // From both resets, no usage: the model alone.
        ui.wall = Some(WEEK_RESETS * 1000);
        assert_eq!(
            last(&c, &ui, 40),
            format!("  ⏸ manual mode on{}Opus 5.5", " ".repeat(12))
        );
    }

    #[test]
    fn the_footer_says_the_model_and_an_effort_that_is_set() {
        let m = |model: &str, chosen: &str, effort: &str| Meta {
            model: model.into(),
            chosen: chosen.into(),
            effort: effort.into(),
            ..Meta::default()
        };
        assert_eq!(
            settings(&m("claude-opus-5-5", "default", "")).as_deref(),
            Some("Opus 5.5")
        );
        assert_eq!(
            settings(&m("claude-sonnet-5-5", "sonnet", "high")).as_deref(),
            Some("Sonnet 5.5 ● high")
        );
        // Before claude's first `init`, the model as chosen; with neither, nothing.
        assert_eq!(settings(&m("", "haiku", "")).as_deref(), Some("Haiku"));
        assert_eq!(settings(&m("", "", "low")), None);
    }

    #[test]
    fn model_and_effort_pickers_open_on_the_sessions_own() {
        let list: Vec<(String, String)> = [
            ("default", "Default (recommended)"),
            ("opus", "Opus"),
            ("fable", "Fable"),
            ("sonnet", "Sonnet"),
        ]
        .iter()
        .map(|(v, d)| (v.to_string(), d.to_string()))
        .collect();
        let m = |model: &str, chosen: &str| Meta {
            model: model.into(),
            chosen: chosen.into(),
            ..Meta::default()
        };
        // On the default model, Default is the session's own, though it resolves to Opus.
        assert_eq!(
            current_model(&m("claude-opus-5-5", "default"), &list),
            Some(0)
        );
        assert_eq!(current_model(&m("claude-opus-5-5", "opus"), &list), Some(1));
        // A model chosen as a full id, or before the chosen one is known: as resolved.
        assert_eq!(
            current_model(&m("claude-sonnet-5-5", "claude-sonnet-5-5"), &list),
            Some(3)
        );
        assert_eq!(current_model(&m("claude-fable-5-1", ""), &list), Some(2));
        assert_eq!(current_model(&m("", ""), &list), None);
        // Effort: the session's own level, or auto while none is set.
        let e = |effort: &str| Meta {
            effort: effort.into(),
            ..Meta::default()
        };
        assert_eq!(EFFORT_CHOICES[effort_index(&e(""))], "auto");
        assert_eq!(EFFORT_CHOICES[effort_index(&e("xhigh"))], "xhigh");
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
        ui.load = Some(figures(0.9, 2.0, 70));
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
        for l in want
            .iter_mut()
            .filter(|l| *l == "● Creating empty file named x")
        {
            *l = "● Create empty file named x".into();
        }
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
    fn a_pasted_prompt_is_drawn_with_its_stand_in_waiting_and_once_echoed() {
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        let t0 = Instant::now();
        let full = "look: a\nb\nc\nd";
        c.sent(full, t0);
        c.collapsed(full, "look: [Pasted text #1 +3 lines]");
        let got = text(&draw(&c, &Ui::new(), 40, 24, t0));
        assert!(
            got.iter().any(|l| l == "⠋ look: [Pasted text #1 +3 lines]"),
            "{got:?}"
        );
        assert!(!got.iter().any(|l| l == "  b"), "{got:?}");
        feed(
            &mut c,
            &[r#"{"t":"add","id":1,"k":"user","x":"look: a\nb\nc\nd\n"}"#],
        );
        assert!(c.pending.is_empty());
        let got = text(&draw(&c, &Ui::new(), 40, 24, t0));
        assert!(
            got.iter().any(|l| l == "❯ look: [Pasted text #1 +3 lines]"),
            "{got:?}"
        );
        assert!(!got.iter().any(|l| l == "  b"), "{got:?}");
        // A prompt this view did not collapse is drawn whole.
        feed(&mut c, &[r#"{"t":"add","id":2,"k":"user","x":"x\ny"}"#]);
        let got = text(&draw(&c, &Ui::new(), 40, 24, t0));
        assert!(got.iter().any(|l| l == "  y"), "{got:?}");
    }

    #[test]
    fn a_prompt_shows_at_once_behind_the_spinner_until_claude_echoes_it() {
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        let t0 = Instant::now();
        c.sent("hello there", t0);
        let got = text(&draw(&c, &Ui::new(), 40, 24, t0));
        assert!(got.iter().any(|l| l == "⠋ hello there"), "{got:?}");
        let later = text(&draw(
            &c,
            &Ui::new(),
            40,
            24,
            t0 + Duration::from_millis(250),
        ));
        assert!(
            later.iter().any(|l| l == "⠹ hello there"),
            "it turns: {later:?}"
        );
        // Someone else's prompt is not this one.
        feed(&mut c, &[r#"{"t":"add","id":1,"k":"user","x":"other"}"#]);
        assert_eq!(c.pending.len(), 1);
        feed(
            &mut c,
            &[r#"{"t":"add","id":2,"k":"user","x":"hello there"}"#],
        );
        assert!(c.pending.is_empty());
        let got = text(&draw(&c, &Ui::new(), 40, 24, t0));
        assert!(got.iter().any(|l| l == "❯ hello there"), "{got:?}");
        assert_eq!(got.iter().filter(|l| l.ends_with("hello there")).count(), 1);
    }

    #[test]
    fn the_status_line_says_what_an_ssh_sessions_says() {
        let f = figures(0.9, 2.0, 70);
        let at = |w: usize| plain(&status_line("claude-sessions/claude-3", &f, w));
        // R1 at 80, in mockups-80.md.
        assert_eq!(
            at(80),
            "  claude-sessions/claude-3 · RAM: 0.9G / 2.0G, Load: 0.7"
        );
        // Where that does not fit, the ssh session's own line, then its figures alone.
        assert_eq!(at(52), "  RAM: 0.9G / 2.0G, Load: 0.7, claude-sessions");
        assert_eq!(at(40), "  RAM: 0.9G / 2.0G, Load: 0.7");
        // statusline.rs's levels: RAM past 70% and load past 0.7 a core are yellow, past 85%
        // and 1.0 a core red.
        let hot = status_line("x/y", &figures(1.8, 2.0, 300), 40);
        let style = |word: &str| hot.iter().find(|(t, _)| t.starts_with(word)).unwrap().1;
        assert_eq!(style("RAM").fg, Some(RED));
        assert_eq!(style("Load").fg, Some(YELLOW));
        let calm = status_line("x/y", &f, 40);
        assert!(calm.iter().all(|(_, s)| s.fg == Some(GREY)), "{calm:?}");
    }

    #[test]
    fn the_input_soft_wraps_at_words_and_no_row_starts_with_a_space() {
        let rows = |t: &str, cursor: usize, room: usize| {
            let chars: Vec<char> = t.chars().collect();
            let (r, at) = soft_wrap(&chars, cursor, room);
            let r: Vec<String> = r
                .iter()
                .map(|&(a, b)| chars[a..b].iter().collect())
                .collect();
            (r, at)
        };
        // The space a row breaks on hangs at its end.
        assert_eq!(
            rows("hello world again", 0, 11).0,
            ["hello world ", "again"]
        );
        assert_eq!(
            rows("hello world again", 0, 10).0,
            ["hello ", "world ", "again"]
        );
        assert_eq!(rows("aaa bbb  ccc", 0, 8).0, ["aaa bbb  ", "ccc"]);
        // A word wider than a row is broken; a newline ends one.
        assert_eq!(rows("abcdefghij", 0, 4).0, ["abcd", "efgh", "ij"]);
        assert_eq!(rows("one\ntwo", 0, 10).0, ["one", "two"]);
        // The cursor: on the word it is in, at the start of a wrapped row, past a full one.
        assert_eq!(rows("hello world", 6, 8).1, (1, 0));
        assert_eq!(rows("hello world", 9, 8).1, (1, 3));
        assert_eq!(rows("one\ntwo", 3, 10).1, (0, 3), "before the newline");
        assert_eq!(
            rows("abcd", 4, 4),
            (vec!["abcd".into(), String::new()], (1, 0))
        );
        assert_eq!(rows("", 0, 10), (vec![String::new()], (0, 0)));
        // Drawn in the box: no row after the first starts with a space.
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        let mut ui = Ui::new();
        ui.input = "please wrap this prompt at its words  and not mid word".into();
        ui.cursor = ui.input.chars().count();
        let f = draw(&c, &ui, 20, 24, Instant::now());
        let got = text(&f);
        let at = got.iter().position(|l| l.starts_with("❯ please")).unwrap();
        assert_eq!(
            &got[at..at + 4],
            [
                "❯ please wrap this",
                "  prompt at its",
                "  words  and not mid",
                "  word"
            ]
        );
        assert_eq!(f.cursor, Some((at + 3, 6)), "after the last word");
    }

    #[test]
    fn a_prompt_on_its_way_from_before_a_reconnect_is_drawn_until_echoed() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                r#"{"t":"st","s":"working"}"#,
                r#"{"t":"wait","p":[{"u":"u1","x":"then this","q":true}]}"#,
                r#"{"t":"wait","p":[{"u":"u1","x":"then this","q":true}]}"#,
            ],
        );
        assert_eq!(c.pending.len(), 1, "once, however often it is said");
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        let at = got.iter().position(|l| l.ends_with("then this")).unwrap();
        assert_eq!(got[at + 1], "  ctrl+x ctrl+s to send now");
        feed(
            &mut c,
            &[r#"{"t":"add","id":1,"k":"user","x":"then this"}"#],
        );
        assert!(c.pending.is_empty());
    }

    #[test]
    fn the_cached_layout_follows_every_change_it_depends_on() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"tool","n":"Read","a":"a.txt"}"#,
                r#"{"t":"add","id":2,"k":"text","x":"first"}"#,
            ],
        );
        let shown = |c: &Convo, w: usize| text(&draw(c, &Ui::new(), w, 24, Instant::now()));
        assert!(shown(&c, 40).iter().any(|l| l.contains("first")));
        // An op, streamed or set.
        feed(&mut c, &[r#"{"t":"app","id":2,"x":" and more"}"#]);
        assert!(shown(&c, 40).iter().any(|l| l.contains("first and more")));
        feed(&mut c, &[r#"{"t":"set","id":2,"x":"replaced"}"#]);
        assert!(shown(&c, 40).iter().any(|l| l.contains("replaced")));
        // What the client changes itself, and the width.
        let folded = shown(&c, 40);
        c.expanded = true;
        assert_ne!(shown(&c, 40), folded);
        assert_eq!(*laid_out(&c, 40), transcript(&c, 40).0);
        assert_eq!(*laid_out(&c, 20), transcript(&c, 20).0, "the width");
        c.hidden.insert(1);
        assert!(!shown(&c, 40).iter().any(|l| l.contains("replaced")));
        assert_eq!(*laid_out(&c, 40), transcript(&c, 40).0);
    }

    #[test]
    fn an_older_stretch_goes_above_what_is_there_and_the_view_stays_put() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":30,"k":"user","x":"newer"}"#,
                r#"{"t":"add","id":31,"k":"text","x":"newer reply"}"#,
                r#"{"t":"n","n":2}"#,
                r#"{"t":"more","m":true}"#,
            ],
        );
        c.hidden.insert(1);
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        assert!(
            got.iter().any(|l| l == "  Loading earlier messages…"),
            "{got:?}"
        );
        for n in 0..20 {
            feed(
                &mut c,
                &[&format!(
                    r#"{{"t":"add","id":{},"k":"text","x":"line {n}"}}"#,
                    40 + n
                )],
            );
        }
        let mut ui = Ui::new();
        ui.scroll = 5;
        let before = text(&draw(&c, &ui, 40, 12, Instant::now()));
        assert!(before.iter().any(|l| l.contains("line 1")), "{before:?}");
        feed(
            &mut c,
            &[
                r#"{"t":"old","o":[{"t":"add","id":1,"k":"user","x":"older"},{"t":"add","id":2,"k":"text","x":"older reply"}]}"#,
                r#"{"t":"more","m":false}"#,
            ],
        );
        assert_eq!(c.entries.len(), 24);
        assert!(matches!(&c.entries[0], Entry::Prompt(p) if p == "older"));
        assert!(
            c.hidden.contains(&3),
            "a rewind's mark moves with its entry"
        );
        // Its ops still find their items.
        feed(&mut c, &[r#"{"t":"set","id":2,"x":"older reply, set"}"#]);
        assert!(matches!(&c.entries[1], Entry::Text(t) if t.ends_with("set")));
        assert!(!c.more);
        // Measured from the bottom, the view is where it was.
        assert_eq!(text(&draw(&c, &ui, 40, 12, Instant::now())), before);
    }

    #[test]
    fn a_prompt_taller_than_half_the_screen_scrolls_with_the_cursor() {
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        let mut ui = Ui::new();
        ui.input = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.cursor = ui.input.chars().count();
        let f = draw(&c, &ui, 40, 24, Instant::now());
        let got = text(&f);
        // Twelve rows of 24, the cursor's last among them, the rows above it marked.
        let at = got.iter().position(|l| l == "↑ line 9").unwrap();
        assert_eq!(got[at + 11], "  line 20");
        assert!(got[at + 12].starts_with('─'), "{got:?}");
        assert_eq!(f.cursor, Some((at + 11, 9)));
        // At the start: the first rows, the rows below marked.
        ui.cursor = 0;
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        let at = got.iter().position(|l| l == "❯ line 1").unwrap();
        assert_eq!(got[at + 11], "↓ line 12");
        assert!(got[at + 12].starts_with('─'), "{got:?}");
    }

    #[test]
    fn esc_hands_back_a_prompt_nothing_answered_as_claude_code_does() {
        let live = |c: &mut Convo| {
            // The first batch is the log as it stood; ops after it are live.
            c.apply(&parse(r#"{"t":"n","n":1}"#).unwrap(), Instant::now());
        };
        let back = |c: &mut Convo, x: &str, q: bool| {
            c.apply(
                &parse(&format!(r#"{{"t":"back","x":"{x}","q":{q}}}"#)).unwrap(),
                Instant::now(),
            )
        };
        let shown = |c: &Convo| text(&draw(c, &Ui::new(), 40, 24, Instant::now()));
        // Caught before claude took it in: off the screen and back to the input.
        let mut c = convo();
        live(&mut c);
        c.sent("hello", Instant::now());
        assert_eq!(back(&mut c, "hello", false), Applied::Back("hello".into()));
        assert!(c.pending.is_empty());
        // Taken in, thinking, nothing drawn: the prompt and its note go, the text comes back.
        let mut c = convo();
        live(&mut c);
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"user","x":"essay"}"#,
                r#"{"t":"add","id":2,"k":"note","x":"Interrupted"}"#,
            ],
        );
        assert_eq!(back(&mut c, "essay", false), Applied::Back("essay".into()));
        assert!(
            !shown(&c)
                .iter()
                .any(|l| l.contains("essay") || l.contains("Interrupted"))
        );
        // Answered in part: it stays, under Claude Code's note.
        let mut c = convo();
        live(&mut c);
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"user","x":"count"}"#,
                r#"{"t":"add","id":2,"k":"text","x":"1 2 3"}"#,
                r#"{"t":"add","id":3,"k":"note","x":"Interrupted"}"#,
            ],
        );
        assert_eq!(back(&mut c, "count", false), Applied::Nothing);
        let got = shown(&c);
        let at = got.iter().position(|l| l == "● 1 2 3").unwrap();
        assert_eq!(got[at + 1], "  ⎿  Interrupted · What should Claude");
        // Another waiting behind it: the interrupt makes way for that, and this one stays.
        let mut c = convo();
        live(&mut c);
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"user","x":"rivers"}"#,
                r#"{"t":"add","id":2,"k":"note","x":"Interrupted"}"#,
            ],
        );
        assert_eq!(back(&mut c, "rivers", true), Applied::Nothing);
        assert!(shown(&c).iter().any(|l| l == "❯ rivers"));
        // Read back from the log, the rewind still shows, but nothing comes to the input.
        let mut c = Convo::new("claude-sessions", "claude-3", "run");
        feed(
            &mut c,
            &[
                r#"{"t":"add","id":1,"k":"user","x":"essay"}"#,
                r#"{"t":"add","id":2,"k":"note","x":"Interrupted"}"#,
            ],
        );
        assert_eq!(back(&mut c, "essay", false), Applied::Nothing);
        assert!(!shown(&c).iter().any(|l| l.contains("essay")));
    }

    #[test]
    fn a_prompt_sent_behind_a_turn_offers_send_now() {
        let mut c = convo();
        feed(
            &mut c,
            &[&meta("", "default"), r#"{"t":"st","s":"working"}"#],
        );
        c.sent("and lakes", Instant::now());
        assert!(c.pending[0].queued);
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        let at = got.iter().position(|l| l.ends_with("and lakes")).unwrap();
        assert_eq!(got[at + 1], "  ctrl+x ctrl+s to send now");
        // Sent at rest, it is the next turn itself, not one waiting.
        let mut c = convo();
        c.sent("hi", Instant::now());
        assert!(!c.pending[0].queued);
    }

    #[test]
    fn a_known_slash_command_is_lavender_wherever_it_is_in_the_prompt() {
        let runs = |t: &str| {
            let chars: Vec<char> = t.chars().collect();
            slash_runs(&chars, &["compact".to_string()])
        };
        assert_eq!(runs("/compact keep the tests"), vec![(0..8)]);
        assert_eq!(runs("please run /compact later"), vec![(11..19)]);
        assert_eq!(runs("/model"), vec![(0..6)], "the client's own");
        assert!(runs("/notacommand x").is_empty(), "an unknown one is text");
        assert!(runs("a/compact").is_empty(), "inside a word is text");
        assert!(runs("/").is_empty());
        // Drawn: the command lavender, the rest plain, as Claude Code's screen had it.
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        c.meta.commands = vec!["compact".into()];
        let mut ui = Ui::new();
        ui.input = "/compact keep the tests".into();
        ui.cursor = ui.input.chars().count();
        let f = draw(&c, &ui, 40, 24, Instant::now());
        let row = f
            .lines
            .iter()
            .find(|l| plain(l).starts_with("❯ /compact"))
            .unwrap();
        assert_eq!(row[1], ("/compact".to_string(), fg(LAVENDER)));
        assert_eq!(row[2], (" keep the tests".to_string(), PLAIN));
    }

    #[test]
    fn up_takes_queued_prompts_back_to_edit_as_claude_code_does() {
        let mut c = convo();
        feed(
            &mut c,
            &[&meta("", "default"), r#"{"t":"st","s":"working"}"#],
        );
        let a = c.sent("then lakes", Instant::now());
        let b = c.sent("and seas", Instant::now());
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        assert!(
            got.iter()
                .any(|l| l == "❯ Press up to edit queued messages"),
            "{got:?}"
        );
        assert_eq!(
            c.unqueue(),
            (vec!["then lakes".into(), "and seas".into()], vec![a, b])
        );
        assert!(c.pending.is_empty());
    }

    #[test]
    fn a_queued_prompt_taken_in_mid_turn_lands_between_the_tool_calls() {
        let mut c = convo();
        feed(
            &mut c,
            &[
                &meta("", "default"),
                r#"{"t":"add","id":1,"k":"user","x":"sleep three times"}"#,
                r#"{"t":"st","s":"working"}"#,
                r#"{"t":"add","id":2,"k":"tool","n":"Bash","a":"sleep 4"}"#,
            ],
        );
        c.sent("Also say BANANA", Instant::now());
        feed(
            &mut c,
            &[
                r#"{"t":"res","id":2,"ok":true,"p":[],"l":0}"#,
                r#"{"t":"add","id":3,"k":"user","x":"Also say BANANA"}"#,
                r#"{"t":"add","id":4,"k":"tool","n":"Bash","a":"sleep 4"}"#,
            ],
        );
        assert!(c.pending.is_empty(), "taken in: no longer waiting");
        let got = text(&draw(&c, &Ui::new(), 40, 24, Instant::now()));
        let ran = got
            .iter()
            .position(|l| l.trim() == "Ran 1 shell command")
            .unwrap();
        let said = got.iter().position(|l| l == "❯ Also say BANANA").unwrap();
        let running = got
            .iter()
            .position(|l| l.contains("Running 1 shell command"))
            .unwrap();
        assert!(ran < said && said < running, "{got:?}");
        assert!(!got.iter().any(|l| l.contains("send now")));
    }

    fn catalog() -> Vec<Command> {
        // As Claude Code 2.1.311 listed them for `/co` and `/` on 2026-11-02.
        [
            ("keepalive", "Use in a claude-sessions slot before you start or wait on something that will sit quiet", ""),
            ("mail-send", "Use when you need to tell another session's agent something, ask it a question", ""),
            ("copy", "Copy Claude's last response to clipboard (or /copy N for the Nth-latest)", ""),
            ("color", "Set the prompt bar color for this session", "[red|blue|green|yellow|purple|orange|pink|cyan|default]"),
            ("config", "Open settings", ""),
            ("model", "Set the AI model for Claude Code (currently Haiku 5.5)", ""),
            ("auto-mode-setup", "Teach auto mode about your environment, plus optional rule tweaks", ""),
        ]
        .iter()
        .map(|(n, a, h)| Command {
            name: n.to_string(),
            about: a.to_string(),
            hint: h.to_string(),
        })
        .collect()
    }

    #[test]
    fn a_name_typed_whole_comes_first_in_the_suggestion_list() {
        // claude 2.1.311's two `auto…` commands, two lines each at 40 wide, filled the five
        // rows, and `/auto` typed whole showed only them; `/mode` has `/model` before it.
        let mut ui = Ui::new();
        ui.catalog = [
            ("model", "Set the AI model for Claude Code"),
            (
                "auto-mode-setup",
                "Teach auto mode about your environment, plus optional rule tweaks",
            ),
        ]
        .iter()
        .map(|(n, a)| Command {
            name: n.to_string(),
            about: a.to_string(),
            hint: String::new(),
        })
        .collect();
        let names =
            |ui: &Ui| -> Vec<String> { suggestions(ui).into_iter().map(|c| c.name).collect() };
        ui.input = "/mode".into();
        assert_eq!(names(&ui), ["mode", "model", "auto-mode-setup"]);
        let drawn: Vec<String> = suggest_lines(&suggestions(&ui), 0, 40)
            .iter()
            .map(plain)
            .collect();
        assert!(drawn[0].starts_with("  ❯ /mode "), "{drawn:?}");
        // Calibration: short of the whole name, claude's order stands, the client's after.
        ui.input = "/mod".into();
        assert_eq!(names(&ui), ["model", "mode", "auto-mode-setup"]);
    }

    #[test]
    fn the_suggestion_list_is_drawn_as_claude_codes_at_40_and_70() {
        let list: Vec<Command> = catalog().into_iter().skip(2).take(3).collect();
        let at = |w: usize, sel: usize| {
            suggest_lines(&list, sel, w)
                .iter()
                .map(|l| plain(l).trim_end().to_string())
                .collect::<Vec<_>>()
        };
        // Read off Claude Code's screen, `/co` typed.
        assert_eq!(
            at(40, 0),
            [
                "  ❯ /copy           Copy Claude's last",
                "                    response to clipb…",
                "    /color          Set the prompt bar",
                "                    color for this se…",
                "    /config         Open settings",
            ]
        );
        assert_eq!(
            at(70, 0),
            [
                "  ❯ /copy                       Copy Claude's last response to",
                "                                clipboard (or /copy N for the Nth-l…",
                "    /color                      Set the prompt bar color for this",
                "                                session",
                "    /config                     Open settings",
            ]
        );
        // The chosen one lavender, the rest grey.
        let lines = suggest_lines(&list, 1, 40);
        assert_eq!(lines[0][0].1, fg(GREY));
        assert_eq!(lines[2][0].1, fg(LAVENDER));
        // Five lines at most, scrolled to the chosen one; a long name keeps its end.
        let all = catalog();
        let got: Vec<String> = suggest_lines(&all, 6, 40)
            .iter()
            .map(|l| plain(l).trim_end().to_string())
            .collect();
        assert!(got.len() <= 5, "{got:?}");
        assert!(
            got.iter().any(|l| l.starts_with("  ❯ …to-mode-setup  ")),
            "{got:?}"
        );
    }

    #[test]
    fn the_list_offers_what_starts_with_the_name_then_what_has_it() {
        let mut ui = Ui::new();
        ui.catalog = catalog();
        let names = |ui: &Ui| {
            suggestions(ui)
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
        };
        ui.input = "/mod".into();
        assert_eq!(
            names(&ui),
            ["model", "mode", "auto-mode-setup"],
            "as Claude Code's `/mod`, with the view's own `/mode`"
        );
        ui.input = "/co".into();
        assert_eq!(
            names(&ui),
            ["copy", "color", "config", "disconnect", "remote-control"]
        );
        ui.input = "/exp".into();
        assert_eq!(names(&ui), ["expand"], "the phone's own");
        // A space ends it, Esc puts it away, a dialog has its own keys.
        ui.input = "/co ".into();
        assert!(names(&ui).is_empty());
        ui.input = "/co".into();
        ui.suggest_off = true;
        assert!(names(&ui).is_empty());
        ui.suggest_off = false;
        ui.dialog = Some(Dialog::Effort { sel: 0 });
        assert!(names(&ui).is_empty());
        ui.input = "plain".into();
        ui.dialog = None;
        assert!(names(&ui).is_empty());
    }

    #[test]
    fn the_list_sits_over_the_prompt_and_a_chosen_command_shows_its_hint() {
        let mut c = convo();
        feed(&mut c, &[&meta("", "default")]);
        let mut ui = Ui::new();
        ui.catalog = catalog();
        ui.input = "/co".into();
        ui.cursor = 3;
        let f = draw(&c, &ui, 40, 24, Instant::now());
        let got = text(&f);
        let at = got
            .iter()
            .position(|l| l == "  ❯ /copy           Copy Claude's last")
            .unwrap();
        assert!(
            got[at + 5].starts_with("────"),
            "the box's rule under the list: {got:?}"
        );
        assert_eq!(got[at + 6], "❯ /co");
        assert_eq!(
            f.cursor,
            Some((at + 6, 5)),
            "the cursor in the box, past the list"
        );
        ui.input = "/color ".into();
        ui.cursor = 7;
        let got = text(&draw(&c, &ui, 70, 24, Instant::now()));
        assert!(
            got.iter()
                .any(|l| l == "❯ /color  [red|blue|green|yellow|purple|orange|pink|cyan|default]"),
            "{got:?}"
        );
    }

    #[test]
    fn scrolling_back_moves_a_line_a_step() {
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
            bottom.iter().any(|l| l.starts_with("❯ again")),
            "the last prompt under the count, not alone on top: {bottom:?}"
        );
        assert!(bottom[0].trim().parse::<u32>().is_ok(), "{bottom:?}");
        ui.scroll = 1;
        let back = text(&draw(&c, &ui, 40, 24, Instant::now()));
        // Under the pinned line, the line that was just above the screen, then what was on top.
        assert_eq!(back[2], bottom[0], "{back:?}");
        let (body, _) = transcript(&c, 40);
        assert_eq!(
            max_scroll(&c, &ui, 40, 24),
            body.len() - (24 - 5),
            "back as far as the top"
        );
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
        ui.dialog = Some(Dialog::Effort { sel: 2 });
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        assert_eq!(got[15..], mockup("R6")[15..]);

        // `/mode`, a turn running in auto mode: the session's own ticked, the cursor on it.
        c.entries.clear();
        c.local(Entry::Prompt("/mode".into()));
        feed(&mut c, &[&meta("fix the door", "auto")]);
        ui.dialog = Some(Dialog::Mode { sel: 3 });
        let got = text(&draw(&c, &ui, 40, 24, Instant::now()));
        assert_eq!(got[13..], mockup("R11")[13..], "{got:#?}");
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
        ui.load = Some(figures(0.9, 2.0, 70));
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
        let got = &text(&f)[22];
        let want = &mockup("R8")[22];
        // The spinner's frame is the clock's; the rest is the mockup's.
        let rest = |l: &str| l.trim_start().chars().skip(1).collect::<String>();
        assert_eq!(rest(got), rest(want), "{got:?}");
        assert!(
            crate::ui::SPINNER
                .iter()
                .any(|g| got.trim_start().starts_with(g)),
            "{got:?}"
        );
    }

    #[test]
    fn reconnecting_goes_after_the_status_line_where_both_fit_and_in_its_place_where_not() {
        let f = figures(0.9, 2.0, 70);
        let down = "⠋ Reconnecting… (8s)";
        let at = |w: usize| {
            plain(&reconnecting_line(
                "claude-sessions/claude-3",
                Some(&f),
                down,
                w,
            ))
        };
        assert_eq!(
            at(80),
            "  claude-sessions/claude-3 · RAM: 0.9G / 2.0G, Load: 0.7 · ⠋ Reconnecting… (8s)"
        );
        assert_eq!(
            at(70),
            "  RAM: 0.9G / 2.0G, Load: 0.7, claude-sessions · ⠋ Reconnecting… (8s)"
        );
        assert_eq!(
            at(60),
            "  RAM: 0.9G / 2.0G, Load: 0.7 · ⠋ Reconnecting… (8s)"
        );
        assert_eq!(at(40), "  ⠋ Reconnecting… (8s)");
        assert_eq!(
            plain(&reconnecting_line("x/y", None, down, 80)),
            "  ⠋ Reconnecting… (8s)"
        );
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
        assert_eq!(mode_named("manual"), Some("default"));
        assert_eq!(mode_named("Accept"), Some("acceptEdits"));
        assert_eq!(mode_named("accept edits"), Some("acceptEdits"));
        assert_eq!(mode_named("bypassPermissions"), None);
        assert_eq!(
            mode_line("default"),
            Some(("⏸ manual mode on".into(), MANUAL_GREY))
        );
        assert_eq!(mode_line(""), None, "nothing before claude says");
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
