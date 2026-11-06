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
use crate::sdk::{self, Block, Msg, Question, Window};

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
    /// Claude's background tasks — shells, monitors, subagents, workflows — as they stand:
    /// the whole list, sent when any of it changes, for Claude Code's tasks view.
    Tasks(Vec<BgTask>),
    /// An interrupt caught a prompt a view sent (`command_lifecycle` `cancelled`): its text,
    /// and whether another prompt was waiting behind it. The view decides, as Claude Code
    /// does, whether that hands the prompt back to its input (`convo.rs`).
    Back {
        text: String,
        uuid: String,
        queued: bool,
    },
    /// claude's summary of one step's tool calls, on each call item it covers: `group` is
    /// the first of them, so the view draws the step as one line of `text`.
    Summary { id: u64, text: String, group: u64 },
}

/// One of claude's background tasks, as the tasks view is told of it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BgTask {
    /// claude's `task_id`, which `get_task_output` and `stop_task` take.
    pub id: String,
    /// `shell`, `monitor`, `agent`, `workflow`, `remote`, or claude's own `task_type`.
    pub kind: String,
    pub description: String,
    /// A shell's or monitor's command; a subagent's type (`Explore`); a workflow's name.
    pub detail: String,
    /// A subagent's prompt, its first `PROMPT_MAX` characters: the details view shows a
    /// few lines of it.
    pub prompt: String,
    /// When it started and ended, epoch milliseconds, where claude said.
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    /// `running`, `completed`, `failed`, `killed` or `stopped`.
    pub status: String,
    /// The transcript item a subagent or workflow is drawn as, whose progress lines say
    /// what it is doing.
    pub item: Option<u64>,
    /// A subagent's model, tokens and tool calls so far, and what it did last
    /// (`Read(/a/b)`); a workflow's tokens, its agents done and in all.
    pub model: String,
    pub tokens: u64,
    pub tools: u64,
    pub activity: String,
    pub agents_done: u64,
    pub agents: u64,
    /// A workflow ran from a script, which `s save` can keep.
    pub saveable: bool,
}

/// How many ended tasks the list keeps, the latest: Claude Code's dialog shows ended ones
/// until they are cleared; a long session must not grow the list without end.
pub const ENDED_KEPT: usize = 10;

/// How much of a subagent's prompt the list carries.
pub const PROMPT_MAX: usize = 400;

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
    /// The account's 5-hour and weekly usage, as claude last read each.
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// What the owner typed, as claude echoed it.
    Prompt(String),
    /// The assistant's text; may grow by `Append`.
    Text(String),
    /// The server's summary of the prose between tool calls, sent as a narration thinking
    /// block: drawn as text is, with Claude Code's grey `· summary` after it.
    Narration(String),
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
    /// A background task's notice, which claude hands the model as a prompt: drawn as a note,
    /// and counted by Claude Code's tasks view while it is open.
    Notice(String),
    /// A message another agent sent: a subagent's hand-back, another session's, a
    /// teammate's. Drawn as Claude Code draws one, not as a prompt.
    Message(sdk::Message),
    /// The conversation was compacted, from and to so many tokens.
    Compacted { pre: Option<u64>, post: Option<u64> },
}

/// What one line of claude's output came to.
#[derive(Debug, Default, PartialEq)]
pub struct Out {
    pub ops: Vec<Op>,
    /// Lines claude is owed at once: the refusal of a request the view does not serve.
    pub to_claude: Vec<Value>,
    /// A subagent's own conversation, reduced as the main one is: by the tool-use id of the
    /// call that started it, for Claude Code's view of one agent (`relay.rs`).
    pub sub: Vec<(String, Op)>,
}

/// The state that turns a stream of lines into items.
#[derive(Debug, Default)]
pub struct Reducer {
    next_id: u64,
    /// A slash command's reply, written by claude itself: claude sends it before the command's
    /// echo, so it waits to be drawn under the command.
    local: Vec<String>,
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
    /// Their uuids in the order they were sent.
    said_order: Vec<String>,
    /// Tool calls of the turn under way with no result yet: what Ctrl+X Ctrl+S puts in the
    /// background, as Claude Code's does, rather than stopping the turn (`relay.rs`).
    running: std::collections::HashSet<u64>,
    /// Which tool call items are shell commands, which a prompt sent meanwhile backgrounds.
    shells: std::collections::HashSet<u64>,
    /// Background tasks, in the order they started, and the list as last sent.
    bg: Vec<Bg>,
    bg_sent: Vec<BgTask>,
    /// What started each background shell, monitor or agent: tool name and command or
    /// type, by tool-use id, until its task is heard of or its result comes.
    calls: HashMap<String, (String, String)>,
}

/// A background task, and the tool call that started it.
#[derive(Debug, Clone)]
struct Bg {
    task: BgTask,
    tool_use_id: String,
}

/// claude's `task_type` as the view names it; a monitor is a `local_bash` whose call was
/// `Monitor`, which only the call says.
fn task_kind(task_type: &str) -> &str {
    match task_type {
        "local_bash" => "shell",
        "local_agent" => "agent",
        "local_workflow" => "workflow",
        "remote_agent" => "remote",
        other => other,
    }
}

/// What the view is told of one subagent or workflow.
#[derive(Debug, Default)]
struct TaskView {
    item: u64,
    workflow: bool,
    /// The model a subagent's messages name.
    model: String,
    /// A workflow's script, for Claude Code's `s save`; none for a saved one run by name.
    script: Option<String>,
    /// A subagent's own conversation, as its own reducer has it.
    convo: Box<Reducer>,
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

    /// A reducer whose items are numbered from `first`: for a stretch of history read on its
    /// own, whose numbers must not meet the conversation's.
    pub fn numbered_from(first: u64) -> Reducer {
        Reducer {
            next_id: first.saturating_sub(1),
            ..Reducer::default()
        }
    }

    /// The number the next item would take.
    pub fn next_number(&self) -> u64 {
        self.next_id + 1
    }

    /// Prompts a view sent that claude has not echoed yet, oldest first: their uuid, text, and
    /// whether claude has them queued behind a turn. A view that connects after sending one
    /// draws it as on its way, as the view that sent it did.
    pub fn waiting_prompts(&self) -> Vec<(String, String, bool)> {
        self.said_order
            .iter()
            .filter_map(|u| {
                let (text, st) = self.said.get(u)?;
                matches!(st.as_str(), "sent" | "queued")
                    .then(|| (u.clone(), text.clone(), st == "queued"))
            })
            .collect()
    }

    /// Every tool call's whole result, by item, taken out of the reducer.
    pub fn take_results(&mut self) -> HashMap<u64, String> {
        std::mem::take(&mut self.results)
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

    /// A subagent's line, without the id that marks it as one, to its own reducer: what
    /// that makes of it goes out under the agent's call.
    fn feed_sub(&mut self, parent: &str, v: &Value, out: &mut Out) {
        let Some(t) = self.tasks.get_mut(parent) else {
            return;
        };
        if let Some(m) = v
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
        {
            t.model = m.to_string();
        }
        let mut line = v.clone();
        if let Value::Obj(m) = &mut line {
            m.remove("parent_tool_use_id");
        }
        let o = t.convo.feed(&line);
        // Its whole results are not kept: the agent's view shows their first lines.
        t.convo.results.clear();
        out.sub
            .extend(o.ops.into_iter().map(|op| (parent.to_string(), op)));
    }

    /// The tool-use id of the call that started task `task_id`: what its own conversation's
    /// ops are sent under.
    pub fn tool_of_task(&self, task_id: &str) -> Option<&str> {
        self.bg
            .iter()
            .find(|b| b.task.id == task_id && !b.tool_use_id.is_empty())
            .map(|b| b.tool_use_id.as_str())
    }

    /// A workflow's script, by its task id, for `s save`.
    pub fn workflow_script(&self, task_id: &str) -> Option<&str> {
        let tool = self.tool_of_task(task_id)?;
        self.tasks.get(tool)?.script.as_deref()
    }

    /// A workflow's phases and agents as Claude Code's browser shows them, by its task id:
    /// `{ph:[titles], ag:[{l, ph, m, st, tk, tc, t0, ms, tl}]}`, and the prompt and result
    /// previews (`pp`, `rp`) of agent `agent` only, since they are long and many.
    pub fn workflow_view(&self, task_id: &str, agent: Option<usize>) -> Option<Value> {
        let tool = self.tool_of_task(task_id)?;
        let t = self.tasks.get(tool).filter(|t| t.workflow)?;
        let mut v = Value::obj();
        v.set(
            "ph",
            Value::Arr(t.phases.iter().map(Value::string).collect()),
        );
        let ags = t
            .agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let mut o = obj(vec![
                    ("l", Value::string(a.label.clone())),
                    ("ph", Value::num(a.phase as f64)),
                    ("st", Value::string(a.state.clone())),
                ]);
                let put = |o: &mut Value, k: &str, s: &str| {
                    if !s.is_empty() {
                        o.set(k, Value::string(s));
                    }
                };
                put(&mut o, "m", &a.model);
                let tool = if a.last_summary.is_empty() {
                    a.last_tool.clone()
                } else {
                    format!("{}({})", a.last_tool, a.last_summary)
                };
                put(&mut o, "tl", &cut(&tool));
                put(&mut o, "er", &cut(&a.error));
                for (k, n) in [
                    ("tk", Some(a.tokens)),
                    ("tc", Some(a.tool_calls)),
                    ("t0", a.started_ms),
                    ("ms", a.duration_ms),
                ] {
                    if let Some(n) = n.filter(|n| *n > 0) {
                        o.set(k, Value::num(n as f64));
                    }
                }
                if agent == Some(i) {
                    put(&mut o, "pp", &a.prompt);
                    put(&mut o, "rp", &a.result);
                }
                o
            })
            .collect();
        v.set("ag", Value::Arr(ags));
        Some(v)
    }

    /// The background task list, sent when it differs from what was sent last.
    fn show_bg(&mut self, out: &mut Out) {
        // What the agents and workflows have done, from their calls' own records.
        for b in self.bg.iter_mut() {
            let Some(t) = self.tasks.get(&b.tool_use_id) else {
                continue;
            };
            let k = &mut b.task;
            k.model = t.model.clone();
            k.tokens = t.usage.tokens;
            k.tools = t.usage.tool_uses.max(t.calls.len() as u64);
            k.activity = t.calls.last().cloned().unwrap_or_default();
            k.saveable = t.workflow && t.script.is_some();
            if t.workflow {
                k.agents = t.agents.len() as u64;
                k.agents_done = t.agents.iter().filter(|a| a.state == "done").count() as u64;
                let sum: u64 = t.agents.iter().map(|a| a.tokens).sum();
                k.tokens = k.tokens.max(sum);
                if k.model.is_empty() {
                    k.model = t
                        .agents
                        .first()
                        .map(|a| a.model.clone())
                        .unwrap_or_default();
                }
            }
        }
        let now: Vec<BgTask> = self.bg.iter().map(|b| b.task.clone()).collect();
        if now != self.bg_sent {
            self.bg_sent = now.clone();
            out.ops.push(Op::Tasks(now));
        }
    }

    /// A task's start, change or end, by claude's task id.
    fn bg_event(&mut self, task_id: &str, tool_use_id: &str, event: &sdk::Task) {
        if task_id.is_empty() {
            return;
        }
        match event {
            sdk::Task::Started {
                task_type,
                description,
                started_ms,
                prompt,
                workflow_name,
            } => {
                let call = self.calls.remove(tool_use_id);
                let item = self.tasks.get(tool_use_id).map(|t| t.item);
                let b = self.bg_entry(task_id, task_type, description, *started_ms);
                if !tool_use_id.is_empty() {
                    b.tool_use_id = tool_use_id.to_string();
                }
                if let Some((name, detail)) = call {
                    if name == "Monitor" {
                        b.task.kind = "monitor".into();
                    }
                    b.task.detail = detail;
                }
                b.task.item = item.or(b.task.item);
                if !prompt.is_empty() {
                    b.task.prompt = prompt.chars().take(PROMPT_MAX).collect();
                }
                // A workflow's name is its detail, as a shell's command is.
                if !workflow_name.is_empty() {
                    b.task.detail = cut(workflow_name);
                }
            }
            sdk::Task::Updated { status, ended_ms } => {
                if let Some(b) = self.bg.iter_mut().find(|b| b.task.id == task_id) {
                    if !status.is_empty() {
                        b.task.status = status.clone();
                    }
                    b.task.ended_ms = ended_ms.or(b.task.ended_ms);
                }
            }
            // The notification's word wins: `stopped` where the update said `killed`.
            sdk::Task::Done { status, .. } => {
                if let Some(b) = self.bg.iter_mut().find(|b| b.task.id == task_id) {
                    b.task.status = if status.is_empty() {
                        "completed".into()
                    } else {
                        status.clone()
                    };
                }
            }
            sdk::Task::Progress { .. } => {}
        }
        self.trim_bg();
    }

    /// The entry for `task_id`, made if there is none yet.
    fn bg_entry(
        &mut self,
        task_id: &str,
        task_type: &str,
        description: &str,
        started_ms: Option<u64>,
    ) -> &mut Bg {
        let at = match self.bg.iter().position(|b| b.task.id == task_id) {
            Some(at) => at,
            None => {
                self.bg.push(Bg {
                    task: BgTask {
                        id: task_id.to_string(),
                        kind: task_kind(task_type).to_string(),
                        status: "running".into(),
                        ..BgTask::default()
                    },
                    tool_use_id: String::new(),
                });
                self.bg.len() - 1
            }
        };
        let b = &mut self.bg[at];
        if !description.is_empty() {
            b.task.description = cut(description);
        }
        b.task.started_ms = b.task.started_ms.or(started_ms);
        b
    }

    /// claude's list of what runs in the background now: anything in it is running, and
    /// anything not in it that was has ended, whatever line saying so went missing.
    fn tasks_now(&mut self, now: Vec<sdk::TaskNow>) {
        for t in &now {
            let b = self.bg_entry(&t.task_id, &t.task_type, &t.description, t.started_ms);
            b.task.status = "running".into();
        }
        for b in self.bg.iter_mut() {
            if b.task.status == "running" && !now.iter().any(|t| t.task_id == b.task.id) {
                b.task.status = "completed".into();
            }
        }
        self.trim_bg();
    }

    /// Only the latest `ENDED_KEPT` ended tasks stay.
    fn trim_bg(&mut self) {
        let ended = self
            .bg
            .iter()
            .filter(|b| b.task.status != "running")
            .count();
        let mut drop = ended.saturating_sub(ENDED_KEPT);
        self.bg.retain(|b| {
            let gone = drop > 0 && b.task.status != "running";
            if gone {
                drop -= 1;
            }
            !gone
        });
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
    /// Whether a tool call of the turn under way is still running.
    pub fn tool_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether a shell command of the turn under way is still running.
    pub fn shell_running(&self) -> bool {
        self.running.iter().any(|i| self.shells.contains(i))
    }

    /// Whether a running shell command is one claude can put in the background yet. claude
    /// makes a task of a foreground command (`task_started`, `is_backgrounded` false) about
    /// three seconds after it starts, and a `background_tasks` before then moves nothing and
    /// says so to no one (2.1.311, measured 2026-11-06). Claude Code's own terminal waits out
    /// the same grace before it moves anything.
    pub fn shell_movable(&self) -> bool {
        self.bg.iter().any(|b| {
            self.tools
                .get(&b.tool_use_id)
                .is_some_and(|i| self.running.contains(i) && self.shells.contains(i))
        })
    }

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
            let parent = parent.to_string();
            let msg = sdk::classify(v);
            if !matches!(msg, Msg::Permission { .. } | Msg::Question { .. }) {
                self.feed_sub(&parent, v, &mut out);
            }
            match msg {
                // A subagent's permission prompt still needs the owner.
                Msg::Permission { .. } | Msg::Question { .. } => {}
                Msg::Assistant { blocks, .. } => {
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
                    self.show_bg(&mut out);
                    return out;
                }
                _ => return out,
            }
        }
        let msg = sdk::classify(v);
        let quiet = matches!(
            msg,
            Msg::Prompt(_)
                | Msg::Noise
                | Msg::Limits { .. }
                | Msg::Unknown(_)
                | Msg::Lifecycle { .. }
                | Msg::ToolSummary { .. }
                | Msg::Response { .. }
        );
        if !quiet {
            self.flush_local(&mut out);
        }
        match msg {
            Msg::Assistant {
                blocks,
                synthetic: true,
                ..
            } if blocks.iter().all(|b| matches!(b, Block::Text(_))) => {
                for b in blocks {
                    if let Block::Text(t) = b {
                        self.local.push(t);
                    }
                }
            }
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
            // A window claude did not read this time keeps its last reading.
            Msg::Limits {
                five_hour,
                seven_day,
            } => {
                let meta = Meta {
                    five_hour: five_hour.or(self.meta.five_hour),
                    seven_day: seven_day.or(self.meta.seven_day),
                    ..self.meta.clone()
                };
                self.set_meta(&mut out, meta);
            }
            Msg::Status(status) => match status.as_deref() {
                Some("compacting") => self.set_state(&mut out, "compacting"),
                Some(_) => self.set_state(&mut out, "working"),
                None => {}
            },
            // A command's echo comes after its reply, which is drawn under it; any other
            // prompt is drawn after what was waiting.
            Msg::Prompt(text) => {
                // Echoed: no longer on its way, whatever claude's lifecycle lines say after.
                let echoed = self.said_order.iter().find(|u| {
                    self.said.get(*u).is_some_and(|(t, st)| {
                        matches!(st.as_str(), "sent" | "queued") && t.trim() == text.trim()
                    })
                });
                if let Some(u) = echoed.cloned() {
                    if let Some(said) = self.said.get_mut(&u) {
                        said.1 = "echoed".into();
                    }
                }
                let command = text.starts_with('/');
                if !command {
                    self.flush_local(&mut out);
                }
                self.add(&mut out, Item::Prompt(text));
                self.set_state(&mut out, "working");
                self.flush_local(&mut out);
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
            Msg::Assistant {
                message_id, blocks, ..
            } => {
                for b in blocks {
                    match b {
                        Block::Text(text) => self.finish_text(&mut out, &message_id, text),
                        Block::ToolUse { id, name, input } if is_task_tool(&name) => {
                            let kind = input
                                .get("subagent_type")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            self.calls.insert(id.clone(), (name.clone(), kind));
                            let arg = if name == "Workflow" {
                                workflow_title(&input)
                            } else {
                                tool_arg(&name, &input)
                            };
                            let shown = agent_name(&name, &input);
                            let item = self.add(&mut out, Item::Agent { name: shown, arg });
                            self.tools.insert(id.clone(), item);
                            // A subagent is a foreground task too, which Ctrl+B backgrounds.
                            self.running.insert(item);
                            let mut convo = Box::new(Reducer::new());
                            // The agent's view starts with what it was asked.
                            if let Some(p) = input.get("prompt").and_then(Value::as_str) {
                                let mut first = Out::default();
                                convo.add(&mut first, Item::Prompt(p.to_string()));
                                convo.set_state(&mut first, "working");
                                out.sub
                                    .extend(first.ops.into_iter().map(|op| (id.clone(), op)));
                            }
                            self.tasks.insert(
                                id.clone(),
                                TaskView {
                                    item,
                                    workflow: name == "Workflow",
                                    script: input
                                        .get("script")
                                        .and_then(Value::as_str)
                                        .map(String::from),
                                    convo,
                                    ..TaskView::default()
                                },
                            );
                            self.show_task(&id, &mut out);
                        }
                        Block::ToolUse { id, name, input } => {
                            if matches!(name.as_str(), "Bash" | "Monitor") {
                                let cmd = input.get("command").and_then(Value::as_str);
                                let cmd = cut(cmd.unwrap_or("").lines().next().unwrap_or(""));
                                self.calls.insert(id.clone(), (name.clone(), cmd));
                            }
                            let arg = tool_arg(&name, &input);
                            let shell = name == "Bash";
                            let item = self.add(&mut out, Item::Tool { name, arg });
                            self.tools.insert(id, item);
                            self.running.insert(item);
                            if shell {
                                self.shells.insert(item);
                            }
                        }
                        Block::Narration(text) => {
                            self.add(&mut out, Item::Narration(text));
                        }
                        Block::Other(_) => {}
                    }
                }
            }
            Msg::ToolResults(results) => {
                for r in results {
                    // A call whose task was not heard of by its result never had one.
                    if !self.bg.iter().any(|b| b.tool_use_id == r.tool_use_id) {
                        self.calls.remove(&r.tool_use_id);
                    }
                    let Some(&id) = self.tools.get(&r.tool_use_id) else {
                        continue;
                    };
                    self.running.remove(&id);
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
            Msg::Notice(text) => {
                self.add(&mut out, Item::Notice(text));
                self.set_state(&mut out, "working");
            }
            // Claude takes it as a prompt, and answers it.
            Msg::Message(m) => {
                self.add(&mut out, Item::Message(m));
                self.set_state(&mut out, "working");
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
            Msg::TasksNow(now) => {
                self.tasks_now(now);
                self.show_bg(&mut out);
            }
            Msg::Task {
                task_id,
                tool_use_id,
                event,
            } => {
                self.bg_event(&task_id, &tool_use_id, &event);
                let Some(t) = self.tasks.get_mut(&tool_use_id) else {
                    // A background shell command's task, or one before this log began.
                    self.show_bg(&mut out);
                    return out;
                };
                match event {
                    sdk::Task::Started { .. } => {}
                    // Ended: its own view stops working.
                    sdk::Task::Updated { status, .. } if status != "running" => {
                        let mut o = Out::default();
                        t.convo.set_state(&mut o, "idle");
                        out.sub
                            .extend(o.ops.into_iter().map(|op| (tool_use_id.clone(), op)));
                    }
                    sdk::Task::Updated { .. } => {}
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
                        let mut o = Out::default();
                        t.convo.set_state(&mut o, "idle");
                        out.sub
                            .extend(o.ops.into_iter().map(|op| (tool_use_id.clone(), op)));
                    }
                }
                self.show_task(&tool_use_id, &mut out);
                self.show_bg(&mut out);
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
                self.running.clear();
                self.shells.clear();
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
                    self.said_order.retain(|u| self.said.contains_key(u));
                    self.said_order.push(uuid.clone());
                    self.said.insert(uuid, (text, "sent".into()));
                }
                sdk::Input::Other => {}
            },
            Msg::ToolSummary { summary, ids } if !summary.is_empty() => {
                let mut items: Vec<u64> = ids
                    .iter()
                    .filter_map(|i| self.tools.get(i).copied())
                    .collect();
                items.sort_unstable();
                if let Some(&group) = items.first() {
                    for id in items {
                        out.ops.push(Op::Summary {
                            id,
                            text: summary.clone(),
                            group,
                        });
                    }
                }
            }
            Msg::ToolSummary { .. } => {}
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
    /// The slash command reply that waited for its echo, drawn now.
    fn flush_local(&mut self, out: &mut Out) {
        for t in std::mem::take(&mut self.local) {
            if !t.is_empty() {
                self.add(out, Item::Text(t));
            }
        }
    }

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
/// the last progress op of each agent, and the last task list, is kept.
pub fn coalesce(ops: Vec<Op>) -> Vec<Op> {
    let mut merged: Vec<Op> = Vec::with_capacity(ops.len());
    for op in ops {
        // A progress op replaces what was under its agent, so an earlier one in the batch is
        // already out of date.
        if let Op::Progress { id, .. } = &op {
            merged.retain(|m| !matches!(m, Op::Progress { id: was, .. } if was == id));
        }
        // So does a task list.
        if let Op::Tasks(_) = &op {
            merged.retain(|m| !matches!(m, Op::Tasks(_)));
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
            Op::Summary { id: n, text, group } => obj(vec![
                ("t", Value::string("sum")),
                id(n),
                ("x", Value::string(text.clone())),
                ("g", Value::num(*group as f64)),
            ]),
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
            Op::Tasks(list) => obj(vec![
                ("t", Value::string("tasks")),
                ("l", Value::Arr(list.iter().map(BgTask::to_value).collect())),
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
                for (k, w) in [("u5", m.five_hour), ("u7", m.seven_day)] {
                    if let Some(w) = w {
                        v.set(
                            k,
                            obj(vec![
                                ("p", Value::num(w.pct)),
                                ("r", Value::num(w.resets_at as f64)),
                            ]),
                        );
                    }
                }
                v
            }
        }
    }
}

impl BgTask {
    fn to_value(&self) -> Value {
        let mut v = obj(vec![
            ("i", Value::string(self.id.clone())),
            ("k", Value::string(self.kind.clone())),
            ("d", Value::string(self.description.clone())),
            ("st", Value::string(self.status.clone())),
        ]);
        if !self.detail.is_empty() {
            v.set("c", Value::string(self.detail.clone()));
        }
        let set = |v: &mut Value, k: &str, n: Option<u64>| {
            if let Some(n) = n {
                v.set(k, Value::num(n as f64));
            }
        };
        if !self.prompt.is_empty() {
            v.set("p", Value::string(self.prompt.clone()));
        }
        if !self.model.is_empty() {
            v.set("m", Value::string(self.model.clone()));
        }
        if !self.activity.is_empty() {
            v.set("ac", Value::string(self.activity.clone()));
        }
        if self.saveable {
            v.set("sv", Value::Bool(true));
        }
        let nonzero = |n: u64| (n > 0).then_some(n);
        set(&mut v, "tk", nonzero(self.tokens));
        set(&mut v, "tc", nonzero(self.tools));
        set(&mut v, "ad", nonzero(self.agents_done));
        set(&mut v, "an", nonzero(self.agents));
        set(&mut v, "t0", self.started_ms);
        set(&mut v, "t1", self.ended_ms);
        set(&mut v, "it", self.item);
        v
    }
}

impl Item {
    fn to_value(&self) -> Value {
        let k = |k: &str| ("k", Value::string(k));
        match self {
            Item::Prompt(x) => obj(vec![k("user"), ("x", Value::string(x.clone()))]),
            Item::Text(x) => obj(vec![k("text"), ("x", Value::string(x.clone()))]),
            // Text to a client that does not know the flag.
            Item::Narration(x) => obj(vec![
                k("text"),
                ("x", Value::string(x.clone())),
                ("sm", Value::Bool(true)),
            ]),
            Item::Note(x) => obj(vec![k("note"), ("x", Value::string(x.clone()))]),
            // A note to a client that does not know the flag.
            Item::Notice(x) => obj(vec![
                k("note"),
                ("x", Value::string(x.clone())),
                ("tn", Value::Bool(true)),
            ]),
            // An older client skips it.
            Item::Message(m) => {
                let mut v = obj(vec![
                    k("msg"),
                    ("n", Value::string(m.from.clone())),
                    ("x", Value::string(m.body.clone())),
                ]);
                if m.inline {
                    v.set("i", Value::Bool(true));
                }
                v
            }
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
            all.sub.extend(o.sub);
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
    fn usage_limits_ride_the_meta_op_and_a_window_not_read_again_keeps_its_reading() {
        let mut r = Reducer::new();
        let both = feed_all(
            &mut r,
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.12,"resetsAt":1793990000},"seven_day":{"utilization":0.72,"resetsAt":1794006000}}}}"#,
            ],
        );
        assert_eq!(
            wire(&both.ops),
            [
                r#"{"cwd":"","e":"","m":"","pm":"","sid":"","t":"meta","ti":"","u5":{"p":12,"r":1793990000},"u7":{"p":72,"r":1794006000},"ver":""}"#
            ]
        );
        let weekly = feed_all(
            &mut r,
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"seven_day":{"utilization":0.73,"resetsAt":1794006000}}}}"#,
            ],
        );
        assert_eq!(
            wire(&weekly.ops),
            [
                r#"{"cwd":"","e":"","m":"","pm":"","sid":"","t":"meta","ti":"","u5":{"p":12,"r":1793990000},"u7":{"p":73,"r":1794006000},"ver":""}"#
            ]
        );
        // The same reading again, or one with no window, sends nothing.
        let again = feed_all(
            &mut r,
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"seven_day":{"utilization":0.731,"resetsAt":1794006000}}}}"#,
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            ],
        );
        assert!(again.ops.is_empty());
    }

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
    fn a_tool_call_runs_from_its_use_to_its_result_or_the_turns_end() {
        let mut r = Reducer::new();
        let use_ = |id: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{id}","content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"sleep 20"}}}}]}}}}"#
            )
        };
        assert!(!r.tool_running());
        feed_all(&mut r, &[&use_("t1")]);
        assert!(r.tool_running(), "Ctrl+X Ctrl+S backgrounds it");
        feed_all(
            &mut r,
            &[
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Command was manually backgrounded"}]}}"#,
            ],
        );
        assert!(!r.tool_running(), "its result came, backgrounded or done");
        // One the turn ended without a result for: not running into the next turn.
        feed_all(&mut r, &[&use_("t2")]);
        feed_all(
            &mut r,
            &[
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s"}"#,
            ],
        );
        assert!(!r.tool_running());
    }

    #[test]
    fn only_a_running_shell_command_is_one_a_prompt_backgrounds() {
        let mut r = Reducer::new();
        let use_ = |id: &str, name: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{id}","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{"command":"sleep 20","file_path":"/a"}}}}]}}}}"#
            )
        };
        let result = |id: &str| {
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","content":"ok"}}]}}}}"#
            )
        };
        feed_all(&mut r, &[&use_("t1", "Read")]);
        assert!(
            r.tool_running() && !r.shell_running(),
            "a read is not a command"
        );
        feed_all(&mut r, &[&use_("t2", "Bash")]);
        assert!(r.shell_running());
        feed_all(&mut r, &[&result("t2")]);
        assert!(
            !r.shell_running() && r.tool_running(),
            "the read still runs"
        );
        feed_all(&mut r, &[&use_("t3", "Bash")]);
        feed_all(
            &mut r,
            &[r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s"}"#],
        );
        assert!(!r.shell_running(), "the turn ended");
    }

    #[test]
    fn a_tool_use_summary_goes_on_each_call_of_its_step() {
        let mut r = Reducer::new();
        let use_ = |id: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{id}","content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
            )
        };
        feed_all(&mut r, &[&use_("t1"), &use_("t2"), &use_("t3")]);
        // As 2.1.311 sends one: a step's ids, a moment after the step.
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"tool_use_summary","summary":"Listed root, read hostname","preceding_tool_use_ids":["t2","t1"],"uuid":"u","session_id":"s"}"#,
                r#"{"type":"tool_use_summary","summary":"x","preceding_tool_use_ids":["nope"],"uuid":"u","session_id":"s"}"#,
            ],
        );
        let lines: Vec<String> = o
            .ops
            .iter()
            .map(|op| crate::json::to_line(&op.to_value()))
            .collect();
        assert_eq!(
            lines,
            [
                r#"{"g":1,"id":1,"t":"sum","x":"Listed root, read hostname"}"#,
                r#"{"g":1,"id":2,"t":"sum","x":"Listed root, read hostname"}"#,
            ]
        );
    }

    #[test]
    fn narration_between_tool_calls_is_an_item_of_its_own_marked_as_a_summary() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"","signature":"x"}]}}"#,
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"Checking the relay next.","signature":"x"}]},"narration_block_indexes":[0]}"#,
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            ],
        );
        let lines: Vec<String> = o
            .ops
            .iter()
            .map(|op| crate::json::to_line(&op.to_value()))
            .collect();
        assert_eq!(
            lines[0],
            r#"{"id":1,"k":"text","sm":true,"t":"add","x":"Checking the relay next."}"#
        );
        assert!(lines[1].contains(r#""k":"tool""#), "{lines:?}");
        assert_eq!(lines.len(), 2, "ordinary thinking draws nothing: {lines:?}");
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
    fn a_slash_commands_reply_is_drawn_under_the_command_as_it_was_typed() {
        // What headless claude 2.1.311 sent for `/usage` on 2026-11-02: its reply first, then
        // the command's echo in its wrapper, then the result.
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"7ce1922b","model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"Current week (all models): 23% used"}]},"parent_tool_use_id":null}"#,
                r#"{"type":"user","message":{"role":"user","content":"<command-name>/usage</command-name>\n            <command-message>usage</command-message>\n            <command-args></command-args>"},"parent_tool_use_id":null,"isReplay":true}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"Current week (all models): 23% used"}"#,
            ],
        );
        let w = wire(&o.ops);
        assert_eq!(
            &w[..3],
            [
                r#"{"id":1,"k":"user","t":"add","x":"/usage"}"#,
                r#"{"s":"working","t":"st"}"#,
                r#"{"id":2,"k":"text","t":"add","x":"Current week (all models): 23% used"}"#,
            ],
            "{w:?}"
        );
        assert_eq!(
            sdk::command_typed("<command-name>/btw</command-name><command-args>a b</command-args>")
                .as_deref(),
            Some("/btw a b")
        );
    }

    #[test]
    fn an_agents_message_is_an_item_of_its_own_and_starts_a_turn() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"user","message":{"role":"user","content":"<agent-message from=\"a1\">\nreport\n</agent-message>"},"isReplay":true,"origin":{"kind":"peer","name":"Explore","handbackReportText":"All done."}}"#,
                r#"{"type":"user","message":{"role":"user","content":"<cross-session-message from=\"infra/claude-22\">\nPinned.\n</cross-session-message>"},"isReplay":true}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"k":"msg","n":"Explore","t":"add","x":"All done."}"#,
                r#"{"s":"working","t":"st"}"#,
                r#"{"i":true,"id":2,"k":"msg","n":"infra/claude-22","t":"add","x":"Pinned."}"#,
            ]
        );
    }

    #[test]
    fn a_background_tasks_notification_is_its_summary_not_its_markup() {
        // As claude replays it on the phone (owner, 2026-11-02), and as a transcript keeps it.
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>a1</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n<summary>Agent \"Review\" finished</summary>\n<result>see the handback</result>\n</task-notification>"},"parent_tool_use_id":null,"isReplay":true}"#,
            ],
        );
        assert_eq!(
            wire(&o.ops),
            [
                r#"{"id":1,"k":"note","t":"add","tn":true,"x":"Agent \"Review\" finished"}"#,
                r#"{"s":"working","t":"st"}"#,
            ]
        );
    }

    #[test]
    fn a_prompt_sent_and_not_yet_echoed_is_known_as_on_its_way() {
        let mut r = Reducer::new();
        let say = |u: &str, x: &str| {
            format!(
                r#"{{"type":"_in","line":{}}}"#,
                crate::json::to_line(&sdk::prompt(x, u))
            )
        };
        let a = "aaaaaaaa-1111-4111-8111-111111111111";
        let b = "bbbbbbbb-1111-4111-8111-111111111111";
        feed_all(
            &mut r,
            &[
                &say(a, "first"),
                &format!(r#"{{"type":"command_lifecycle","command_uuid":"{a}","state":"queued"}}"#),
                &format!(
                    r#"{{"type":"command_lifecycle","command_uuid":"{a}","state":"started"}}"#
                ),
                r#"{"type":"user","message":{"role":"user","content":"first"},"isReplay":true}"#,
                &say(b, "behind it"),
                &format!(r#"{{"type":"command_lifecycle","command_uuid":"{b}","state":"queued"}}"#),
            ],
        );
        assert_eq!(
            r.waiting_prompts(),
            [(b.to_string(), "behind it".to_string(), true)]
        );
        // Echoed, it is on its way no more, even before claude says it started.
        feed_all(
            &mut r,
            &[r#"{"type":"user","message":{"role":"user","content":"behind it"},"isReplay":true}"#],
        );
        assert!(r.waiting_prompts().is_empty());
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

    // Background tasks as 2.1.311 wrote them on 2026-11-04, cut to the fields read: two
    // shells, a monitor and a subagent started at once, the subagent finishing, the monitor
    // stopped from the host (`stop_task`).
    const BACKGROUND: &[&str] = &[
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_s","name":"Bash","input":{"command":"for i in $(seq 60); do echo tick $i; sleep 2; done","description":"Print ticks","run_in_background":true}}]}}"#,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bub8w8bhh","run_id":"0mw1kjm9v-295e62a9","task_type":"local_bash","description":"Print ticks"}]}"#,
        r#"{"type":"system","subtype":"task_started","task_id":"bub8w8bhh","run_id":"0mw1kjm9v-295e62a9","tool_use_id":"tu_s","description":"Print ticks","is_backgrounded":true,"task_type":"local_bash"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_s","content":"Command running in background with ID: bub8w8bhh."}]}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_a","name":"Agent","input":{"description":"List big files","prompt":"…","subagent_type":"Explore","run_in_background":true}}]}}"#,
        r#"{"type":"system","subtype":"task_started","task_id":"a46e","run_id":"0mw1kjn2l-1614a111","tool_use_id":"tu_a","description":"List big files","subagent_type":"Explore","is_backgrounded":true,"task_type":"local_agent","prompt":"Find the five largest files."}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_m","name":"Monitor","input":{"command":"for i in $(seq 30); do echo mon $i; sleep 3; done","description":"Watch mon","timeout_ms":300000}}]}}"#,
        r#"{"type":"system","subtype":"task_started","task_id":"byee","run_id":"0mw1kjpk7-e36b8f12","tool_use_id":"tu_m","description":"Watch mon","is_backgrounded":true,"task_type":"local_bash"}"#,
        r#"{"type":"system","subtype":"task_updated","task_id":"a46e","run_id":"0mw1kjn2l-1614a111","patch":{"status":"completed","end_time":1793763624232}}"#,
        r#"{"type":"system","subtype":"task_notification","task_id":"a46e","tool_use_id":"tu_a","status":"completed","summary":"done","usage":{"total_tokens":1,"tool_uses":1,"duration_ms":4500}}"#,
        r#"{"type":"system","subtype":"task_updated","task_id":"byee","patch":{"status":"killed","end_time":1793763656919}}"#,
        r#"{"type":"system","subtype":"task_notification","task_id":"byee","tool_use_id":"tu_m","status":"stopped","summary":"Watch mon"}"#,
    ];

    fn last_tasks(ops: &[Op]) -> Option<String> {
        ops.iter()
            .rev()
            .find(|o| matches!(o, Op::Tasks(_)))
            .map(|o| to_line(&o.to_value()))
    }

    #[test]
    fn background_shells_monitors_and_agents_are_listed_as_claude_has_them() {
        let mut r = Reducer::new();
        let o = feed_all(&mut r, BACKGROUND);
        assert_eq!(
            last_tasks(&o.ops).unwrap(),
            concat!(
                r#"{"l":["#,
                r#"{"c":"for i in $(seq 60); do echo tick $i; sleep 2; done","d":"Print ticks","i":"bub8w8bhh","k":"shell","st":"running","t0":1793763618691},"#,
                r#"{"c":"Explore","d":"List big files","i":"a46e","it":2,"k":"agent","p":"Find the five largest files.","st":"completed","t0":1793763619725,"t1":1793763624232,"tc":1,"tk":1},"#,
                r#"{"c":"for i in $(seq 30); do echo mon $i; sleep 3; done","d":"Watch mon","i":"byee","k":"monitor","st":"stopped","t0":1793763622951,"t1":1793763656919}"#,
                r#"],"t":"tasks"}"#
            )
        );
        // The start's time is the run id's, which the end's measures against: 4.5 s.
        assert_eq!(sdk::run_started("0mw1kjn2l-1614a111"), Some(1793763619725));
        assert_eq!(sdk::run_started("toolu_x"), None);
        assert_eq!(sdk::run_started("zzzzzzzzzzzz-1"), None, "out of reason");
        // A line that changes nothing in the list sends no list.
        let again = feed_all(&mut r, &[BACKGROUND[11]]);
        assert_eq!(last_tasks(&again.ops), None);
    }

    #[test]
    fn a_task_claude_no_longer_lists_has_ended_and_only_the_latest_ended_are_kept() {
        let mut r = Reducer::new();
        let now = |ids: &[&str]| {
            let tasks: Vec<String> = ids
                .iter()
                .map(|i| {
                    format!(r#"{{"task_id":"{i}","task_type":"local_bash","description":"d{i}"}}"#)
                })
                .collect();
            format!(
                r#"{{"type":"system","subtype":"background_tasks_changed","tasks":[{}]}}"#,
                tasks.join(",")
            )
        };
        let o = feed_all(&mut r, &[&now(&["a", "b"])]);
        assert_eq!(
            last_tasks(&o.ops).unwrap(),
            r#"{"l":[{"d":"da","i":"a","k":"shell","st":"running"},{"d":"db","i":"b","k":"shell","st":"running"}],"t":"tasks"}"#
        );
        // `a` is gone from claude's list with no line saying how it ended.
        let o = feed_all(&mut r, &[&now(&["b"])]);
        assert!(
            last_tasks(&o.ops)
                .unwrap()
                .contains(r#""i":"a","k":"shell","st":"completed""#)
        );
        // Many more come and go: the running one stays, and only ENDED_KEPT ended ones.
        for n in 0..(ENDED_KEPT + 5) {
            let id = format!("x{n}");
            feed_all(&mut r, &[&now(&["b", &id]), &now(&["b"])]);
        }
        let ended: Vec<&str> =
            r.bg.iter()
                .filter(|b| b.task.status != "running")
                .map(|b| b.task.id.as_str())
                .collect();
        assert_eq!(ended.len(), ENDED_KEPT);
        assert_eq!(ended.last(), Some(&format!("x{}", ENDED_KEPT + 4).as_str()));
        assert!(
            r.bg.iter()
                .any(|b| b.task.id == "b" && b.task.status == "running")
        );
        // A foreground call's command is not kept once its result comes without a task.
        feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"fg","name":"Bash","input":{"command":"ls"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"fg","content":"x"}]}}"#,
            ],
        );
        assert!(!r.calls.contains_key("fg"));
    }

    #[test]
    fn a_subagents_own_conversation_is_reduced_under_its_call_with_its_model_and_counts() {
        let mut r = Reducer::new();
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_a","name":"Agent","input":{"description":"slow reader","prompt":"Read the files.","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
                r#"{"type":"system","subtype":"task_started","task_id":"ae72","run_id":"0mw1z3876-605c2715","tool_use_id":"tu_a","description":"slow reader","task_type":"local_agent","prompt":"Read the files."}"#,
                r#"{"type":"assistant","parent_tool_use_id":"tu_a","message":{"id":"s1","model":"claude-haiku-5-5","content":[{"type":"tool_use","id":"tu_r","name":"Read","input":{"file_path":"/usr/share/doc/apt/copyright"}}]}}"#,
                r#"{"type":"user","parent_tool_use_id":"tu_a","message":{"role":"user","content":[{"tool_use_id":"tu_r","type":"tool_result","content":"Format: x\nLicense: GPL-2"}]}}"#,
                r#"{"type":"system","subtype":"task_progress","task_id":"ae72","tool_use_id":"tu_a","usage":{"total_tokens":16612,"tool_uses":1,"duration_ms":2093}}"#,
            ],
        );
        let sub: Vec<String> = o
            .sub
            .iter()
            .map(|(k, op)| format!("{k} {}", to_line(&op.to_value())))
            .collect();
        assert_eq!(
            sub,
            [
                r#"tu_a {"id":1,"k":"user","t":"add","x":"Read the files."}"#,
                r#"tu_a {"s":"working","t":"st"}"#,
                r#"tu_a {"a":"/usr/share/doc/apt/copyright","id":2,"k":"tool","n":"Read","t":"add"}"#,
                r#"tu_a {"id":2,"l":2,"ok":true,"p":["Format: x","License: GPL-2"],"t":"res"}"#,
            ]
        );
        assert_eq!(r.tool_of_task("ae72"), Some("tu_a"));
        let list = last_tasks(&o.ops).unwrap();
        assert!(
            list.contains(r#""ac":"Read(/usr/share/doc/apt/copyright)""#)
                && list.contains(r#""m":"claude-haiku-5-5""#)
                && list.contains(r#""tc":1,"tk":16612"#),
            "{list}"
        );
        // Its end stops its own view working.
        let o = feed_all(
            &mut r,
            &[
                r#"{"type":"system","subtype":"task_notification","task_id":"ae72","tool_use_id":"tu_a","status":"completed"}"#,
            ],
        );
        assert!(o.sub.iter().any(|(_, op)| *op == Op::State("idle".into())));
    }

    #[test]
    fn a_workflows_agents_are_kept_for_its_browser_with_one_agents_previews() {
        let mut r = Reducer::new();
        feed_all(
            &mut r,
            &[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"tu_w","name":"Workflow","input":{"script":"export const meta = {name:'two-readers', description:'Two agents'}\n"}}]}}"#,
                r#"{"type":"system","subtype":"task_started","task_id":"wm4","run_id":"0mw1z4zbr-626ec235","tool_use_id":"tu_w","description":"Two agents","task_type":"local_workflow"}"#,
                r#"{"type":"system","subtype":"task_progress","task_id":"wm4","tool_use_id":"tu_w","usage":{"total_tokens":0,"tool_uses":0,"duration_ms":1},"workflow_progress":[{"type":"workflow_phase","index":1,"title":"Read"},{"type":"workflow_agent","index":1,"label":"reader-a","phaseIndex":1,"model":"claude-haiku-5-5","state":"progress","startedAt":1793788130153,"lastToolName":"Read","lastToolSummary":"/usr/share/doc/sed/copyright","promptPreview":"Read three files","tokens":30029,"toolCalls":3},{"type":"workflow_agent","index":2,"label":"reader-b","phaseIndex":1,"model":"claude-haiku-5-5","state":"done","startedAt":1793788130154,"tokens":26757,"toolCalls":3,"durationMs":6939,"resultPreview":"GPL-3+"}]}"#,
            ],
        );
        let v = r.workflow_view("wm4", Some(1)).unwrap();
        assert_eq!(
            to_line(&v),
            concat!(
                r#"{"ag":[{"l":"reader-a","m":"claude-haiku-5-5","ph":1,"st":"progress","t0":1793788130153,"tc":3,"tk":30029,"tl":"Read(/usr/share/doc/sed/copyright)"},"#,
                r#"{"l":"reader-b","m":"claude-haiku-5-5","ms":6939,"ph":1,"rp":"GPL-3+","st":"done","t0":1793788130154,"tc":3,"tk":26757}],"ph":["Read"]}"#
            )
        );
        assert_eq!(
            r.workflow_script("wm4"),
            Some("export const meta = {name:'two-readers', description:'Two agents'}\n")
        );
        let list = to_line(&Op::Tasks(r.bg.iter().map(|b| b.task.clone()).collect()).to_value());
        assert!(
            list.contains(r#""ad":1,"an":2"#) && list.contains(r#""sv":true"#),
            "{list}"
        );
        assert!(r.workflow_view("nope", None).is_none());
    }
}
