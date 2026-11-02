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
    /// `answered`, `withdrawn` (claude cancelled it), or `answered elsewhere`; a question's
    /// answers keyed by its text, for the transcript to say what was chosen.
    Resolved {
        id: u64,
        how: String,
        answers: Vec<(String, String)>,
    },
    /// What runs under a subagent or workflow call now, and whether it is `running` or has
    /// ended (`completed`, `failed`, `stopped`): replaces the lines sent before.
    Progress {
        id: u64,
        lines: Vec<String>,
        state: String,
    },
    /// What claude is doing: `working`, `compacting`, `waiting` (on an answer) or `idle`.
    State(String),
    /// The conversation's model, permission mode, title and id, sent when one changes.
    Meta(Meta),
    /// An interrupt caught a prompt a view sent (`command_lifecycle` `cancelled`): its text,
    /// and whether another prompt was waiting behind it. The view decides, as Claude Code
    /// does, whether that hands the prompt back to its input (`convo.rs`).
    Back {
        text: String,
        uuid: String,
        queued: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Meta {
    pub session_id: String,
    pub model: String,
    pub permission_mode: String,
    pub title: String,
    /// The effort a view set, empty for the model's default: claude's own lines never say it.
    pub effort: String,
    /// Where claude runs, and its version, for the logo block Claude Code draws on top.
    pub cwd: String,
    pub version: String,
    /// The slash commands claude has, for the view to colour as Claude Code colours them.
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// What the owner typed, as claude echoed it.
    Prompt(String),
    /// The assistant's text; may grow by `Append`.
    Text(String),
    /// A tool call: its name and the one argument Claude Code shows beside it.
    Tool { name: String, arg: String },
    /// A subagent or a workflow: drawn on its own, as Claude Code does, not folded into a run
    /// of tool calls. `name` is the agent's type (`Explore`, `Agent`) or `Workflow`, `arg`
    /// its description.
    Agent { name: String, arg: String },
    /// A tool waiting on the owner's permission. `always` is the label of Claude Code's
    /// "Yes, and don't ask again" option, empty when claude suggested no rule to add.
    Permission {
        request_id: String,
        tool: String,
        arg: String,
        description: String,
        always: String,
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
    /// The rules a waiting permission prompt's "don't ask again" would add, by request id.
    suggestions: HashMap<String, Vec<Value>>,
    /// Every tool result's full text, by item, for when the view asks.
    results: HashMap<u64, String>,
    /// Subagents and workflows, by the tool-use id that started them.
    tasks: HashMap<String, TaskView>,
    meta: Meta,
    state: String,
    /// The next user text is a compaction's summary, which Claude Code does not draw.
    after_compact: bool,
    /// Prompts views sent with a uuid, read back from the holder's log, and where each has got
    /// to by claude's `command_lifecycle`: until it ends, one way or another.
    said: HashMap<String, (String, String)>,
}

/// What the view is told of one subagent or workflow.
#[derive(Debug, Default)]
struct TaskView {
    item: u64,
    workflow: bool,
    /// A subagent's own tool calls, as `Name(arg)`.
    calls: Vec<String>,
    usage: sdk::Usage,
    phases: Vec<String>,
    agents: Vec<sdk::WorkflowAgent>,
    /// How it ended, once it has.
    ended: Option<String>,
    /// The lines and state last sent, so a progress line that changes nothing sends nothing.
    sent: (Vec<String>, String),
}

impl TaskView {
    fn state(&self) -> String {
        self.ended.clone().unwrap_or_else(|| "running".into())
    }

    /// Claude Code's lines under an agent call: its last tool calls and how many more while
    /// it runs, `Done (3 tool uses · 21.6k tokens · 7.6s)` once it has finished. A
    /// workflow's say its phase, how many of that phase's agents are done, and what the
    /// running ones are doing.
    fn lines(&self) -> Vec<String> {
        let u = self.usage;
        if let Some(how) = &self.ended {
            let word = match how.as_str() {
                "completed" => "Done",
                "failed" => "Failed",
                _ => "Stopped",
            };
            let mut parts = Vec::new();
            if self.workflow && !self.agents.is_empty() {
                parts.push(plural(self.agents.len() as u64, "agent"));
            }
            parts.push(plural(u.tool_uses, "tool use"));
            if u.tokens > 0 {
                parts.push(format!("{} tokens", tokens(u.tokens)));
            }
            parts.push(duration(u.duration_ms));
            return vec![format!("{word} ({})", parts.join(" · "))];
        }
        if self.workflow {
            let Some(phase) = self.agents.iter().map(|a| a.phase).max() else {
                return vec![match self.phases.first() {
                    Some(p) => format!("{p} · starting…"),
                    None => "Starting…".into(),
                }];
            };
            let here: Vec<&sdk::WorkflowAgent> =
                self.agents.iter().filter(|a| a.phase == phase).collect();
            let done = here.iter().filter(|a| a.state == "done").count();
            let title = (phase as usize)
                .checked_sub(1)
                .and_then(|i| self.phases.get(i))
                .map(|t| format!("{t} · "))
                .unwrap_or_default();
            let mut out = vec![format!("{title}{done} of {} agents done", here.len())];
            let running: Vec<&&sdk::WorkflowAgent> = here
                .iter()
                .filter(|a| !matches!(a.state.as_str(), "done" | "failed" | "queued"))
                .collect();
            for a in running.iter().take(2) {
                out.push(if a.last_tool.is_empty() {
                    a.label.clone()
                } else {
                    format!("{} · {}", a.label, a.last_tool)
                });
            }
            if running.len() > 2 {
                out.push(format!("+{} more running", running.len() - 2));
            }
            return out;
        }
        if self.calls.is_empty() {
            return vec!["Initializing…".into()];
        }
        const SHOWN: usize = 2;
        let shown = self.calls.len().min(SHOWN);
        let mut out = self.calls[self.calls.len() - shown..].to_vec();
        let more = (u.tool_uses as usize).max(self.calls.len()) - shown;
        if more > 0 {
            out.push(format!(
                "+{more} more tool use{}",
                if more == 1 { "" } else { "s" }
            ));
        }
        out
    }
}

fn plural(n: u64, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// `512`, `21.6k`, `1.2m`, as Claude Code counts tokens.
fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}m", n as f64 / 1e6),
    }
}

/// `7.6s`, `1m 23s`, `1h 2m`.
fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{:.1}s", ms as f64 / 1e3),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s / 60 % 60),
    }
}

/// Whether a tool call starts a subagent or a workflow, which the view draws on its own.
fn is_task_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task" | "Workflow")
}

/// The name an agent call is drawn under: its type, as Claude Code titles it, or `Agent` for
/// the general-purpose one; `Workflow` for a workflow.
fn agent_name(name: &str, input: &Value) -> String {
    if name == "Workflow" {
        return name.into();
    }
    match input.get("subagent_type").and_then(Value::as_str) {
        Some(t) if !t.is_empty() && t != "general-purpose" => t.to_string(),
        _ => "Agent".into(),
    }
}

/// A workflow call's title: a saved workflow's name, or the description in its script's
/// `meta`, which is a literal by the tool's own rule.
fn workflow_title(input: &Value) -> String {
    if let Some(n) = input.get("name").and_then(Value::as_str) {
        return cut(n);
    }
    let script = input.get("script").and_then(Value::as_str).unwrap_or("");
    let found = script.find("description:").and_then(|at| {
        let rest = script[at + "description:".len()..].trim_start();
        let q = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"' | '`'))?;
        let body = &rest[1..];
        body.find(q).map(|end| body[..end].to_string())
    });
    cut(&found.unwrap_or_else(|| "workflow".into()))
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

    /// A subagent's or workflow's lines, sent when they differ from the last sent.
    fn show_task(&mut self, key: &str, out: &mut Out) {
        let Some(t) = self.tasks.get_mut(key) else {
            return;
        };
        let now = (t.lines(), t.state());
        if t.sent != now {
            t.sent = now.clone();
            out.ops.push(Op::Progress {
                id: t.item,
                lines: now.0,
                state: now.1,
            });
        }
    }

    /// The conversation's id, model, mode, title and effort, as last heard.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// The full text of a tool result the view was sent a preview of.
    pub fn full_result(&self, id: u64) -> Option<&str> {
        self.results.get(&id).map(String::as_str)
    }

    /// The input claude sent with a waiting request, which an answer carries back.
    pub fn waiting_input(&self, request_id: &str) -> Option<&Value> {
        self.waiting.get(request_id).map(|(_, input)| input)
    }

    /// The rules "Yes, and don't ask again" adds for a waiting permission prompt.
    pub fn waiting_suggestions(&self, request_id: &str) -> &[Value] {
        self.suggestions
            .get(request_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// A waiting prompt was answered by a view: it stops waiting, with `how` said. `None` when
    /// it was not waiting — answered already, by another view or by claude's withdrawal — so
    /// the relay sends claude nothing and tells that view `answered elsewhere`.
    pub fn answered(
        &mut self,
        request_id: &str,
        how: &str,
        answers: Vec<(String, String)>,
    ) -> Option<Out> {
        let (id, _) = self.waiting.remove(request_id)?;
        self.suggestions.remove(request_id);
        let mut out = Out::default();
        out.ops.push(Op::Resolved {
            id,
            how: how.to_string(),
            answers,
        });
        if self.waiting.is_empty() && self.state == "waiting" {
            self.set_state(&mut out, "working");
        }
        Some(out)
    }

    /// One line from claude.
    pub fn feed(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        // A subagent's own messages are drawn by Claude Code as progress under its agent call,
        // its tool calls as the last few of them, not as the conversation's.
        if let Some(parent) = sdk::parent_tool_use_id(v) {
            match sdk::classify(v) {
                // A subagent's permission prompt still needs the owner.
                Msg::Permission { .. } | Msg::Question { .. } => {}
                Msg::Assistant { blocks, .. } => {
                    let parent = parent.to_string();
                    if let Some(t) = self.tasks.get_mut(&parent) {
                        for b in blocks {
                            if let Block::ToolUse { name, input, .. } = b {
                                // How a subagent hands its answer back, not work it did:
                                // counted in its tool uses, not listed.
                                if matches!(name.as_str(), "SubagentHandback" | "StructuredOutput")
                                {
                                    continue;
                                }
                                let arg = tool_arg(&name, &input);
                                t.calls.push(if arg.is_empty() {
                                    name
                                } else {
                                    format!("{name}({arg})")
                                });
                            }
                        }
                    }
                    self.show_task(&parent, &mut out);
                    return out;
                }
                _ => return out,
            }
        }
        match sdk::classify(v) {
            Msg::Init {
                session_id,
                model,
                permission_mode,
                cwd,
                version,
                commands,
            } => {
                let meta = Meta {
                    session_id,
                    model,
                    permission_mode,
                    cwd,
                    version,
                    commands,
                    ..self.meta.clone()
                };
                self.set_meta(&mut out, meta);
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
                        Block::ToolUse { id, name, input } if is_task_tool(&name) => {
                            let arg = if name == "Workflow" {
                                workflow_title(&input)
                            } else {
                                tool_arg(&name, &input)
                            };
                            let shown = agent_name(&name, &input);
                            let item = self.add(&mut out, Item::Agent { name: shown, arg });
                            self.tools.insert(id.clone(), item);
                            self.tasks.insert(
                                id.clone(),
                                TaskView {
                                    item,
                                    workflow: name == "Workflow",
                                    ..TaskView::default()
                                },
                            );
                            self.show_task(&id, &mut out);
                        }
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
                    // An agent's result is its report, kept for the view to ask for; what is
                    // drawn is its progress and how it ended. A call that failed outright (a
                    // workflow whose script would not run) ends there.
                    if let Some(t) = self.tasks.get_mut(&r.tool_use_id) {
                        if r.is_error && t.ended.is_none() {
                            t.ended = Some("failed".into());
                        }
                        self.results.entry(id).or_insert(r.text);
                        self.show_task(&r.tool_use_id, &mut out);
                        continue;
                    }
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
                suggestions,
            } => {
                let arg = tool_arg(&tool, &input);
                let always = always_label(&suggestions).unwrap_or_default();
                let id = self.add(
                    &mut out,
                    Item::Permission {
                        request_id: request_id.clone(),
                        tool,
                        arg,
                        description: description.unwrap_or_default(),
                        always,
                    },
                );
                self.waiting.insert(request_id.clone(), (id, input));
                if !suggestions.is_empty() {
                    self.suggestions.insert(request_id, suggestions);
                }
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
            Msg::Answered { questions, answers } => {
                let id = self.add(
                    &mut out,
                    Item::Ask {
                        request_id: String::new(),
                        questions,
                    },
                );
                out.ops.push(Op::Resolved {
                    id,
                    how: "answered".into(),
                    answers,
                });
            }
            Msg::Task { tool_use_id, event } => {
                let Some(t) = self.tasks.get_mut(&tool_use_id) else {
                    // A background shell command's task, or one before this log began.
                    return out;
                };
                match event {
                    sdk::Task::Started { .. } => {}
                    sdk::Task::Progress {
                        usage,
                        phases,
                        agents,
                    } => {
                        t.usage = usage;
                        if !phases.is_empty() {
                            t.phases = phases;
                        }
                        if !agents.is_empty() {
                            t.agents = agents;
                        }
                    }
                    sdk::Task::Done {
                        status,
                        usage,
                        report,
                    } => {
                        t.ended = Some(if status.is_empty() {
                            "completed".into()
                        } else {
                            status
                        });
                        if usage != sdk::Usage::default() {
                            t.usage = usage;
                        }
                        if let Some(r) = report {
                            self.results.insert(t.item, r);
                        }
                    }
                }
                self.show_task(&tool_use_id, &mut out);
            }
            Msg::Cancelled { request_id } => {
                if let Some(o) = self.answered(&request_id, "withdrawn", Vec::new()) {
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
                    if let Some(o) = self.answered(&r, "withdrawn", Vec::new()) {
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
            // What a view sent, read back from the holder's log: an answer resolves its prompt
            // for every view at once, and a change of model, mode or effort shows at once
            // rather than at the next turn's init.
            Msg::Input(input) => match input {
                sdk::Input::Answer {
                    request_id,
                    how,
                    answers,
                } => {
                    if let Some(o) = self.answered(&request_id, &how, answers) {
                        out.ops.extend(o.ops);
                    }
                }
                sdk::Input::Model(model) => {
                    let meta = Meta {
                        model,
                        ..self.meta.clone()
                    };
                    self.set_meta(&mut out, meta);
                }
                sdk::Input::Mode(permission_mode) => {
                    let meta = Meta {
                        permission_mode,
                        ..self.meta.clone()
                    };
                    self.set_meta(&mut out, meta);
                }
                sdk::Input::Effort(effort) => {
                    let meta = Meta {
                        effort: effort.unwrap_or_default(),
                        ..self.meta.clone()
                    };
                    self.set_meta(&mut out, meta);
                }
                sdk::Input::Say { uuid, text } => {
                    self.said.insert(uuid, (text, "sent".into()));
                }
                sdk::Input::Other => {}
            },
            Msg::Lifecycle { uuid, state } => match state.as_str() {
                "queued" | "started" => {
                    if let Some(said) = self.said.get_mut(&uuid) {
                        said.1 = state;
                    }
                }
                "cancelled" => {
                    if let Some((text, _)) = self.said.remove(&uuid) {
                        // A prompt behind it is what the interrupt makes way for (Claude Code
                        // keeps the interrupted one, with its note, when one waits).
                        let queued = self.said.values().any(|(_, st)| st == "queued");
                        out.ops.push(Op::Back { text, uuid, queued });
                    }
                }
                _ => {
                    self.said.remove(&uuid);
                }
            },
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

/// The label Claude Code puts on "Yes, and don't ask again", from what claude suggested
/// adding, read from 2.1.311's permission dialog: a Bash rule's command prefixes, said as
/// `touch commands in this project`; another tool's rules by name; directories as access.
/// `None` when nothing it suggested is a rule or a directory, and the option is not offered.
pub fn always_label(suggestions: &[Value]) -> Option<String> {
    let mut commands: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for s in suggestions {
        match s.get("type").and_then(Value::as_str) {
            Some("addRules") => {
                for r in s.get("rules").and_then(Value::as_arr).unwrap_or(&[]) {
                    let tool = r.get("toolName").and_then(Value::as_str).unwrap_or("");
                    match (tool, r.get("ruleContent").and_then(Value::as_str)) {
                        ("Bash", Some(rule)) => {
                            let prefix = rule
                                .trim_end_matches(":*")
                                .trim_end_matches(" *")
                                .trim_end_matches('*')
                                .trim();
                            let word = prefix.split_whitespace().next().unwrap_or(prefix);
                            if !word.is_empty() && !commands.iter().any(|c| c == word) {
                                commands.push(word.to_string());
                            }
                        }
                        ("", _) => {}
                        (tool, _) => {
                            if !tools.iter().any(|t| t == tool) {
                                tools.push(tool.to_string());
                            }
                        }
                    }
                }
            }
            Some("addDirectories") => {
                for d in s.get("directories").and_then(Value::as_arr).unwrap_or(&[]) {
                    if let Some(d) = d.as_str() {
                        dirs.push(d.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let and = |v: &[String]| v.join(" and ");
    let label = if !commands.is_empty() {
        format!(
            "Yes, and don't ask again for {} commands in this project",
            and(&commands)
        )
    } else if !tools.is_empty() {
        format!("Yes, and don't ask again for {}", and(&tools))
    } else if !dirs.is_empty() {
        format!(
            "Yes, and always allow access to {} from this project",
            and(&dirs)
        )
    } else {
        return None;
    };
    Some(label)
}

/// Merge a batch of ops for one write: consecutive appends to one item become one, and only
/// the last progress op of each agent is kept.
pub fn coalesce(ops: Vec<Op>) -> Vec<Op> {
    let mut merged: Vec<Op> = Vec::with_capacity(ops.len());
    for op in ops {
        // A progress op replaces what was under its agent, so an earlier one in the batch is
        // already out of date.
        if let Op::Progress { id, .. } = &op {
            merged.retain(|m| !matches!(m, Op::Progress { id: was, .. } if was == id));
        }
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
            Op::Resolved {
                id: n,
                how,
                answers,
            } => {
                let mut v = obj(vec![
                    ("t", Value::string("done")),
                    id(n),
                    ("x", Value::string(how.clone())),
                ]);
                if !answers.is_empty() {
                    let mut a = Value::obj();
                    for (q, x) in answers {
                        a.set(q, Value::string(x.clone()));
                    }
                    v.set("a", a);
                }
                v
            }
            Op::Progress {
                id: n,
                lines,
                state,
            } => obj(vec![
                ("t", Value::string("prog")),
                id(n),
                (
                    "p",
                    Value::Arr(lines.iter().cloned().map(Value::string).collect()),
                ),
                ("st", Value::string(state.clone())),
            ]),
            Op::State(s) => obj(vec![
                ("t", Value::string("st")),
                ("s", Value::string(s.clone())),
            ]),
            Op::Back { text, uuid, queued } => obj(vec![
                ("t", Value::string("back")),
                ("x", Value::string(text.clone())),
                ("u", Value::string(uuid.clone())),
                ("q", Value::Bool(*queued)),
            ]),
            Op::Meta(m) => {
                let mut v = obj(vec![
                    ("t", Value::string("meta")),
                    ("sid", Value::string(m.session_id.clone())),
                    ("m", Value::string(m.model.clone())),
                    ("pm", Value::string(m.permission_mode.clone())),
                    ("ti", Value::string(m.title.clone())),
                    ("e", Value::string(m.effort.clone())),
                    ("cwd", Value::string(m.cwd.clone())),
                    ("ver", Value::string(m.version.clone())),
                ]);
                // Only when there are some: an older client reads the op without it.
                if !m.commands.is_empty() {
                    v.set(
                        "sc",
                        Value::Arr(m.commands.iter().map(Value::string).collect()),
                    );
                }
                v
            }
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
            Item::Agent { name, arg } => obj(vec![
                k("agent"),
                ("n", Value::string(name.clone())),
                ("a", Value::string(arg.clone())),
            ]),
            Item::Permission {
                request_id,
                tool,
                arg,
                description,
                always,
            } => {
                let mut v = obj(vec![
                    k("perm"),
                    ("r", Value::string(request_id.clone())),
                    ("n", Value::string(tool.clone())),
                    ("a", Value::string(arg.clone())),
                    ("d", Value::string(description.clone())),
                ]);
                if !always.is_empty() {
                    v.set("al", Value::string(always.clone()));
                }
                v
            }
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
                                                .zip(
                                                    q.previews
                                                        .iter()
                                                        .map(String::as_str)
                                                        .chain(std::iter::repeat("")),
                                                )
                                                .map(|((l, d), p)| {
                                                    let mut o = vec![
                                                        Value::string(l.clone()),
                                                        Value::string(d.clone()),
                                                    ];
                                                    // Only an option with a preview has a third.
                                                    if !p.is_empty() {
                                                        o.push(Value::string(p));
                                                    }
                                                    Value::Arr(o)
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
                r#"{"cwd":"","e":"","m":"claude-haiku-5-5","pm":"default","sid":"s1","t":"meta","ti":"","ver":""}"#,
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

        let answered = r
            .answered("req-1", "allowed", vec![])
            .expect("it was waiting");
        assert_eq!(
            wire(&answered.ops),
            [
                r#"{"id":3,"t":"done","x":"allowed"}"#,
                r#"{"s":"working","t":"st"}"#
            ]
        );
        assert!(
            r.answered("req-1", "allowed", vec![]).is_none(),
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
    fn a_questions_answers_reach_the_view_live_and_from_a_transcript() {
        let mut r = Reducer::new();
        feed_all(
            &mut r,
            &[
                r#"{"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Red or blue?","header":"Color","options":[{"label":"Red","description":"r"}],"multiSelect":false}]}}}"#,
            ],
        );
        // The view's answer, read back from the holder's log.
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"_in","line":{"type":"control_response","response":{"subtype":"success","request_id":"q1","response":{"behavior":"allow","updatedInput":{"questions":[],"answers":{"Red or blue?":"Red"}}}}}}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops)[0],
            r#"{"a":{"Red or blue?":"Red"},"id":1,"t":"done","x":"answered"}"#
        );
        // A resume: no request comes, the transcript's tool result says what was chosen.
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"_history","line":{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"User has answered"}]},"toolUseResult":{"questions":[{"question":"Red or blue?","header":"Color","options":[{"label":"Red","description":"r"}],"multiSelect":false}],"answers":{"Red or blue?":"Red"}}}}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"k":"ask","q":[{"h":"Color","m":false,"o":[["Red","r"]],"q":"Red or blue?"}],"r":"","t":"add"}"#,
                r#"{"a":{"Red or blue?":"Red"},"id":1,"t":"done","x":"answered"}"#,
            ]
        );
    }

    #[test]
    fn an_interrupt_that_catches_a_prompt_hands_it_back_saying_whether_another_waited() {
        let life = |uuid: &str, state: &str| {
            format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"{state}"}}"#)
        };
        let line =
            |v: Value| parse(&format!(r#"{{"type":"_in","line":{}}}"#, to_line(&v))).unwrap();
        let mut r = Reducer::new();
        r.feed(&line(sdk::prompt("first", "u1")));
        for st in ["queued", "started"] {
            assert!(r.feed(&parse(&life("u1", st)).unwrap()).ops.is_empty());
        }
        // A second sent while the first runs.
        r.feed(&line(sdk::prompt("second", "u2")));
        r.feed(&parse(&life("u2", "queued")).unwrap());
        let o = r.feed(&parse(&life("u1", "cancelled")).unwrap());
        assert_eq!(
            wire(&o.ops),
            [r#"{"q":true,"t":"back","u":"u1","x":"first"}"#]
        );
        // The second, caught alone, waits on nothing.
        r.feed(&parse(&life("u2", "started")).unwrap());
        let o = r.feed(&parse(&life("u2", "cancelled")).unwrap());
        assert_eq!(
            wire(&o.ops),
            [r#"{"q":false,"t":"back","u":"u2","x":"second"}"#]
        );
        // Once answered, a prompt is forgotten; one no view sent is not said back.
        r.feed(&line(sdk::prompt("third", "u3")));
        r.feed(&parse(&life("u3", "completed")).unwrap());
        assert!(
            r.feed(&parse(&life("u3", "cancelled")).unwrap())
                .ops
                .is_empty()
        );
        assert!(
            r.feed(&parse(&life("u9", "cancelled")).unwrap())
                .ops
                .is_empty()
        );
    }

    #[test]
    fn a_prompt_claude_takes_in_between_tool_calls_is_drawn_between_them() {
        // The order 2.1.311 sent on 2026-11-02: a prompt queued during the first call is
        // echoed after its result, and the same turn goes on to the next call.
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"user","message":{"role":"user","content":"sleep three times"},"isReplay":true}"#,
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 4"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":""}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":"Also say BANANA"},"isReplay":true}"#,
                r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"sleep 4"}}]}}"#,
            ],
        );
        let kinds: Vec<String> = wire(&o.ops)
            .iter()
            .filter(|l| l.contains(r#""t":"add""#) || l.contains(r#""t":"res""#))
            .map(|l| {
                let v = parse(l).unwrap();
                format!(
                    "{}:{}",
                    v.get("t").and_then(Value::as_str).unwrap(),
                    v.get("k").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        assert_eq!(
            kinds,
            ["add:user", "add:tool", "res:", "add:user", "add:tool"]
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
        assert!(r.answered("q1", "answered", vec![]).is_none());
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
            r#"{"a":"Find it","id":1,"k":"agent","n":"Agent","t":"add"}"#
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

    // A foreground Explore agent as 2.1.311 wrote it on 2026-11-01, cut to the fields read:
    // its call, the task's start and progress, its own tool calls under the call's id, its
    // handback, the notification, then the call's result.
    const AGENT: &[&str] = &[
        r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_a","name":"Agent","input":{"description":"Count lines in .txt files","subagent_type":"Explore","prompt":"long"}}]},"parent_tool_use_id":null}"#,
        r#"{"type":"system","subtype":"task_started","task_id":"t1","tool_use_id":"toolu_a","description":"Count lines in .txt files","subagent_type":"Explore","task_type":"local_agent"}"#,
        r#"{"type":"system","subtype":"task_progress","task_id":"t1","tool_use_id":"toolu_a","usage":{"total_tokens":20012,"tool_uses":1,"duration_ms":2354},"last_tool_name":"Glob"}"#,
        r#"{"type":"assistant","message":{"id":"i1","content":[{"type":"tool_use","id":"toolu_g","name":"Glob","input":{"pattern":"*.txt"}}]},"parent_tool_use_id":"toolu_a"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_g","type":"tool_result","content":"a.txt\nb.txt"}]},"parent_tool_use_id":"toolu_a"}"#,
        r#"{"type":"assistant","message":{"id":"i2","content":[{"type":"tool_use","id":"toolu_b","name":"Bash","input":{"command":"wc -l a.txt b.txt"}}]},"parent_tool_use_id":"toolu_a"}"#,
        r#"{"type":"assistant","message":{"id":"i3","content":[{"type":"tool_use","id":"toolu_r","name":"Read","input":{"file_path":"a.txt"}}]},"parent_tool_use_id":"toolu_a"}"#,
        r#"{"type":"assistant","message":{"id":"i4","content":[{"type":"tool_use","id":"toolu_h","name":"SubagentHandback","input":{"message":"a.txt: 2, b.txt: 1"}}]},"parent_tool_use_id":"toolu_a"}"#,
        r#"{"type":"system","subtype":"task_notification","task_id":"t1","tool_use_id":"toolu_a","status":"completed","usage":{"total_tokens":21565,"tool_uses":4,"duration_ms":7594},"handback_report":{"text":"a.txt: 2, b.txt: 1"}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_a","type":"tool_result","content":[{"type":"text","text":"This agent's report was delivered to you as a message"}]}]},"parent_tool_use_id":null}"#,
    ];

    fn progress(ops: &[Op]) -> Vec<(Vec<String>, String)> {
        ops.iter()
            .filter_map(|o| match o {
                Op::Progress { lines, state, .. } => Some((lines.clone(), state.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_agent_is_drawn_under_its_type_with_its_last_calls_then_how_it_ended() {
        let mut r = Reducer::new();
        let o = feed_all(&mut r, AGENT);
        let lines = wire(&o.ops);
        assert_eq!(
            lines[0],
            r#"{"a":"Count lines in .txt files","id":1,"k":"agent","n":"Explore","t":"add"}"#
        );
        let p = progress(&o.ops);
        let got: Vec<Vec<&str>> = p
            .iter()
            .map(|(l, _)| l.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            got,
            [
                vec!["Initializing…"],
                vec!["Glob(*.txt)"],
                vec!["Glob(*.txt)", "Bash(wc -l a.txt b.txt)"],
                vec!["Bash(wc -l a.txt b.txt)", "Read(a.txt)", "+1 more tool use"],
                vec!["Done (4 tool uses · 21.6k tokens · 7.6s)"],
            ],
            "the last two calls and a count of the rest; the handback is not one of them"
        );
        assert!(p[..4].iter().all(|(_, st)| st == "running"));
        assert_eq!(p[4].1, "completed");
        assert!(
            !lines.iter().any(|l| l.contains(r#""t":"res""#)),
            "its result is its report, not drawn: {lines:?}"
        );
        assert_eq!(r.full_result(1), Some("a.txt: 2, b.txt: 1"));
    }

    #[test]
    fn a_workflow_says_its_phase_its_running_agents_and_its_totals() {
        let mut r = Reducer::new();
        let agent = |label: &str, state: &str, tool: &str| {
            format!(
                r#"{{"type":"workflow_agent","label":"{label}","phaseIndex":1,"state":"{state}","lastToolName":"{tool}"}}"#
            )
        };
        let prog = |agents: &[String]| {
            format!(
                r#"{{"type":"system","subtype":"task_progress","tool_use_id":"toolu_w","usage":{{"total_tokens":0,"tool_uses":0,"duration_ms":40}},"workflow_progress":[{{"type":"workflow_phase","index":1,"title":"Read"}},{}]}}"#,
                agents.join(",")
            )
        };
        let start = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_w","name":"Workflow","input":{"script":"export const meta = {\n  name: 'r',\n  description: 'Read both files',\n}\n"}}]}}"#;
        let a = prog(&[
            agent("read:a", "running", "Read"),
            agent("read:b", "queued", ""),
        ]);
        let b = prog(&[
            agent("read:a", "done", "Read"),
            agent("read:b", "running", ""),
        ]);
        let launched = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_w","type":"tool_result","content":"Workflow launched in background."}]}}"#;
        let done = r#"{"type":"system","subtype":"task_notification","tool_use_id":"toolu_w","status":"completed","usage":{"total_tokens":43599,"tool_uses":4,"duration_ms":67575}}"#;
        let o = feed_all(&mut r, &[start, &a, launched, &b, done]);
        assert_eq!(
            wire(&o.ops)[0],
            r#"{"a":"Read both files","id":1,"k":"agent","n":"Workflow","t":"add"}"#
        );
        let p = progress(&o.ops);
        let got: Vec<Vec<&str>> = p
            .iter()
            .map(|(l, _)| l.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            got,
            [
                vec!["Starting…"],
                vec!["Read · 0 of 2 agents done", "read:a · Read"],
                vec!["Read · 1 of 2 agents done", "read:b"],
                vec!["Done (2 agents · 4 tool uses · 43.6k tokens · 1m 7s)"],
            ]
        );
    }

    #[test]
    fn an_agent_call_that_fails_outright_says_so() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_w","name":"Workflow","input":{"name":"audit"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_w","type":"tool_result","content":"script did not parse","is_error":true}]}}"#,
            ],
        );
        let p = progress(&o.ops);
        assert_eq!(p.last().map(|(_, st)| st.as_str()), Some("failed"));
        assert!(p.last().unwrap().0[0].starts_with("Failed ("));
        assert_eq!(r.full_result(1), Some("script did not parse"));
    }

    #[test]
    fn only_the_last_progress_of_each_agent_in_a_batch_is_sent() {
        let p = |id, l: &str| Op::Progress {
            id,
            lines: vec![l.into()],
            state: "running".into(),
        };
        let merged = coalesce(vec![
            p(1, "a"),
            p(2, "x"),
            p(1, "b"),
            Op::State("idle".into()),
        ]);
        assert_eq!(merged, vec![p(2, "x"), p(1, "b"), Op::State("idle".into())]);
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

    #[test]
    fn an_answer_any_view_sent_resolves_the_prompt_for_every_reader_of_the_log() {
        let allow = to_line(&sdk::allow("req-1", Value::obj()));
        let logged = format!(r#"{{"type":"_in","line":{allow}}}"#);
        // Two relays reading the same log: both see the prompt answered, neither owes claude.
        for _ in 0..2 {
            let mut r = Reducer::new();
            feed_all(&mut r, TURN);
            let out = r.feed(&parse(&logged).unwrap());
            assert_eq!(
                wire(&out.ops),
                [
                    r#"{"id":3,"t":"done","x":"allowed"}"#,
                    r#"{"s":"working","t":"st"}"#
                ]
            );
            assert!(out.to_claude.is_empty());
            assert!(r.waiting_input("req-1").is_none());
            // The same answer again, as a log could hold had the holder not dropped it: nothing.
            assert!(r.feed(&parse(&logged).unwrap()).ops.is_empty());
        }
    }

    #[test]
    fn a_change_a_view_sent_shows_at_once_and_effort_survives_the_next_init() {
        let mut r = Reducer::new();
        feed_all(&mut r, TURN);
        let line =
            |v: Value| parse(&format!(r#"{{"type":"_in","line":{}}}"#, to_line(&v))).unwrap();
        let out = r.feed(&line(sdk::set_model("c1", "claude-opus-5-5")));
        assert_eq!(
            wire(&out.ops),
            [
                r#"{"cwd":"","e":"","m":"claude-opus-5-5","pm":"default","sid":"s1","t":"meta","ti":"","ver":""}"#
            ]
        );
        r.feed(&line(sdk::set_effort("c2", Some("high"))));
        r.feed(&line(sdk::set_permission_mode("c3", "plan")));
        let init = r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-5-5","permissionMode":"plan"}"#;
        assert!(
            feed_all(&mut r, &[init]).ops.is_empty(),
            "the next init agrees, and keeps the effort it does not carry"
        );
        assert!(
            r.feed(&line(sdk::user_text("hi"))).ops.is_empty(),
            "claude echoes a prompt itself"
        );
    }

    #[test]
    fn dont_ask_again_is_labelled_as_claude_code_labels_it_and_offered_only_with_a_rule() {
        let v = |s: &str| parse(s).unwrap();
        // As 2.1.311 suggested for `touch spike-file && ls` (spike, 2026-11-01).
        let touch = v(
            r#"{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch spike-file *"}],"behavior":"allow","destination":"localSettings"}"#,
        );
        let dirs = v(r#"{"type":"addDirectories","directories":["/w"],"destination":"session"}"#);
        assert_eq!(
            always_label(&[touch.clone(), dirs.clone()]).as_deref(),
            Some("Yes, and don't ask again for touch commands in this project")
        );
        let two = v(
            r#"{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cargo test:*"},{"toolName":"Bash","ruleContent":"git status"}]}"#,
        );
        assert_eq!(
            always_label(&[two]).as_deref(),
            Some("Yes, and don't ask again for cargo and git commands in this project")
        );
        let edit = v(r#"{"type":"addRules","rules":[{"toolName":"Edit"}]}"#);
        assert_eq!(
            always_label(&[edit]).as_deref(),
            Some("Yes, and don't ask again for Edit")
        );
        assert_eq!(
            always_label(&[dirs]).as_deref(),
            Some("Yes, and always allow access to /w from this project")
        );
        assert_eq!(always_label(&[]), None);
        assert_eq!(
            always_label(&[v(r#"{"type":"setMode","mode":"acceptEdits"}"#)]),
            None
        );

        // Carried on the item, and kept for the answer.
        let mut r = Reducer::new();
        let req = format!(
            r#"{{"type":"control_request","request_id":"p1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"touch x"}},"permission_suggestions":[{}]}}}}"#,
            to_line(&touch)
        );
        let out = r.feed(&v(&req));
        assert!(
            wire(&out.ops)[0]
                .contains(r#""al":"Yes, and don't ask again for touch commands in this project""#),
            "{:?}",
            wire(&out.ops)
        );
        assert_eq!(r.waiting_suggestions("p1"), [touch]);
        r.answered("p1", "allowed", vec![]);
        assert!(r.waiting_suggestions("p1").is_empty());
    }
}
