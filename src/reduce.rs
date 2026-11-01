//! Claude's stream-json lines reduced to the few items the remote view draws, in the hub,
//! before the metered link (`docs/remote-view.md` § *The pieces*).
//!
//! A small session wrote 177 KB of stream-json; most of it is a tool and command list re-sent
//! before every turn, hook progress, token estimates and per-delta stream events. What the view
//! needs is what Claude Code itself draws: the prompts, the assistant's text as it streams, a
//! line per tool call with a few lines of its result, a prompt or question waiting on an
//! answer, a note per local command, a compaction marker, and whether claude is working. So
//! the reducer keeps those, numbers each item, and turns the rest into nothing.
//!
//! **Pure**: lines in, ops out, no clock and no I/O. Batching streamed text into one write a
//! tenth of a second is the relay's (`coalesce` merges what it has gathered), and so is
//! numbering ops for resume. A full tool result stays here, in memory, sent only when the view
//! asks for it (`full_result`).
//!
//! The wire form of an op is a JSON object with short keys (`Op::to_value`), one per line. It
//! is read tolerantly by the view: an op type or field it does not know is skipped.

use std::collections::HashMap;

use crate::json::Value;
use crate::sdk::{self, Block, Msg, Question};

/// How many lines of a tool's result the view is sent before it asks for the rest: Claude
/// Code's collapsed result shows a few lines and a count of the others.
pub const PREVIEW_LINES: usize = 3;
/// How long a single line of a preview, or a tool call's argument, may be. Anything longer is
/// cut, with an ellipsis, since it would wrap to many rows on a phone anyway.
pub const LINE_MAX: usize = 200;

/// One change to what the view draws.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// A new item, numbered `id`, drawn after the others.
    Add { id: u64, item: Item },
    /// More text streamed into a text item.
    Append { id: u64, text: String },
    /// A text item's whole text, when what streamed differs from what was finished.
    SetText { id: u64, text: String },
    /// A tool call's result: whether it succeeded, its first lines, and how many lines in all.
    Result {
        id: u64,
        ok: bool,
        preview: Vec<String>,
        lines: usize,
    },
    /// A permission prompt or question is no longer waiting, and why: `allowed`, `denied`,
    /// `answered`, `withdrawn` (claude cancelled it), or `answered elsewhere`.
    Resolved { id: u64, how: String },
    /// What claude is doing: `working`, `compacting`, `waiting` (on an answer) or `idle`.
    State(String),
    /// The conversation's model, permission mode, title and id, sent when one changes.
    Meta(Meta),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Meta {
    pub session_id: String,
    pub model: String,
    pub permission_mode: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// What the owner typed, as claude echoed it.
    Prompt(String),
    /// The assistant's text; may grow by `Append`.
    Text(String),
    /// A tool call: its name and the one argument Claude Code shows beside it.
    Tool { name: String, arg: String },
    /// A tool waiting on the owner's permission.
    Permission {
        request_id: String,
        tool: String,
        arg: String,
        description: String,
    },
    /// An `AskUserQuestion` waiting on the owner's answers.
    Ask {
        request_id: String,
        questions: Vec<Question>,
    },
    /// A line of its own: a local command's output, an interruption, an error.
    Note(String),
    /// The conversation was compacted, from and to so many tokens.
    Compacted { pre: Option<u64>, post: Option<u64> },
}

/// What one line of claude's output came to.
#[derive(Debug, Default, PartialEq)]
pub struct Out {
    pub ops: Vec<Op>,
    /// Lines claude is owed at once: the refusal of a request the view does not serve.
    pub to_claude: Vec<Value>,
}

/// The state that turns a stream of lines into items.
#[derive(Debug, Default)]
pub struct Reducer {
    next_id: u64,
    /// Text items being streamed, by (message, block index).
    streaming: HashMap<(String, u64), u64>,
    /// Each streamed item's text so far, and whether its finished block has arrived.
    texts: HashMap<u64, (String, bool)>,
    /// The streamed text items of each message, in the order they were opened.
    by_message: HashMap<String, Vec<u64>>,
    /// Tool call items by tool-use id.
    tools: HashMap<String, u64>,
    /// Waiting prompts and questions by request id, with the input claude sent, which an
    /// answer must carry back.
    waiting: HashMap<String, (u64, Value)>,
    /// Every tool result's full text, by item, for when the view asks.
    results: HashMap<u64, String>,
    meta: Meta,
    state: String,
    /// The next user text is a compaction's summary, which Claude Code does not draw.
    after_compact: bool,
}

impl Reducer {
    pub fn new() -> Reducer {
        Reducer::default()
    }

    fn add(&mut self, out: &mut Out, item: Item) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        out.ops.push(Op::Add { id, item });
        id
    }

    fn set_state(&mut self, out: &mut Out, state: &str) {
        if self.state != state {
            self.state = state.to_string();
            out.ops.push(Op::State(state.to_string()));
        }
    }

    fn set_meta(&mut self, out: &mut Out, meta: Meta) {
        if self.meta != meta {
            self.meta = meta.clone();
            out.ops.push(Op::Meta(meta));
        }
    }

    /// The full text of a tool result the view was sent a preview of.
    pub fn full_result(&self, id: u64) -> Option<&str> {
        self.results.get(&id).map(String::as_str)
    }

    /// The input claude sent with a waiting request, which an answer carries back.
    pub fn waiting_input(&self, request_id: &str) -> Option<&Value> {
        self.waiting.get(request_id).map(|(_, input)| input)
    }

    /// A waiting prompt was answered by a view: it stops waiting, with `how` said. `None` when
    /// it was not waiting — answered already, by another view or by claude's withdrawal — so
    /// the relay sends claude nothing and tells that view `answered elsewhere`.
    pub fn answered(&mut self, request_id: &str, how: &str) -> Option<Out> {
        let (id, _) = self.waiting.remove(request_id)?;
        let mut out = Out::default();
        out.ops.push(Op::Resolved {
            id,
            how: how.to_string(),
        });
        if self.waiting.is_empty() && self.state == "waiting" {
            self.set_state(&mut out, "working");
        }
        Some(out)
    }

    /// One line from claude.
    pub fn feed(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        // A subagent's own messages are drawn by Claude Code as progress under its Task call,
        // not as the conversation's; the call's result is what the view shows.
        if sdk::parent_tool_use_id(v).is_some() {
            if let Msg::Permission { .. } | Msg::Question { .. } = sdk::classify(v) {
                // A subagent's permission prompt still needs the owner.
            } else {
                return out;
            }
        }
        match sdk::classify(v) {
            Msg::Init {
                session_id,
                model,
                permission_mode,
            } => {
                let title = self.meta.title.clone();
                self.set_meta(
                    &mut out,
                    Meta {
                        session_id,
                        model,
                        permission_mode,
                        title,
                    },
                );
            }
            Msg::TitleChanged(title) => {
                let meta = Meta {
                    title,
                    ..self.meta.clone()
                };
                self.set_meta(&mut out, meta);
            }
            Msg::Status(status) => match status.as_deref() {
                Some("compacting") => self.set_state(&mut out, "compacting"),
                Some(_) => self.set_state(&mut out, "working"),
                None => {}
            },
            Msg::Prompt(text) => {
                self.add(&mut out, Item::Prompt(text));
                self.set_state(&mut out, "working");
            }
            Msg::TextDelta {
                message_id,
                index,
                text,
            } => {
                let key = (message_id.clone(), index);
                if let Some(&id) = self.streaming.get(&key) {
                    if let Some((so_far, _)) = self.texts.get_mut(&id) {
                        so_far.push_str(&text);
                    }
                    out.ops.push(Op::Append { id, text });
                } else {
                    let id = self.add(&mut out, Item::Text(text.clone()));
                    self.streaming.insert(key, id);
                    self.texts.insert(id, (text, false));
                    self.by_message.entry(message_id).or_default().push(id);
                }
            }
            Msg::Assistant { message_id, blocks } => {
                for b in blocks {
                    match b {
                        Block::Text(text) => self.finish_text(&mut out, &message_id, text),
                        Block::ToolUse { id, name, input } => {
                            let arg = tool_arg(&name, &input);
                            let item = self.add(&mut out, Item::Tool { name, arg });
                            self.tools.insert(id, item);
                        }
                        Block::Other(_) => {}
                    }
                }
            }
            Msg::ToolResults(results) => {
                for r in results {
                    let Some(&id) = self.tools.get(&r.tool_use_id) else {
                        continue;
                    };
                    let (preview, lines) = preview(&r.text);
                    self.results.insert(id, r.text);
                    out.ops.push(Op::Result {
                        id,
                        ok: !r.is_error,
                        preview,
                        lines,
                    });
                }
            }
            Msg::LocalOutput(text) => {
                let text = strip_ansi(&text);
                if !text.trim().is_empty() {
                    self.add(&mut out, Item::Note(text.trim().to_string()));
                }
            }
            Msg::UserText(text) => {
                if std::mem::take(&mut self.after_compact) {
                    // The compaction's summary, for the model; Claude Code draws a marker.
                } else if text.starts_with("[Request interrupted by user") {
                    self.add(&mut out, Item::Note("Interrupted".into()));
                }
            }
            Msg::Compacted {
                pre_tokens,
                post_tokens,
            } => {
                self.after_compact = true;
                self.add(
                    &mut out,
                    Item::Compacted {
                        pre: pre_tokens,
                        post: post_tokens,
                    },
                );
            }
            Msg::Permission {
                request_id,
                tool,
                input,
                description,
            } => {
                let arg = tool_arg(&tool, &input);
                let id = self.add(
                    &mut out,
                    Item::Permission {
                        request_id: request_id.clone(),
                        tool,
                        arg,
                        description: description.unwrap_or_default(),
                    },
                );
                self.waiting.insert(request_id, (id, input));
                self.set_state(&mut out, "waiting");
            }
            Msg::Question {
                request_id,
                input,
                questions,
            } => {
                let id = self.add(
                    &mut out,
                    Item::Ask {
                        request_id: request_id.clone(),
                        questions,
                    },
                );
                self.waiting.insert(request_id, (id, input));
                self.set_state(&mut out, "waiting");
            }
            Msg::Cancelled { request_id } => {
                if let Some(o) = self.answered(&request_id, "withdrawn") {
                    out.ops.extend(o.ops);
                }
            }
            Msg::OtherRequest {
                request_id,
                subtype,
            } => {
                out.to_claude.push(sdk::refuse(
                    &request_id,
                    &format!("{subtype} is not served by the remote view"),
                ));
            }
            Msg::Result {
                subtype, is_error, ..
            } => {
                // Prompts still waiting when a turn ends were withdrawn with it.
                let left: Vec<String> = self.waiting.keys().cloned().collect();
                for r in left {
                    if let Some(o) = self.answered(&r, "withdrawn") {
                        out.ops.extend(o.ops);
                    }
                }
                if is_error && subtype != "error_during_execution" {
                    self.add(&mut out, Item::Note(format!("Error: {subtype}")));
                }
                self.streaming.clear();
                self.by_message.clear();
                self.texts.clear();
                self.set_state(&mut out, "idle");
            }
            Msg::Response { .. } | Msg::Noise | Msg::Unknown(_) => {}
        }
        out
    }

    /// A finished text block: the first of its message's streamed items not yet finished, put
    /// right if what streamed differs; or a new item, when nothing streamed (no partial
    /// messages, or a line read back from a log).
    fn finish_text(&mut self, out: &mut Out, message_id: &str, text: String) {
        let open = self.by_message.get(message_id).and_then(|ids| {
            ids.iter()
                .copied()
                .find(|id| self.texts.get(id).is_some_and(|(_, done)| !done))
        });
        match open {
            Some(id) => {
                let (so_far, done) = self.texts.get_mut(&id).expect("open item has text");
                *done = true;
                if *so_far != text {
                    *so_far = text.clone();
                    out.ops.push(Op::SetText { id, text });
                }
            }
            None => {
                if !text.is_empty() {
                    self.add(out, Item::Text(text));
                }
            }
        }
    }
}

/// Merge a batch of ops for one write: consecutive appends to one item become one.
pub fn coalesce(ops: Vec<Op>) -> Vec<Op> {
    let mut merged: Vec<Op> = Vec::with_capacity(ops.len());
    for op in ops {
        if let (
            Some(Op::Append {
                id: last,
                text: so_far,
            }),
            Op::Append { id, text },
        ) = (merged.last_mut(), &op)
        {
            if last == id {
                so_far.push_str(text);
                continue;
            }
        }
        merged.push(op);
    }
    merged
}

/// Cut a line to `LINE_MAX` characters, with an ellipsis when cut.
fn cut(s: &str) -> String {
    if s.chars().count() <= LINE_MAX {
        return s.to_string();
    }
    let mut t: String = s.chars().take(LINE_MAX - 1).collect();
    t.push('…');
    t
}

/// The first lines of a tool result and how many lines it has, trailing blank lines aside.
fn preview(text: &str) -> (Vec<String>, usize) {
    let text = strip_ansi(text);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let shown = lines.iter().take(PREVIEW_LINES).map(|l| cut(l)).collect();
    (shown, lines.len())
}

/// The argument Claude Code shows beside a tool call's name: the command, the path, the
/// pattern. For a tool it does not know, the first string in its input.
fn tool_arg(name: &str, input: &Value) -> String {
    let key = match name {
        "Bash" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" | "Agent" => "description",
        "Skill" => "skill",
        _ => "",
    };
    let found = input
        .get(key)
        .and_then(Value::as_str)
        .or_else(|| match input {
            Value::Obj(m) if key.is_empty() => m.values().find_map(Value::as_str),
            _ => None,
        });
    // A command's first line, as Claude Code draws a multi-line one.
    cut(found.unwrap_or("").lines().next().unwrap_or(""))
}

/// Remove the terminal escapes a local command's output may carry (claude colours `/model`'s
/// answer, say): the view draws its own styles.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Parameters and intermediates, then one final byte in @..~.
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---- The wire ----

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut o = Value::obj();
    for (k, v) in pairs {
        o.set(k, v);
    }
    o
}

fn opt_num(n: Option<u64>) -> Value {
    n.map(|n| Value::num(n as f64)).unwrap_or(Value::Null)
}

impl Op {
    /// The op as the view is sent it: `t` its type, then short keys.
    pub fn to_value(&self) -> Value {
        let id = |id: &u64| ("id", Value::num(*id as f64));
        match self {
            Op::Add { id: n, item } => {
                let mut v = item.to_value();
                v.set("t", Value::string("add"));
                v.set("id", Value::num(*n as f64));
                v
            }
            Op::Append { id: n, text } => obj(vec![
                ("t", Value::string("app")),
                id(n),
                ("x", Value::string(text.clone())),
            ]),
            Op::SetText { id: n, text } => obj(vec![
                ("t", Value::string("set")),
                id(n),
                ("x", Value::string(text.clone())),
            ]),
            Op::Result {
                id: n,
                ok,
                preview,
                lines,
            } => obj(vec![
                ("t", Value::string("res")),
                id(n),
                ("ok", Value::Bool(*ok)),
                (
                    "p",
                    Value::Arr(preview.iter().cloned().map(Value::string).collect()),
                ),
                ("l", Value::num(*lines as f64)),
            ]),
            Op::Resolved { id: n, how } => obj(vec![
                ("t", Value::string("done")),
                id(n),
                ("x", Value::string(how.clone())),
            ]),
            Op::State(s) => obj(vec![
                ("t", Value::string("st")),
                ("s", Value::string(s.clone())),
            ]),
            Op::Meta(m) => obj(vec![
                ("t", Value::string("meta")),
                ("sid", Value::string(m.session_id.clone())),
                ("m", Value::string(m.model.clone())),
                ("pm", Value::string(m.permission_mode.clone())),
                ("ti", Value::string(m.title.clone())),
            ]),
        }
    }
}

impl Item {
    fn to_value(&self) -> Value {
        let k = |k: &str| ("k", Value::string(k));
        match self {
            Item::Prompt(x) => obj(vec![k("user"), ("x", Value::string(x.clone()))]),
            Item::Text(x) => obj(vec![k("text"), ("x", Value::string(x.clone()))]),
            Item::Note(x) => obj(vec![k("note"), ("x", Value::string(x.clone()))]),
            Item::Tool { name, arg } => obj(vec![
                k("tool"),
                ("n", Value::string(name.clone())),
                ("a", Value::string(arg.clone())),
            ]),
            Item::Permission {
                request_id,
                tool,
                arg,
                description,
            } => obj(vec![
                k("perm"),
                ("r", Value::string(request_id.clone())),
                ("n", Value::string(tool.clone())),
                ("a", Value::string(arg.clone())),
                ("d", Value::string(description.clone())),
            ]),
            Item::Ask {
                request_id,
                questions,
            } => obj(vec![
                k("ask"),
                ("r", Value::string(request_id.clone())),
                (
                    "q",
                    Value::Arr(
                        questions
                            .iter()
                            .map(|q| {
                                obj(vec![
                                    ("q", Value::string(q.question.clone())),
                                    ("h", Value::string(q.header.clone())),
                                    ("m", Value::Bool(q.multi_select)),
                                    (
                                        "o",
                                        Value::Arr(
                                            q.options
                                                .iter()
                                                .map(|(l, d)| {
                                                    Value::Arr(vec![
                                                        Value::string(l.clone()),
                                                        Value::string(d.clone()),
                                                    ])
                                                })
                                                .collect(),
                                        ),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
            Item::Compacted { pre, post } => obj(vec![
                k("compact"),
                ("b", opt_num(*pre)),
                ("a", opt_num(*post)),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{parse, to_line};

    fn feed_all(r: &mut Reducer, lines: &[&str]) -> Out {
        let mut all = Out::default();
        for l in lines {
            let o = r.feed(&parse(l).expect("fixture parses"));
            all.ops.extend(o.ops);
            all.to_claude.extend(o.to_claude);
        }
        all
    }

    fn wire(ops: &[Op]) -> Vec<String> {
        ops.iter().map(|o| to_line(&o.to_value())).collect()
    }

    // A turn as 2.1.311 wrote it on 2026-11-01, cut to the fields read: a prompt echoed, a
    // tool call that asked permission, its result, streamed text, the end of the turn, with
    // the noise between that the reducer must drop.
    const TURN: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-haiku-5-5","permissionMode":"default","tools":["Bash","Read"]}"#,
        r#"{"type":"user","message":{"role":"user","content":"Make a file."},"isReplay":true,"parent_tool_use_id":null}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        r#"{"type":"system","subtype":"hook_started","hook_name":"UserPromptSubmit"}"#,
        r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50}"#,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}},"api_message_id":"msg_1"}"#,
        r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"touch spike-file && ls","description":"Create it"}}]}}"#,
        r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch spike-file && ls","description":"Create it"},"description":"Create it"}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
    ];
    const AFTER_ALLOW: &[&str] = &[
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"a\nb\nc\nd\nspike-file\n","is_error":false}]},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Do"}},"api_message_id":"msg_2"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ne"}},"api_message_id":"msg_2"}"#,
        r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Done"}]}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s1"}"#,
    ];

    #[test]
    fn a_turn_reduces_to_what_claude_code_draws() {
        let mut r = Reducer::new();
        let before = feed_all(&mut r, TURN);
        assert_eq!(
            wire(&before.ops),
            [
                r#"{"m":"claude-haiku-5-5","pm":"default","sid":"s1","t":"meta","ti":""}"#,
                r#"{"id":1,"k":"user","t":"add","x":"Make a file."}"#,
                r#"{"s":"working","t":"st"}"#,
                r#"{"a":"touch spike-file && ls","id":2,"k":"tool","n":"Bash","t":"add"}"#,
                r#"{"a":"touch spike-file && ls","d":"Create it","id":3,"k":"perm","n":"Bash","r":"req-1","t":"add"}"#,
                r#"{"s":"waiting","t":"st"}"#,
            ]
        );
        assert!(before.to_claude.is_empty());
        assert_eq!(
            r.waiting_input("req-1")
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str),
            Some("touch spike-file && ls"),
            "an answer must carry claude's input back"
        );

        let answered = r.answered("req-1", "allowed").expect("it was waiting");
        assert_eq!(
            wire(&answered.ops),
            [
                r#"{"id":3,"t":"done","x":"allowed"}"#,
                r#"{"s":"working","t":"st"}"#
            ]
        );
        assert!(
            r.answered("req-1", "allowed").is_none(),
            "a second answer is too late"
        );

        let after = feed_all(&mut r, AFTER_ALLOW);
        assert_eq!(
            wire(&after.ops),
            [
                r#"{"id":2,"l":5,"ok":true,"p":["a","b","c"],"t":"res"}"#,
                r#"{"id":4,"k":"text","t":"add","x":"Do"}"#,
                r#"{"id":4,"t":"app","x":"ne"}"#,
                r#"{"s":"idle","t":"st"}"#,
            ],
            "the finished text matched what streamed, so nothing is resent"
        );
        assert_eq!(r.full_result(2), Some("a\nb\nc\nd\nspike-file\n"));
    }

    #[test]
    fn the_reduction_is_a_small_fraction_of_what_claude_wrote() {
        // Calibrated against the real thing: the spike's whole turn was ~18 KB of stream-json
        // for a one-line answer. Here the fixture is already cut; the property held is that
        // noise produces nothing at all.
        let mut r = Reducer::new();
        let noise = [
            r#"{"type":"system","subtype":"hook_response","exit_code":0,"stdout":""}"#,
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":237}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#,
            r#"{"type":"rate_limit_event"}"#,
            r#"{"type":"keep_alive"}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"c1","response":{"commands":[]}}}"#,
            r#"{"type":"system","subtype":"something_new_in_2_1_400"}"#,
        ];
        assert_eq!(feed_all(&mut r, &noise), Out::default());
        // And an unchanged system/init before the next turn says nothing either.
        feed_all(&mut r, &TURN[..1]);
        assert_eq!(feed_all(&mut r, &TURN[..1]), Out::default());
    }

    #[test]
    fn a_finished_block_that_differs_from_its_stream_is_put_right() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"api_message_id":"m"}"#,
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"Hello"}]}}"#,
                // A message that never streamed, as from a log read back.
                r#"{"type":"assistant","message":{"id":"n","content":[{"type":"text","text":"Whole"}]}}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"k":"text","t":"add","x":"Hel"}"#,
                r#"{"id":1,"t":"set","x":"Hello"}"#,
                r#"{"id":2,"k":"text","t":"add","x":"Whole"}"#,
            ]
        );
    }

    #[test]
    fn two_text_blocks_of_one_message_finish_in_order() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one"}},"api_message_id":"m"}"#,
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"two"}},"api_message_id":"m"}"#,
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"one"}]}}"#,
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"two"}]}}"#,
            ],
        );
        assert_eq!(
            o.ops.len(),
            2,
            "two adds, nothing resent: {:?}",
            wire(&o.ops)
        );
    }

    #[test]
    fn a_question_waits_and_a_withdrawn_one_says_so() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Red or blue?","header":"Color","options":[{"label":"Red","description":"r"}],"multiSelect":false}]}}}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"k":"ask","q":[{"h":"Color","m":false,"o":[["Red","r"]],"q":"Red or blue?"}],"r":"q1","t":"add"}"#,
                r#"{"s":"waiting","t":"st"}"#,
            ]
        );
        let o = feed_all(
            &mut r,
            &[r#"{"type":"control_cancel_request","request_id":"q1"}"#],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"t":"done","x":"withdrawn"}"#,
                r#"{"s":"working","t":"st"}"#
            ],
            "nothing waits any more, so claude is back at work"
        );
        assert!(r.answered("q1", "answered").is_none());
    }

    #[test]
    fn a_turn_that_ends_withdraws_what_was_waiting() {
        let mut r = Reducer::new();
        feed_all(&mut r, TURN);
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s1"}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":3,"t":"done","x":"withdrawn"}"#,
                r#"{"s":"working","t":"st"}"#,
                r#"{"s":"idle","t":"st"}"#,
            ],
            "an interrupted turn is not an error worth a note"
        );
    }

    #[test]
    fn local_commands_compaction_and_interrupts_are_notes() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to \u001b[1msonnet\u001b[22m</local-command-stdout>"}}"#,
                r#"{"type":"system","subtype":"status","status":"compacting"}"#,
                r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"pre_tokens":29738,"post_tokens":8737}}"#,
                r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation..."}}"#,
                r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Compacted </local-command-stdout>"}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"k":"note","t":"add","x":"Set model to sonnet"}"#,
                r#"{"s":"compacting","t":"st"}"#,
                r#"{"a":8737,"b":29738,"id":2,"k":"compact","t":"add"}"#,
                r#"{"id":3,"k":"note","t":"add","x":"Compacted"}"#,
                r#"{"id":4,"k":"note","t":"add","x":"Interrupted"}"#,
            ]
        );
    }

    #[test]
    fn a_request_the_view_does_not_serve_is_refused_at_once() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback"}}"#,
            ],
        );
        assert!(o.ops.is_empty());
        assert_eq!(
            o.to_claude.iter().map(to_line).collect::<Vec<_>>(),
            [
                r#"{"response":{"error":"hook_callback is not served by the remote view","request_id":"h1","subtype":"error"},"type":"control_response"}"#
            ]
        );
    }

    #[test]
    fn a_subagents_lines_stay_under_its_call_but_its_prompts_reach_the_owner() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_t","name":"Task","input":{"description":"Find it","prompt":"long"}}]}}"#,
                r#"{"type":"assistant","message":{"id":"inner","content":[{"type":"text","text":"subagent talk"}]},"parent_tool_use_id":"toolu_t"}"#,
                r#"{"type":"control_request","request_id":"p","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm x"}},"parent_tool_use_id":"toolu_t"}"#,
            ],
        );
        let lines = wire(&o.ops);
        assert_eq!(
            lines[0],
            r#"{"a":"Find it","id":1,"k":"tool","n":"Task","t":"add"}"#
        );
        assert!(
            !lines.iter().any(|l| l.contains("subagent talk")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains(r#""k":"perm""#)),
            "{lines:?}"
        );
    }

    #[test]
    fn appends_coalesce_per_item_and_nothing_else_merges() {
        let ops = vec![
            Op::Append {
                id: 1,
                text: "a".into(),
            },
            Op::Append {
                id: 1,
                text: "b".into(),
            },
            Op::Append {
                id: 2,
                text: "c".into(),
            },
            Op::State("idle".into()),
            Op::Append {
                id: 2,
                text: "d".into(),
            },
        ];
        assert_eq!(
            coalesce(ops),
            vec![
                Op::Append {
                    id: 1,
                    text: "ab".into()
                },
                Op::Append {
                    id: 2,
                    text: "c".into()
                },
                Op::State("idle".into()),
                Op::Append {
                    id: 2,
                    text: "d".into()
                },
            ]
        );
    }

    #[test]
    fn tool_arguments_and_previews_are_cut_for_a_phone() {
        let long = "x".repeat(500);
        let input = parse(&format!(r#"{{"command":"{long}\nsecond line"}}"#)).unwrap();
        let arg = tool_arg("Bash", &input);
        assert_eq!(arg.chars().count(), LINE_MAX);
        assert!(arg.ends_with('…'));
        assert_eq!(
            tool_arg("mcp__docs__guide", &parse(r#"{"topic":"index"}"#).unwrap()),
            "index",
            "an unknown tool shows its first string"
        );
        assert_eq!(
            tool_arg("TodoWrite", &parse(r#"{"todos":[]}"#).unwrap()),
            ""
        );
        let (p, n) = preview("\u{1b}[31mred\u{1b}[0m\n\n");
        assert_eq!((p, n), (vec!["red".to_string()], 1));
    }
}
