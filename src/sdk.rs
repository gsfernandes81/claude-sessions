//! Claude Code's stream-json control protocol: what a headless claude writes, read into the
//! few things the remote view acts on, and the lines it is sent (`docs/remote-view.md`).
//!
//! claude runs as `claude -p --input-format stream-json --output-format stream-json --verbose
//! --include-partial-messages --permission-prompt-tool stdio`. Every line either way is one
//! JSON object. Read **tolerantly**, as the hook payloads are: the protocol is Claude Code's,
//! it grows a message type most releases, and a line this version does not know is `Unknown`
//! or `Noise`, never an error. What each variant is was read from 2.1.311's real output on
//! 2026-11-01 (the spike recorded in `docs/remote-view.md` § *The protocol, measured*).
//!
//! Pure: no I/O. The holder and the relay own the pipes; this only says what a line means.

use crate::json::Value;

/// One line from claude, read for what the remote view does with it.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// `system/init`, re-sent before every turn: the conversation's id, model and mode.
    Init {
        session_id: String,
        model: String,
        permission_mode: String,
        cwd: String,
        version: String,
        /// The slash commands claude has, without their `/`: its own, skills and plugins'.
        commands: Vec<String>,
    },
    /// `system/status`: `requesting`, `compacting`, or none (back to idle or streaming).
    Status(Option<String>),
    /// Streamed text for the content block `index` of the message `message_id` being written
    /// (the stream event's `api_message_id`, the same id its `assistant` line carries).
    TextDelta {
        message_id: String,
        index: u64,
        text: String,
    },
    /// A prompt, echoed back by `--replay-user-messages` (`isReplay`): how every view sees
    /// what any of them sent, in claude's own order.
    Prompt(String),
    /// A finished assistant message, or the part of one finished so far: claude writes an
    /// `assistant` line per content block, all under the same message id.
    Assistant {
        message_id: String,
        blocks: Vec<Block>,
        /// Written by claude itself, not the model: a slash command's reply, such as
        /// `/usage`'s, which comes before the command's own echo.
        synthetic: bool,
    },
    /// A background task's `<task-notification>`, which claude hands the model as a prompt
    /// and so starts a turn with: its summary.
    Notice(String),
    /// A message another agent sent this conversation, which claude hands the model as a
    /// prompt: a subagent's hand-back (`<agent-message>`), another session's
    /// (`<cross-session-message>`), a teammate's (`<teammate-message>`).
    Message(Message),
    /// Tool results, from a `user` line whose content is `tool_result` blocks.
    ToolResults(Vec<ToolResult>),
    /// A local command's output, `<local-command-stdout>…</local-command-stdout>`: what
    /// `/model`, `/compact` and the like answer with.
    LocalOutput(String),
    /// Any other `user` line's text: a compaction's summary, `[Request interrupted by user]`.
    UserText(String),
    /// `can_use_tool` for anything but a question: claude waits for allow or deny.
    Permission {
        request_id: String,
        tool: String,
        input: Value,
        description: Option<String>,
        /// Claude's `permission_suggestions`: the rules "Yes, and don't ask again" would add,
        /// sent back as `updatedPermissions` when the owner picks it. Empty when none.
        suggestions: Vec<Value>,
    },
    /// `can_use_tool` for `AskUserQuestion`: answered by allowing it with the answers added.
    Question {
        request_id: String,
        input: Value,
        questions: Vec<Question>,
    },
    /// Any other request claude makes of the host (`hook_callback`, `elicitation`,
    /// `mcp_message`, …): refused at once with `refuse`, so claude never waits on a host
    /// that has nothing to say.
    OtherRequest { request_id: String, subtype: String },
    /// claude withdrew a request it made, e.g. a permission it no longer needs.
    Cancelled { request_id: String },
    /// The answer to a request we sent.
    Response {
        request_id: String,
        ok: bool,
        body: Value,
    },
    /// `system/session_title_changed`, which `/rename` and `rename_session` both cause.
    TitleChanged(String),
    /// `system/compact_boundary`: the conversation was compacted, from and to so many tokens.
    Compacted {
        pre_tokens: Option<u64>,
        post_tokens: Option<u64>,
    },
    /// `result`: the turn ended. `subtype` is `success` or an `error_*`.
    Result {
        subtype: String,
        is_error: bool,
        session_id: String,
    },
    /// `rate_limit_event` with its `unifiedWindows`: how much of the account's 5-hour and
    /// weekly limits is used, each window only when claude read it.
    Limits {
        five_hour: Option<Window>,
        seven_day: Option<Window>,
    },
    /// Known and of no use to the view: hook progress, thinking-token estimates, a rate-limit
    /// reading with no window in it, keep-alives and the stream events other than text.
    Noise,
    /// A line a view sent claude, as the holder logs it (`{"type":"_in","line":…}`): read
    /// back so every view sees what any of them answered or changed.
    Input(Input),
    /// `command_lifecycle`: where a prompt sent with a uuid has got to — `queued`, `started`,
    /// `cancelled` (an interrupt caught it, answered or not), and so on.
    Lifecycle { uuid: String, state: String },
    /// A question answered before this claude started, read from the transcript's
    /// `toolUseResult` on a resume: no request comes for it, so it arrives settled.
    Answered {
        questions: Vec<Question>,
        answers: Vec<(String, String)>,
    },
    /// `system/task_started`, `task_progress`, `task_updated` and `task_notification`: a
    /// background shell, monitor, subagent or workflow, by claude's `task_id`, and the tool
    /// call that started it (`tool_use_id`, empty on `task_updated`).
    Task {
        task_id: String,
        tool_use_id: String,
        event: Task,
    },
    /// `system/background_tasks_changed`: the tasks running in the background now, in
    /// claude's order. Sent whenever one starts or ends, so it is the list as claude has it.
    TasksNow(Vec<TaskNow>),
    /// A line of a type this version does not know, kept by its name so a log can say so.
    Unknown(String),
}

/// A usage-limit window: the percentage used, rounded as Claude Code rounds it, and when it
/// resets, in epoch seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub pct: u8,
    pub resets_at: u64,
}

/// One entry of `background_tasks_changed`.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskNow {
    pub task_id: String,
    pub task_type: String,
    pub description: String,
    pub started_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Task {
    /// `task_type` is `local_agent`, `local_workflow`, `local_bash` (a shell or a monitor),
    /// `remote_agent`, …; `started_ms` when it started, read from its `run_id` (`run_started`).
    Started {
        task_type: String,
        description: String,
        started_ms: Option<u64>,
        /// A subagent's prompt, empty for anything else.
        prompt: String,
        /// A workflow's name, from its script's `meta`.
        workflow_name: String,
    },
    /// `task_updated`: its status changed, and when it ended if it has.
    Updated {
        status: String,
        ended_ms: Option<u64>,
    },
    /// The running totals, and a workflow's phases and agents as last reported (empty when a
    /// progress line did not carry them).
    Progress {
        usage: Usage,
        phases: Vec<String>,
        agents: Vec<WorkflowAgent>,
    },
    /// `status` is `completed`, `failed`, `stopped`, …; `report` a subagent's handback.
    Done {
        status: String,
        usage: Usage,
        report: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkflowAgent {
    pub label: String,
    /// 1-based, into the phases.
    pub phase: u64,
    /// `start`, `progress`, `done`, `failed`, `error`, `queued`, …
    pub state: String,
    pub last_tool: String,
    /// What the last tool call was on: a path, a pattern (`lastToolSummary`).
    pub last_summary: String,
    pub model: String,
    pub tokens: u64,
    pub tool_calls: u64,
    /// When it started, epoch milliseconds, and how long it took once done.
    pub started_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// The start of its prompt and of its result, as claude previews them.
    pub prompt: String,
    pub result: String,
    pub error: String,
}

fn usage(v: &Value) -> Usage {
    let u = v.get("usage");
    let n = |k| {
        u.and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        tokens: n("total_tokens"),
        tool_uses: n("tool_uses"),
        duration_ms: n("duration_ms"),
    }
}

/// When a task started: its `run_id` is that time in base-36 milliseconds, a dash, then a
/// random part (`0mw1kjn2l-1614a111`, 2.1.311, measured against its `end_time` on
/// 2026-11-04). `None` for anything else, so a view falls back on when it first heard of it.
pub fn run_started(run_id: &str) -> Option<u64> {
    let (head, _) = run_id.split_once('-')?;
    let ms = u64::from_str_radix(head, 36).ok()?;
    // Within reason: after 2020 and before 2100.
    (1_577_836_800_000..4_102_444_800_000)
        .contains(&ms)
        .then_some(ms)
}

fn task(v: &Value, sub: &str) -> Msg {
    let task_id = s(v, "task_id");
    let tool_use_id = s(v, "tool_use_id");
    if tool_use_id.is_empty() && task_id.is_empty() {
        return Msg::Noise;
    }
    let event = match sub {
        "task_started" => Task::Started {
            task_type: s(v, "task_type"),
            description: s(v, "description"),
            started_ms: run_started(&s(v, "run_id")),
            prompt: s(v, "prompt"),
            workflow_name: s(v, "workflow_name"),
        },
        "task_updated" => {
            let patch = v.get("patch");
            Task::Updated {
                status: patch
                    .and_then(|p| p.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                ended_ms: patch
                    .and_then(|p| p.get("end_time"))
                    .and_then(Value::as_u64),
            }
        }
        "task_progress" => {
            let mut phases = Vec::new();
            let mut agents = Vec::new();
            if let Some(Value::Arr(items)) = v.get("workflow_progress") {
                for it in items {
                    match it.get("type").and_then(Value::as_str) {
                        Some("workflow_phase") => phases.push(s(it, "title")),
                        Some("workflow_agent") => {
                            let n = |k: &str| it.get(k).and_then(Value::as_u64);
                            agents.push(WorkflowAgent {
                                label: s(it, "label"),
                                phase: n("phaseIndex").unwrap_or(0),
                                state: s(it, "state"),
                                last_tool: s(it, "lastToolName"),
                                last_summary: s(it, "lastToolSummary"),
                                model: s(it, "model"),
                                tokens: n("tokens").unwrap_or(0),
                                tool_calls: n("toolCalls").unwrap_or(0),
                                started_ms: n("startedAt"),
                                duration_ms: n("durationMs"),
                                prompt: s(it, "promptPreview"),
                                result: s(it, "resultPreview"),
                                error: s(it, "error"),
                            })
                        }
                        _ => {}
                    }
                }
            }
            Task::Progress {
                usage: usage(v),
                phases,
                agents,
            }
        }
        _ => Task::Done {
            status: s(v, "status"),
            usage: usage(v),
            report: v
                .get("handback_report")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str)
                .map(String::from),
        },
    };
    Msg::Task {
        task_id,
        tool_use_id,
        event,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A thinking block the server marked as narration: its summary of the prose between tool
    /// calls, which Claude Code draws as the assistant's text with a grey `· summary` after it
    /// (2.1.311). Its text, trimmed.
    Narration(String),
    /// Thinking, redacted thinking, images: drawn as their kind, not their content.
    Other(String),
}

/// A message from another agent, as Claude Code draws one: `› Message from @Explore`.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Who sent it: a subagent's type, a session's name, a teammate's id; or Claude Code's
    /// own word when it says none: `agent`, `peer`, `teammate`.
    pub from: String,
    /// What it said, without the envelope: a hand-back's report without its frame.
    pub body: String,
    /// Claude Code puts a peer session's words on the collapsed line itself
    /// (`› Message from @x: …`); a subagent's and a teammate's wait for Ctrl+O.
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub text: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    /// Each option's label and description.
    pub options: Vec<(String, String)>,
    /// Each option's preview, the same length as `options`: empty for one without, as most
    /// are. Claude Code draws a preview beside the list, or under it when narrow (R4b, R4c).
    pub previews: Vec<String>,
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Read one line from claude.
pub fn classify(v: &Value) -> Msg {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    match kind {
        "system" => match sub {
            "init" => Msg::Init {
                session_id: s(v, "session_id"),
                model: s(v, "model"),
                permission_mode: s(v, "permissionMode"),
                cwd: s(v, "cwd"),
                version: s(v, "claude_code_version"),
                commands: v
                    .get("slash_commands")
                    .and_then(Value::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect(),
            },
            "status" => Msg::Status(v.get("status").and_then(Value::as_str).map(String::from)),
            "session_title_changed" => Msg::TitleChanged(s(v, "title")),
            "compact_boundary" => {
                let meta = v.get("compact_metadata");
                let n = |k| meta.and_then(|m| m.get(k)).and_then(Value::as_u64);
                Msg::Compacted {
                    pre_tokens: n("pre_tokens"),
                    post_tokens: n("post_tokens"),
                }
            }
            "task_started" | "task_progress" | "task_updated" | "task_notification" => task(v, sub),
            "background_tasks_changed" => Msg::TasksNow(
                v.get("tasks")
                    .and_then(Value::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(|t| TaskNow {
                        task_id: s(t, "task_id"),
                        task_type: s(t, "task_type"),
                        description: s(t, "description"),
                        started_ms: run_started(&s(t, "run_id")),
                    })
                    .filter(|t| !t.task_id.is_empty())
                    .collect(),
            ),
            "hook_started" | "hook_response" | "hook_progress" | "thinking_tokens" => Msg::Noise,
            other => Msg::Unknown(format!("system/{other}")),
        },
        "stream_event" => {
            let ev = v.get("event");
            let ev_kind = ev.and_then(|e| e.get("type")).and_then(Value::as_str);
            let delta = ev.and_then(|e| e.get("delta"));
            let delta_kind = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
            match (ev_kind, delta_kind) {
                (Some("content_block_delta"), Some("text_delta")) => Msg::TextDelta {
                    message_id: s(v, "api_message_id"),
                    index: ev
                        .and_then(|e| e.get("index"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    text: delta.map(|d| s(d, "text")).unwrap_or_default(),
                },
                _ => Msg::Noise,
            }
        }
        "assistant" => {
            let msg = v.get("message");
            // Claude Code's own list of the frame's narration blocks; a transcript line has
            // none, and its blocks are read by their signatures, as Claude Code reads them.
            let listed: Option<Vec<u64>> = v
                .get("narration_block_indexes")
                .and_then(Value::as_arr)
                .map(|a| a.iter().filter_map(Value::as_u64).collect());
            let blocks = msg
                .and_then(|m| m.get("content"))
                .and_then(Value::as_arr)
                .unwrap_or(&[])
                .iter()
                .enumerate()
                .map(|(i, b)| match narration(b, i, listed.as_deref()) {
                    Some(text) => Block::Narration(text),
                    None => block(b),
                })
                .collect();
            Msg::Assistant {
                message_id: msg.map(|m| s(m, "id")).unwrap_or_default(),
                blocks,
                synthetic: msg.is_some_and(|m| s(m, "model") == "<synthetic>"),
            }
        }
        "user" => user(v),
        "command_lifecycle" => Msg::Lifecycle {
            uuid: s(v, "command_uuid"),
            state: s(v, "state"),
        },
        "_in" => Msg::Input(input(v.get("line").unwrap_or(&Value::Null))),
        "_history" => history(v.get("line").unwrap_or(&Value::Null)),
        // The history is in: whatever it left open was settled before this claude started.
        "_history_end" => Msg::Result {
            subtype: "success".into(),
            is_error: false,
            session_id: String::new(),
        },
        "control_request" => request(v),
        "control_cancel_request" => Msg::Cancelled {
            request_id: s(v, "request_id"),
        },
        "control_response" => {
            let r = v.get("response");
            let get = |k| r.map(|r| s(r, k)).unwrap_or_default();
            let ok = get("subtype") == "success";
            let body = r
                .and_then(|r| {
                    if ok {
                        r.get("response")
                    } else {
                        r.get("error")
                    }
                })
                .cloned()
                .unwrap_or(Value::Null);
            Msg::Response {
                request_id: get("request_id"),
                ok,
                body,
            }
        }
        "result" => Msg::Result {
            subtype: sub.to_string(),
            is_error: v
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(sub != "success"),
            session_id: s(v, "session_id"),
        },
        "rate_limit_event" => limits(v),
        "keep_alive" | "tool_progress" | "prompt_suggestion" => Msg::Noise,
        other => Msg::Unknown(other.to_string()),
    }
}

/// A `rate_limit_event`'s windows. `utilization` is a fraction, shown by Claude Code as
/// `Math.round(utilization*100)`; a window it cannot read is left out, not guessed.
fn limits(v: &Value) -> Msg {
    let windows = v
        .get("rate_limit_info")
        .and_then(|i| i.get("unifiedWindows"));
    let window = |key: &str| {
        let w = windows?.get(key)?;
        let used = w.get("utilization")?.as_f64()?;
        let resets_at = w.get("resetsAt")?.as_u64_lenient()?;
        if !used.is_finite() {
            return None;
        }
        Some(Window {
            pct: (used * 100.0).round().clamp(0.0, 100.0) as u8,
            resets_at,
        })
    };
    match (window("five_hour"), window("seven_day")) {
        (None, None) => Msg::Noise,
        (five_hour, seven_day) => Msg::Limits {
            five_hour,
            seven_day,
        },
    }
}

/// The tool call a line belongs to, when it comes from inside one: a subagent's messages carry
/// the `Task` call's id as `parent_tool_use_id`. `None` for the conversation's own lines.
pub fn parent_tool_use_id(v: &Value) -> Option<&str> {
    v.get("parent_tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// A thinking block's text when it is narration, as Claude Code 2.1.311 decides: listed in
/// the frame's `narration_block_indexes` where the frame has that list, else the block's
/// signature saying `narration` (its `uTe`); and only with text, since one without is ordinary
/// thinking (its `K7e`).
fn narration(b: &Value, index: usize, listed: Option<&[u64]>) -> Option<String> {
    if b.get("type").and_then(Value::as_str) != Some("thinking") {
        return None;
    }
    let text = b.get("thinking").and_then(Value::as_str)?.trim();
    if text.is_empty() {
        return None;
    }
    let is = match listed {
        Some(l) => l.contains(&(index as u64)),
        None => b
            .get("signature")
            .and_then(Value::as_str)
            .and_then(block_kind)
            .is_some_and(|k| k == "narration"),
    };
    is.then(|| text.to_string())
}

/// The block kind a thinking signature carries: base64 of a protobuf whose field 2 holds a
/// message whose field 1 holds the kind as field 8 — read as Claude Code 2.1.311 reads it
/// (`psn`), the last of a repeated field winning. `None` for anything it cannot read, which
/// is ordinary thinking, as Claude Code fails closed.
fn block_kind(signature: &str) -> Option<String> {
    let bytes = base64_decode(signature)?;
    let outer = proto_field(&bytes, 2)?;
    let inner = proto_field(outer, 1)?;
    String::from_utf8(proto_field(inner, 8)?.to_vec()).ok()
}

/// The bytes of the last length-delimited field `want` in a protobuf message; `None` when the
/// message is malformed or lacks it.
fn proto_field(mut b: &[u8], want: u64) -> Option<&[u8]> {
    fn varint(b: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = b.split_first()?;
            *b = rest;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let mut found = None;
    while !b.is_empty() {
        let key = varint(&mut b)?;
        let skip = match key & 7 {
            0 => {
                varint(&mut b)?;
                0
            }
            1 => 8,
            2 => {
                let len = usize::try_from(varint(&mut b)?).ok()?;
                let (body, rest) = (b.get(..len)?, b.get(len..)?);
                if key >> 3 == want {
                    found = Some(body);
                }
                b = rest;
                0
            }
            5 => 4,
            _ => return None,
        };
        b = b.get(skip..)?;
    }
    found
}

/// Standard base64, padded or not; `None` for anything else.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().take_while(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn block(b: &Value) -> Block {
    match b.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => Block::Text(s(b, "text")),
        "tool_use" => Block::ToolUse {
            id: s(b, "id"),
            name: s(b, "name"),
            input: b.get("input").cloned().unwrap_or_else(Value::obj),
        },
        other => Block::Other(other.to_string()),
    }
}

/// A tool result's text: a string, or the text blocks of an array (an image is named, not
/// drawn).
fn result_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::Str(t)) => t.clone(),
        Some(Value::Arr(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => s(p, "text"),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What `/model`, `/compact` and the like answer with, and what one says when it fails.
const LOCAL_TAGS: [&str; 2] = ["local-command-stdout", "local-command-stderr"];

/// A local command's output without its wrapper, if `t` is one.
fn local_output(t: &str) -> Option<String> {
    LOCAL_TAGS.iter().find_map(|name| {
        let rest = t.strip_prefix(&format!("<{name}>"))?;
        Some(
            rest.strip_suffix(&format!("</{name}>"))
                .unwrap_or(rest)
                .to_string(),
        )
    })
}

/// The tags Claude Code wraps what it hands the model as a user message in, which it never
/// shows as typed (2.1.311's own list): a line that opens with one is not a prompt the owner
/// typed, so it is never drawn as one. Those the view draws are read before this list is.
const WRAPPERS: [&str; 28] = [
    "task-notification",
    "agent-message",
    "teammate-message",
    "cross-session-message",
    "remote-review",
    "remote-review-progress",
    "slack-ping",
    "slack-tag-message",
    "tick",
    "fetched-web-content",
    "coordinator-relay",
    "channel",
    "system-reminder",
    "system_reminder",
    "bash-input",
    "bash-stdout",
    "bash-stderr",
    "bash-exit-code",
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "command-message",
    "command-contents",
    "user-prompt-submit-hook",
    "mcp-resource-update",
    "mcp-polling-update",
    "fork-boilerplate",
    "user-memory-input",
];

/// Whether `t` opens with `<name` as a whole tag name.
fn opens(t: &str, name: &str) -> bool {
    t.strip_prefix('<')
        .and_then(|r| r.strip_prefix(name))
        .is_some_and(|r| r.starts_with([' ', '>', '\n', '/']))
}

/// Where a wrapped message's own tag starts: at the top, under Claude Code's one-line
/// lead-in (`Another Claude session sent a message:`), or inside the `<system-reminder>` a
/// message that came mid-turn rides in.
fn unwrapped(t: &str) -> &str {
    let mut t = t.trim_start();
    if opens(t, "system-reminder") {
        t = t.split_once('>').map_or("", |(_, r)| r).trim_start();
    }
    if let Some((first, rest)) = t.split_once('\n') {
        if !first.starts_with('<') && first.trim_end().ends_with(':') && rest.starts_with('<') {
            return rest;
        }
    }
    t
}

/// Whether `t` is Claude Code's wrapping of something, not a prompt typed.
fn is_wrapped(t: &str) -> bool {
    [t.trim_start(), unwrapped(t)]
        .iter()
        .any(|t| WRAPPERS.iter().any(|w| opens(t, w)))
}

/// `name="value"` among a tag's attributes.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = attrs[from..].find(&key) {
        let at = from + i;
        let starts = attrs[..at].ends_with(char::is_whitespace);
        let rest = &attrs[at + key.len()..];
        if starts {
            return rest.split_once('"').map(|(v, _)| v.to_string());
        }
        from = at + key.len();
    }
    None
}

/// Claude Code's frame around a subagent's report, the last line of which this is.
const HANDBACK_START: &str = "[Subagent hand-back]";
const HANDBACK_END: &str = "The report follows:\n";

/// A hand-back's report without its frame and the indent the frame adds, as Claude Code shows
/// it; any other message as it came.
fn unframed(body: &str) -> String {
    if !body.starts_with(HANDBACK_START) {
        return body.to_string();
    }
    let Some((_, report)) = body.split_once(HANDBACK_END) else {
        return body.to_string();
    };
    if report.lines().all(|l| l.starts_with("  ")) {
        report
            .lines()
            .map(|l| &l[2..])
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        report.to_string()
    }
}

/// A message another agent sent, if `t` is one: who from, read as Claude Code reads it (the
/// line's `origin` first, then the tag's own attributes), and what it said.
fn message(t: &str, origin: Option<&Value>) -> Option<Message> {
    let t = unwrapped(t);
    let (name, fallback, inline) = [
        ("agent-message", "agent", false),
        ("cross-session-message", "peer", true),
        ("teammate-message", "teammate", false),
    ]
    .into_iter()
    .find(|(name, ..)| opens(t, name))?;
    let (attrs, inner) = t[1 + name.len()..].split_once('>')?;
    let close = format!("</{name}>");
    let inner = inner.rfind(&close).map_or(inner, |i| &inner[..i]);
    let inner = inner.strip_prefix('\n').unwrap_or(inner).trim_end();
    let o = |k: &str| {
        origin
            .and_then(|o| o.get(k))
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(String::from)
    };
    let from = o("name")
        .or_else(|| attr(attrs, "from-name"))
        .or_else(|| match name {
            "teammate-message" => attr(attrs, "teammate_id"),
            // A session's address is a name; a subagent's `from` is a task id, which Claude
            // Code does not show.
            "cross-session-message" => attr(attrs, "from"),
            _ => None,
        })
        .unwrap_or_else(|| fallback.to_string());
    let body = match o("handbackReportText") {
        Some(report) if name == "agent-message" => report,
        _ => unframed(inner),
    };
    Some(Message {
        from,
        body: body.trim_end().to_string(),
        inline,
    })
}

/// Remove every `<name …>…</name>` from `t`.
fn without(t: &str, name: &str) -> String {
    let close = format!("</{name}>");
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    loop {
        let open = rest
            .match_indices('<')
            .map(|(i, _)| i)
            .find(|&i| opens(&rest[i..], name));
        let Some(i) = open else { break };
        let Some(j) = rest[i..].find(&close) else {
            break;
        };
        out.push_str(&rest[..i]);
        rest = &rest[i + j + close.len()..];
    }
    out.push_str(rest);
    out
}

/// A tool result as Claude Code shows it, not as the model reads it: the reminders claude
/// appends for the model and a sandbox's report left out, an error without its
/// `<tool_use_error>` wrapper, and a result too long to keep inline as the preview claude
/// kept of it.
fn shown_result(text: &str) -> String {
    let text = without(&without(text, "system-reminder"), "sandbox_violations");
    let t = text.trim();
    if let Some(inner) = t
        .strip_prefix("<tool_use_error>")
        .and_then(|r| r.strip_suffix("</tool_use_error>"))
    {
        return inner.to_string();
    }
    if let Some(inner) = t.strip_prefix("<persisted-output>") {
        let inner = inner.strip_suffix("</persisted-output>").unwrap_or(inner);
        return match inner.find("\nPreview (") {
            Some(i) => inner[i + 1..]
                .split_once('\n')
                .map_or("", |(_, r)| r)
                .to_string(),
            None => inner.trim().to_string(),
        };
    }
    text
}

/// What is between `<name>` and `</name>` in `t`, trimmed.
fn tag(t: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = t.find(&open)? + open.len();
    let to = from + t[from..].find(&close)?;
    Some(t[from..to].trim().to_string())
}

/// A slash command as it was typed, from the wrapper claude echoes it in:
/// `<command-name>/usage</command-name> … <command-args>a b</command-args>` is `/usage a b`.
pub(crate) fn command_typed(t: &str) -> Option<String> {
    if !t.starts_with("<command-name>") {
        return None;
    }
    let name = tag(t, "command-name")?;
    let name = if name.starts_with('/') {
        name
    } else {
        format!("/{name}")
    };
    Some(match tag(t, "command-args").filter(|a| !a.is_empty()) {
        Some(args) => format!("{name} {args}"),
        None => name,
    })
}

fn user(v: &Value) -> Msg {
    let content = v.get("message").and_then(|m| m.get("content"));
    let origin = v.get("origin");
    match content {
        // A background task's notice, which claude hands the model as a prompt: its summary,
        // as a note, not the markup.
        Some(Value::Str(t)) if t.trim_start().starts_with("<task-notification>") => {
            Msg::Notice(tag(t, "summary").unwrap_or_else(|| "Background task finished".into()))
        }
        Some(Value::Str(t)) => {
            if let Some(m) = message(t, origin) {
                return Msg::Message(m);
            }
            if let Some(out) = local_output(t) {
                return Msg::LocalOutput(out);
            }
            let replay = v.get("isReplay").and_then(Value::as_bool) == Some(true);
            match command_typed(t) {
                Some(typed) if replay => Msg::Prompt(typed),
                _ if replay && !is_wrapped(t) => Msg::Prompt(t.clone()),
                _ => Msg::UserText(t.clone()),
            }
        }
        Some(Value::Arr(parts)) => {
            let results: Vec<ToolResult> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
                .map(|p| ToolResult {
                    tool_use_id: s(p, "tool_use_id"),
                    text: result_text(p.get("content")),
                    is_error: p.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect();
            if results.is_empty() {
                let found = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .find_map(|p| message(&s(p, "text"), origin));
                return match found {
                    Some(m) => Msg::Message(m),
                    None => Msg::UserText(result_text(content)),
                };
            }
            // A result too long to keep inline: what the tool said, where claude says it
            // (a shell's), rather than the stand-in the model got.
            let said = v.get("tool_use_result").and_then(|r| {
                let out = r.get("stdout").and_then(Value::as_str)?;
                let err = r.get("stderr").and_then(Value::as_str).unwrap_or("");
                Some(if err.is_empty() {
                    out.to_string()
                } else {
                    format!("{out}\n{err}")
                })
            });
            let single = results.len() == 1;
            Msg::ToolResults(
                results
                    .into_iter()
                    .map(|mut r| {
                        r.text = match &said {
                            Some(out)
                                if single
                                    && r.text.trim_start().starts_with("<persisted-output>") =>
                            {
                                out.clone()
                            }
                            _ => shown_result(&r.text),
                        };
                        r
                    })
                    .collect(),
            )
        }
        _ => Msg::UserText(String::new()),
    }
}

/// Whether a transcript line is a prompt typed, which starts a turn: where history may be cut
/// without parting a tool call from its result.
pub fn starts_turn(line: &Value) -> bool {
    matches!(history(line), Msg::Prompt(_))
}

/// A line of the conversation's transcript from before this claude, as the holder logs it on
/// a resume: claude does not send history again. A prompt typed then reads as a prompt;
/// Claude Code's own bookkeeping (meta lines, compaction summaries, command wrappers,
/// subagents' lines) as nothing.
fn history(line: &Value) -> Msg {
    let flag = |k: &str| line.get(k).and_then(Value::as_bool) == Some(true);
    if flag("isMeta") || flag("isCompactSummary") || flag("isSidechain") {
        return Msg::Noise;
    }
    match line.get("type").and_then(Value::as_str) {
        Some("user") => {
            let text = line
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str);
            match text {
                Some(t) if t.starts_with("<command-") || t.starts_with("<local-command") => {
                    Msg::Noise
                }
                Some(_) => {
                    let mut l = line.clone();
                    l.set("isReplay", Value::Bool(true));
                    user(&l)
                }
                None => match line.get("toolUseResult") {
                    Some(r) if r.get("answers").is_some() && r.get("questions").is_some() => {
                        Msg::Answered {
                            questions: questions_of(r),
                            answers: answers_of(r.get("answers")),
                        }
                    }
                    _ => user(line),
                },
            }
        }
        Some("assistant") => classify(line),
        // What reached claude mid-turn, delivered with the next tool results: an agent's
        // message or a task's notice, drawn as it is when it starts a turn. A prompt typed
        // mid-turn is left to its own line, which the transcript also has.
        Some("attachment") => {
            let a = line.get("attachment");
            let queued = a.and_then(|a| a.get("type")).and_then(Value::as_str);
            let prompt = a.and_then(|a| a.get("prompt")).and_then(Value::as_str);
            match (queued, prompt) {
                (Some("queued_command"), Some(p)) if is_wrapped(p) => {
                    let mut content = Value::obj();
                    content.set("content", Value::string(p.to_string()));
                    let mut l = Value::obj();
                    l.set("type", Value::string("user"));
                    l.set("message", content);
                    l.set("isReplay", Value::Bool(true));
                    if let Some(o) = a.and_then(|a| a.get("origin")) {
                        l.set("origin", o.clone());
                    }
                    user(&l)
                }
                _ => Msg::Noise,
            }
        }
        _ => Msg::Noise,
    }
}

/// An `AskUserQuestion` input's questions.
fn questions_of(input: &Value) -> Vec<Question> {
    input
        .get("questions")
        .and_then(Value::as_arr)
        .unwrap_or(&[])
        .iter()
        .map(|q| Question {
            question: s(q, "question"),
            header: s(q, "header"),
            multi_select: q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options: q
                .get("options")
                .and_then(Value::as_arr)
                .unwrap_or(&[])
                .iter()
                .map(|o| (s(o, "label"), s(o, "description")))
                .collect(),
            previews: q
                .get("options")
                .and_then(Value::as_arr)
                .unwrap_or(&[])
                .iter()
                .map(|o| s(o, "preview"))
                .collect(),
        })
        .collect()
}

/// The `answers` of an `AskUserQuestion`, keyed by each question's text.
fn answers_of(v: Option<&Value>) -> Vec<(String, String)> {
    match v {
        Some(Value::Obj(m)) => m
            .iter()
            .filter_map(|(q, a)| a.as_str().map(|a| (q.clone(), a.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

fn request(v: &Value) -> Msg {
    let request_id = s(v, "request_id");
    let Some(r) = v.get("request") else {
        return Msg::OtherRequest {
            request_id,
            subtype: String::new(),
        };
    };
    let subtype = s(r, "subtype");
    if subtype != "can_use_tool" {
        return Msg::OtherRequest {
            request_id,
            subtype,
        };
    }
    let tool = s(r, "tool_name");
    let input = r.get("input").cloned().unwrap_or_else(Value::obj);
    if tool == "AskUserQuestion" {
        let questions = questions_of(&input);
        return Msg::Question {
            request_id,
            input,
            questions,
        };
    }
    Msg::Permission {
        request_id,
        tool,
        description: r
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        input,
        suggestions: r
            .get("permission_suggestions")
            .and_then(Value::as_arr)
            .map(<[Value]>::to_vec)
            .unwrap_or_default(),
    }
}

// ---- What claude is sent ----

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut o = Value::obj();
    for (k, v) in pairs {
        o.set(k, v);
    }
    o
}

/// A request of ours, under our own id.
pub fn control(request_id: &str, request: Value) -> Value {
    obj(vec![
        ("type", Value::string("control_request")),
        ("request_id", Value::string(request_id)),
        ("request", request),
    ])
}

fn subtype(name: &str, mut rest: Vec<(&str, Value)>) -> Value {
    rest.push(("subtype", Value::string(name)));
    obj(rest)
}

/// The first line: answered with the commands, models and account claude has.
/// What a view sent, as far as drawing it goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// An answer to a permission prompt or question: `allowed`, `answered` (a question's
    /// answers), `denied` or `refused`.
    Answer {
        request_id: String,
        how: String,
        /// A question's answers, keyed by its text; empty for anything else.
        answers: Vec<(String, String)>,
    },
    Model(String),
    /// `None` is back to the model's default.
    Effort(Option<String>),
    Mode(String),
    /// A prompt sent with a uuid (`prompt`), so its lifecycle can be told back to the view.
    Say {
        uuid: String,
        text: String,
    },
    /// An interrupt, an end, a rename: claude says what came of each itself.
    Other,
}

fn input(line: &Value) -> Input {
    let kind = line.get("type").and_then(Value::as_str).unwrap_or("");
    let req = line.get("request");
    let sub = req.map(|r| s(r, "subtype")).unwrap_or_default();
    match (kind, sub.as_str()) {
        ("control_response", _) => {
            let r = line.get("response");
            let get = |k| r.map(|r| s(r, k)).unwrap_or_default();
            let outcome = r.and_then(|r| r.get("response"));
            let mut given = Vec::new();
            let how = match (get("subtype").as_str(), outcome.map(|o| s(o, "behavior"))) {
                ("success", Some(b)) if b == "allow" => {
                    let answers = outcome
                        .and_then(|o| o.get("updatedInput"))
                        .and_then(|i| i.get("answers"));
                    given = answers_of(answers);
                    if answers.is_some() {
                        "answered"
                    } else {
                        "allowed"
                    }
                }
                ("success", Some(b)) if b == "deny" => "denied",
                _ => "refused",
            };
            Input::Answer {
                request_id: get("request_id"),
                how: how.to_string(),
                answers: given,
            }
        }
        ("control_request", "set_model") => {
            Input::Model(req.map(|r| s(r, "model")).unwrap_or_default())
        }
        ("control_request", "set_permission_mode") => {
            Input::Mode(req.map(|r| s(r, "mode")).unwrap_or_default())
        }
        ("user", _) if !s(line, "uuid").is_empty() => Input::Say {
            uuid: s(line, "uuid"),
            text: line
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        ("control_request", "apply_flag_settings") => Input::Effort(
            req.and_then(|r| r.get("settings"))
                .and_then(|st| st.get("effortLevel"))
                .and_then(Value::as_str)
                .map(String::from),
        ),
        _ => Input::Other,
    }
}

/// The permission mode, as `/auto` cycles it: `default`, `acceptEdits`, `plan`, `auto`…
pub fn set_permission_mode(request_id: &str, mode: &str) -> Value {
    control(
        request_id,
        subtype("set_permission_mode", vec![("mode", Value::string(mode))]),
    )
}

pub fn initialize(request_id: &str) -> Value {
    control(request_id, subtype("initialize", vec![]))
}

/// A prompt as a view sends it: `user_text` with a uuid, which claude's `command_lifecycle`
/// lines name, so an interrupt that catches it unanswered can hand it back to the view.
pub fn prompt(text: &str, uuid: &str) -> Value {
    let mut v = user_text(text);
    v.set("uuid", Value::string(uuid));
    v
}

/// A prompt, or a slash command typed as one (`/compact`, `/clear`).
pub fn user_text(text: &str) -> Value {
    obj(vec![
        ("type", Value::string("user")),
        (
            "message",
            obj(vec![
                ("role", Value::string("user")),
                ("content", Value::string(text)),
            ]),
        ),
    ])
}

pub fn set_model(request_id: &str, model: &str) -> Value {
    control(
        request_id,
        subtype("set_model", vec![("model", Value::string(model))]),
    )
}

/// Effort has no request of its own: Claude Code's `/effort` sends this overlay to a remote
/// session, and the next turn uses it. `None` goes back to the model's default.
pub fn set_effort(request_id: &str, level: Option<&str>) -> Value {
    let level = level.map(Value::string).unwrap_or(Value::Null);
    control(
        request_id,
        subtype(
            "apply_flag_settings",
            vec![("settings", obj(vec![("effortLevel", level)]))],
        ),
    )
}

/// `session_id` makes claude refuse it if the process has moved to another conversation
/// since (a `/clear`), rather than rename the wrong one.
pub fn rename(request_id: &str, title: &str, session_id: &str) -> Value {
    let mut fields = vec![
        ("title", Value::string(title)),
        ("source", Value::string("host")),
    ];
    // Only when known: headless claude says its session in `init`, which comes with its first
    // reply, and refuses an empty one ("session_id is not the current session"). The field is
    // optional, and without it the rename is for the session claude runs.
    if !session_id.is_empty() {
        fields.push(("session_id", Value::string(session_id)));
    }
    control(request_id, subtype("rename_session", fields))
}

/// `/rc`, Claude Code's `/remote-control`: the session reachable from claude.ai/code and the
/// Claude app, or not. Answered with `session_url` when on (2.1.311, tried 2026-11-02).
pub fn remote_control(request_id: &str, enabled: bool, name: &str) -> Value {
    let mut fields = vec![("enabled", Value::Bool(enabled))];
    if enabled && !name.is_empty() {
        fields.push(("name", Value::string(name)));
    }
    control(request_id, subtype("remote_control", fields))
}

pub fn interrupt(request_id: &str) -> Value {
    control(request_id, subtype("interrupt", vec![]))
}

/// A prompt still waiting in claude's queue, withdrawn by its uuid; nothing if claude has
/// taken it in already.
pub fn cancel_async_message(request_id: &str, uuid: &str) -> Value {
    control(
        request_id,
        subtype(
            "cancel_async_message",
            vec![("message_uuid", Value::string(uuid))],
        ),
    )
}

/// Ctrl+B: the running Bash commands and subagents go on in the background, and the turn
/// goes on without them, so a prompt waiting behind it is taken in at once.
pub fn background_tasks(request_id: &str) -> Value {
    control(request_id, subtype("background_tasks", vec![]))
}

/// A background shell's or monitor's output so far: answered `{output, total_bytes,
/// truncated}`, or an error for any other kind of task.
pub fn get_task_output(request_id: &str, task_id: &str) -> Value {
    control(
        request_id,
        subtype("get_task_output", vec![("task_id", Value::string(task_id))]),
    )
}

/// Interrupt one background agent, as Esc does in Claude Code's view of it: headless claude
/// stops it (`task_updated` `killed`; measured 2026-11-04). Claude Code's terminal can then
/// message it to go on; headless claude refuses `send_task_message` (2.1.311).
pub fn interrupt_task(request_id: &str, task_id: &str) -> Value {
    control(
        request_id,
        subtype("interrupt", vec![("task_id", Value::string(task_id))]),
    )
}

/// Stop a background task, as `x` in Claude Code's tasks dialog does: claude answers, then
/// says it ended (`task_updated` `killed`, `task_notification` `stopped`).
pub fn stop_task(request_id: &str, task_id: &str) -> Value {
    control(
        request_id,
        subtype("stop_task", vec![("task_id", Value::string(task_id))]),
    )
}

/// Ctrl+X Ctrl+S, Claude Code's send-now: the prompts waiting behind the running turn reach
/// the model now. A claude without `interrupt_send_now_v1` reads it as a plain interrupt,
/// after which the waiting prompts run anyway.
pub fn interrupt_send_now(request_id: &str) -> Value {
    control(
        request_id,
        subtype("interrupt", vec![("send_now", Value::Bool(true))]),
    )
}

pub fn end_session(request_id: &str) -> Value {
    control(request_id, subtype("end_session", vec![]))
}

fn answer(request_id: &str, outcome: Value) -> Value {
    obj(vec![
        ("type", Value::string("control_response")),
        (
            "response",
            obj(vec![
                ("subtype", Value::string("success")),
                ("request_id", Value::string(request_id)),
                ("response", outcome),
            ]),
        ),
    ])
}

/// Allow a tool call, with its input as claude sent it or as edited.
pub fn allow(request_id: &str, input: Value) -> Value {
    answer(
        request_id,
        obj(vec![
            ("behavior", Value::string("allow")),
            ("updatedInput", input),
        ]),
    )
}

/// Allow a tool call and add the rules Claude suggested, so it is not asked again: Claude
/// Code's "Yes, and don't ask again".
pub fn allow_always(request_id: &str, input: Value, suggestions: &[Value]) -> Value {
    answer(
        request_id,
        obj(vec![
            ("behavior", Value::string("allow")),
            ("updatedInput", input),
            ("updatedPermissions", Value::Arr(suggestions.to_vec())),
        ]),
    )
}

/// Deny a tool call; `message` is what claude is told. With `interrupt`, the turn stops
/// there, as Claude Code's own No stops it: claude answers with `[Request interrupted by
/// user for tool use]` and ends the turn (2.1.311, measured 2026-11-06). Without, as for
/// Claude Code's No with something typed for Claude, the turn goes on with the denial.
pub fn deny(request_id: &str, message: &str, interrupt: bool) -> Value {
    let mut body = vec![
        ("behavior", Value::string("deny")),
        ("message", Value::string(message)),
    ];
    if interrupt {
        body.push(("interrupt", Value::Bool(true)));
    }
    answer(request_id, obj(body))
}

/// Answer an `AskUserQuestion`: allowed, with its input and `answers` keyed by each
/// question's text. A multi-select's labels are joined with ", ", and free text is the
/// answer as typed, as the Agent SDK documents.
pub fn answer_questions(request_id: &str, input: &Value, answers: &[(String, String)]) -> Value {
    let mut map = Value::obj();
    for (q, a) in answers {
        map.set(q, Value::string(a.clone()));
    }
    let mut input = input.clone();
    input.set("answers", map);
    allow(request_id, input)
}

/// Refuse a request claude made that the view does not serve.
pub fn refuse(request_id: &str, why: &str) -> Value {
    obj(vec![
        ("type", Value::string("control_response")),
        (
            "response",
            obj(vec![
                ("subtype", Value::string("error")),
                ("request_id", Value::string(request_id)),
                ("error", Value::string(why)),
            ]),
        ),
    ])
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn a_transcripts_lines_read_as_the_turns_they_were() {
        let h = |l: &str| {
            let line = crate::json::parse(l).unwrap();
            classify(&obj(vec![
                ("type", Value::string("_history")),
                ("line", line),
            ]))
        };
        assert_eq!(
            h(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#),
            Msg::Prompt("hi".into())
        );
        assert_eq!(
            h(r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"x"}}"#),
            Msg::Noise
        );
        assert_eq!(
            h(
                r#"{"type":"user","message":{"role":"user","content":"<command-name>/model</command-name>"}}"#
            ),
            Msg::Noise
        );
        assert!(matches!(
            h(
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"yo"}]}}"#
            ),
            Msg::Assistant { .. }
        ));
        assert_eq!(h(r#"{"type":"summary","summary":"s"}"#), Msg::Noise);
        let end = crate::json::parse(r#"{"type":"_history_end"}"#).unwrap();
        assert!(matches!(
            classify(&end),
            Msg::Result {
                is_error: false,
                ..
            }
        ));
    }

    fn logged(line: &Value) -> Value {
        obj(vec![("type", Value::string("_in")), ("line", line.clone())])
    }

    #[test]
    fn a_views_line_read_back_from_the_log_says_what_it_answered_or_changed() {
        let q = obj(vec![("questions", Value::Arr(vec![]))]);
        let cases = [
            (
                allow("r1", Value::obj()),
                Input::Answer {
                    request_id: "r1".into(),
                    how: "allowed".into(),
                    answers: vec![],
                },
            ),
            (
                answer_questions("r2", &q, &[("Which?".into(), "Red".into())]),
                Input::Answer {
                    request_id: "r2".into(),
                    how: "answered".into(),
                    answers: vec![("Which?".into(), "Red".into())],
                },
            ),
            (
                deny("r3", "no", false),
                Input::Answer {
                    request_id: "r3".into(),
                    how: "denied".into(),
                    answers: vec![],
                },
            ),
            (
                refuse("r4", "not served"),
                Input::Answer {
                    request_id: "r4".into(),
                    how: "refused".into(),
                    answers: vec![],
                },
            ),
            (set_model("c1", "opus"), Input::Model("opus".into())),
            (
                set_effort("c2", Some("high")),
                Input::Effort(Some("high".into())),
            ),
            (set_effort("c3", None), Input::Effort(None)),
            (
                set_permission_mode("c4", "plan"),
                Input::Mode("plan".into()),
            ),
            (user_text("hi"), Input::Other),
            (interrupt("c5"), Input::Other),
        ];
        for (line, want) in cases {
            assert_eq!(
                classify(&logged(&line)),
                Msg::Input(want),
                "{}",
                crate::json::to_line(&line)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{parse, to_line};

    fn read(line: &str) -> Msg {
        classify(&parse(line).expect("fixture parses"))
    }

    // The fixtures are 2.1.311's own lines from the spike, cut to the fields read and with
    // ids shortened; nothing personal is in them.

    #[test]
    fn init_status_title_and_compaction_read_as_what_they_say() {
        assert_eq!(
            read(
                r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"s1","tools":["Bash"],"model":"claude-haiku-5-5","permissionMode":"default","claude_code_version":"2.1.311"}"#
            ),
            Msg::Init {
                session_id: "s1".into(),
                model: "claude-haiku-5-5".into(),
                permission_mode: "default".into(),
                cwd: "/w".into(),
                version: "2.1.311".into(),
                commands: Vec::new(),
            }
        );
        assert_eq!(
            read(r#"{"type":"system","subtype":"status","status":"compacting","session_id":"s1"}"#),
            Msg::Status(Some("compacting".into()))
        );
        assert_eq!(
            read(
                r#"{"type":"system","subtype":"status","status":null,"compact_result":"success"}"#
            ),
            Msg::Status(None)
        );
        assert_eq!(
            read(
                r#"{"type":"system","subtype":"session_title_changed","title":"spike rename","session_id":"s1"}"#
            ),
            Msg::TitleChanged("spike rename".into())
        );
        assert_eq!(
            read(
                r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":29738,"post_tokens":8737}}"#
            ),
            Msg::Compacted {
                pre_tokens: Some(29738),
                post_tokens: Some(8737)
            }
        );
    }

    #[test]
    fn hook_progress_thinking_estimates_and_rate_limits_are_noise() {
        for line in [
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_response","exit_code":0}"#,
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}}"#,
        ] {
            assert_eq!(read(line), Msg::Noise, "{line}");
        }
    }

    #[test]
    fn a_rate_limit_event_says_the_windows_it_has() {
        // As claude 2.1.311 wrote it on 2026-11-04, with only the weekly window.
        assert_eq!(
            read(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1794006000,"rateLimitType":"seven_day","overageStatus":"rejected","isUsingOverage":false,"unifiedWindows":{"seven_day":{"utilization":0.72,"resetsAt":1794006000,"status":"allowed"}}}}"#
            ),
            Msg::Limits {
                five_hour: None,
                seven_day: Some(Window {
                    pct: 72,
                    resets_at: 1794006000
                }),
            }
        );
        assert_eq!(
            read(
                r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.125,"resetsAt":1793990000},"seven_day":{"utilization":"x","resetsAt":1}}}}"#
            ),
            Msg::Limits {
                five_hour: Some(Window {
                    pct: 13,
                    resets_at: 1793990000
                }),
                seven_day: None,
            }
        );
    }

    #[test]
    fn a_type_this_version_does_not_know_is_unknown_not_an_error() {
        assert_eq!(
            read(r#"{"type":"look","x":1}"#),
            Msg::Unknown("look".into())
        );
        assert_eq!(
            read(r#"{"type":"system","subtype":"placed"}"#),
            Msg::Unknown("system/placed".into())
        );
        assert_eq!(read(r#"{}"#), Msg::Unknown(String::new()));
        // A field of the wrong type reads as absent rather than failing the line.
        assert_eq!(
            read(r#"{"type":"system","subtype":"session_title_changed","title":7}"#),
            Msg::TitleChanged(String::new())
        );
    }

    #[test]
    fn streamed_text_and_finished_blocks() {
        assert_eq!(
            read(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"spi"}},"api_message_id":"msg_1"}"#
            ),
            Msg::TextDelta {
                message_id: "msg_1".into(),
                index: 1,
                text: "spi".into()
            }
        );
        let Msg::Assistant {
            message_id, blocks, ..
        } = read(
            r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo spike-ok"}},{"type":"thinking","thinking":"x"},{"type":"text","text":"done"}]}}"#,
        )
        else {
            panic!("an assistant line is an Assistant");
        };
        assert_eq!(message_id, "msg_1");
        assert_eq!(blocks.len(), 3);
        let Block::ToolUse { id, name, input } = &blocks[0] else {
            panic!("tool use")
        };
        assert_eq!((id.as_str(), name.as_str()), ("toolu_1", "Bash"));
        assert_eq!(
            input.get("command").and_then(Value::as_str),
            Some("echo spike-ok")
        );
        assert_eq!(blocks[1], Block::Other("thinking".into()));
        assert_eq!(blocks[2], Block::Text("done".into()));
    }

    /// Signatures shaped as 2.1.311's are: field 2, then field 1, then the block kind as field
    /// 8, among fields of every wire type.
    const NARRATION_SIG: &str = "CAQSFwoRCBMYAjgBQgluYXJyYXRpb24SAqq7GgMBAgM=";
    const THINKING_SIG: &str = "CAQSFgoQCBMYAjgBQgh0aGlua2luZxICqrsaAwECAw==";

    #[test]
    fn a_thinking_signature_says_its_block_kind_and_anything_unreadable_says_none() {
        assert_eq!(block_kind(NARRATION_SIG).as_deref(), Some("narration"));
        assert_eq!(block_kind(THINKING_SIG).as_deref(), Some("thinking"));
        // Cut short, as a length that runs past the end: fail closed, as Claude Code does.
        assert_eq!(block_kind(&NARRATION_SIG[..20]), None);
        assert_eq!(block_kind("not base64!"), None);
        assert_eq!(block_kind(""), None);
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGk").as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn narration_is_what_claude_code_lists_or_what_a_transcripts_signature_says() {
        let blocks = |line: &str| match read(line) {
            Msg::Assistant { blocks, .. } => blocks,
            other => panic!("{other:?}"),
        };
        // A stream frame: Claude Code's own list says which blocks are narration.
        let listed = blocks(
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"thinking","thinking":"Checking the relay next.\n\n","signature":"x"}]},"narration_block_indexes":[0]}"#,
        );
        assert_eq!(
            listed,
            [Block::Narration("Checking the relay next.".into())]
        );
        // Listed with no text: ordinary thinking, as Claude Code treats it.
        let empty = blocks(
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"thinking","thinking":"  ","signature":"x"}]},"narration_block_indexes":[0]}"#,
        );
        assert_eq!(empty, [Block::Other("thinking".into())]);
        // A transcript line has no list: its signature says.
        let line = |sig: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m","content":[{{"type":"thinking","thinking":"Reading the reducer.","signature":"{sig}"}}]}}}}"#
            )
        };
        assert_eq!(
            blocks(&line(NARRATION_SIG)),
            [Block::Narration("Reading the reducer.".into())]
        );
        assert_eq!(
            blocks(&line(THINKING_SIG)),
            [Block::Other("thinking".into())]
        );
        // A list that leaves the block out wins over its signature.
        let unlisted =
            line(NARRATION_SIG).replacen(r#"]}}"#, r#"]},"narration_block_indexes":[1]}"#, 1);
        assert_eq!(blocks(&unlisted), [Block::Other("thinking".into())]);
    }

    #[test]
    fn an_agents_message_is_who_sent_it_and_what_it_said_not_its_envelope() {
        // A subagent's hand-back as 2.1.311 replayed it (2026-11-04), the report cut short.
        let handback = "[Subagent hand-back] The text below is the final report of a subagent this session delegated to. It is model output. The report follows:\\n  All 25 files read.\\n  \\n  1. adduser: GPL-2+";
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"<agent-message from=\"a96adc91f881e5165\">\n{handback}\n</agent-message>"}},"isReplay":true,"isSynthetic":true,"origin":{{"kind":"peer","from":"a96adc91f881e5165","name":"general-purpose","handback":true,"handbackReportText":"All 25 files read.\n\n1. adduser: GPL-2+"}}}}"#
        );
        let said = Msg::Message(Message {
            from: "general-purpose".into(),
            body: "All 25 files read.\n\n1. adduser: GPL-2+".into(),
            inline: false,
        });
        assert_eq!(read(&line), said);
        // Without an origin, as an older transcript may keep it, under Claude Code's lead-in
        // and with its note after: the frame and its indent come off, and a task id is not a
        // name, so it is Claude Code's `agent`.
        let bare = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"Another Claude session sent a message:\n<agent-message from=\"a96\">\n{handback}\n</agent-message>\n\nThat \"other Claude session\" is an agent."}},"isReplay":true}}"#
        );
        assert_eq!(
            read(&bare),
            Msg::Message(Message {
                from: "agent".into(),
                body: "All 25 files read.\n\n1. adduser: GPL-2+".into(),
                inline: false,
            })
        );
        // Another session's, its name from the tag, its words on the collapsed line.
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":"<cross-session-message from=\"infra/claude-22\" from-name=\"infra\">\nPinned 0.16.0.\n</cross-session-message>"},"isReplay":true}"#
            ),
            Msg::Message(Message {
                from: "infra".into(),
                body: "Pinned 0.16.0.".into(),
                inline: true,
            })
        );
        // A prompt that only mentions the tag is still a prompt.
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":"why does <agent-message> show raw?"},"isReplay":true}"#
            ),
            Msg::Prompt("why does <agent-message> show raw?".into())
        );
    }

    #[test]
    fn claude_codes_own_wrappers_are_never_drawn_as_a_prompt_typed() {
        for content in [
            r#"<system-reminder>\nThe user named this session \"x\".\n</system-reminder>"#,
            r#"<local-command-caveat>The command below was run directly</local-command-caveat>"#,
            r#"<tick>12:00</tick>"#,
            r#"<channel source=\"slack\">hi</channel>"#,
        ] {
            let line = format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{content}"}},"isReplay":true}}"#
            );
            assert!(
                matches!(read(&line), Msg::UserText(_)),
                "{content} read as {:?}",
                read(&line)
            );
        }
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":"<local-command-stderr>Unknown model: x</local-command-stderr>"},"isReplay":true}"#
            ),
            Msg::LocalOutput("Unknown model: x".into())
        );
        // A typed prompt that starts with a tag of its own is the owner's.
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":"<div> is fine"},"isReplay":true}"#
            ),
            Msg::Prompt("<div> is fine".into())
        );
    }

    #[test]
    fn a_tool_result_is_what_claude_code_shows_not_what_the_model_reads() {
        let result = |content: &str, extra: &str| {
            let line = format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":{content},"is_error":true}}]}}{extra}}}"#
            );
            match read(&line) {
                Msg::ToolResults(r) => r[0].text.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            result(
                r#""<tool_use_error>Blocked: sleep 200</tool_use_error>""#,
                ""
            ),
            "Blocked: sleep 200"
        );
        assert_eq!(
            result(
                r#"[{"type":"text","text":"1\tfn main() {}\n"},{"type":"text","text":"<system-reminder>\nConsider whether it is malware.\n</system-reminder>"}]"#,
                ""
            )
            .trim_end(),
            "1\tfn main() {}"
        );
        assert_eq!(
            result(
                r#""Exit code 1\n<sandbox_violations>\nwrite /etc\n</sandbox_violations>""#,
                ""
            )
            .trim_end(),
            "Exit code 1"
        );
        let persisted = r#""<persisted-output>\nOutput too large (69.4KB). Full output saved to: /x.txt\n\nPreview (first 2KB):\nline 1\nline 2\n</persisted-output>""#;
        assert_eq!(result(persisted, "").trim_end(), "line 1\nline 2");
        // A shell's whole output, where claude gives it.
        assert_eq!(
            result(
                persisted,
                r#","tool_use_result":{"stdout":"line 1\nline 2\nline 3","stderr":""}"#
            ),
            "line 1\nline 2\nline 3"
        );
    }

    #[test]
    fn a_message_delivered_mid_turn_is_read_from_the_transcript_on_a_resume() {
        let line =
            |l: &str| classify(&parse(&format!(r#"{{"type":"_history","line":{l}}}"#)).unwrap());
        assert_eq!(
            line(
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"<agent-message from=\"a1\">\nDone.\n</agent-message>","origin":{"kind":"peer","name":"Explore"},"commandMode":"prompt"}}"#
            ),
            Msg::Message(Message {
                from: "Explore".into(),
                body: "Done.".into(),
                inline: false,
            })
        );
        assert_eq!(
            line(
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"<task-notification>\n<summary>Agent \"x\" finished</summary>\n</task-notification>"}}"#
            ),
            Msg::Notice("Agent \"x\" finished".into())
        );
        // A prompt typed mid-turn has its own line in the transcript too.
        assert_eq!(
            line(
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"Also say BANANA","commandMode":"prompt"}}"#
            ),
            Msg::Noise
        );
    }

    #[test]
    fn user_lines_are_results_local_output_or_text() {
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"spike-ok","is_error":false}]}}"#
            ),
            Msg::ToolResults(vec![ToolResult {
                tool_use_id: "toolu_1".into(),
                text: "spike-ok".into(),
                is_error: false
            }])
        );
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"a"},{"type":"image"}],"is_error":true}]}}"#
            ),
            Msg::ToolResults(vec![ToolResult {
                tool_use_id: "t2".into(),
                text: "a\n[image]".into(),
                is_error: true
            }])
        );
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to `haiku (claude-haiku-5-5)`</local-command-stdout>"}}"#
            ),
            Msg::LocalOutput("Set model to `haiku (claude-haiku-5-5)`".into())
        );
        assert_eq!(
            read(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#
            ),
            Msg::UserText("[Request interrupted by user]".into())
        );
    }

    #[test]
    fn a_replayed_prompt_is_a_prompt_and_a_subagents_line_names_its_call() {
        let replay = r#"{"type":"user","message":{"role":"user","content":"Reply with ok."},"isReplay":true,"parent_tool_use_id":null}"#;
        assert_eq!(read(replay), Msg::Prompt("Reply with ok.".into()));
        assert_eq!(parent_tool_use_id(&parse(replay).unwrap()), None);
        let inner = parse(r#"{"type":"assistant","message":{"id":"m","content":[]},"parent_tool_use_id":"toolu_9"}"#).unwrap();
        assert_eq!(parent_tool_use_id(&inner), Some("toolu_9"));
    }

    #[test]
    fn a_permission_a_question_and_any_other_request() {
        let Msg::Permission {
            request_id,
            tool,
            input,
            description,
            suggestions,
        } = read(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"touch spike-file && ls"},"description":"Create spike-file","permission_suggestions":[]}}"#,
        )
        else {
            panic!("a can_use_tool for Bash is a Permission")
        };
        assert_eq!((request_id.as_str(), tool.as_str()), ("r1", "Bash"));
        assert_eq!(description.as_deref(), Some("Create spike-file"));
        assert!(suggestions.is_empty());
        assert_eq!(
            input.get("command").and_then(Value::as_str),
            Some("touch spike-file && ls")
        );

        let Msg::Question {
            request_id,
            questions,
            ..
        } = read(
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Do you prefer red or blue?","header":"Color","options":[{"label":"Red","description":"Choose red"},{"label":"Blue","description":"Choose blue"}],"multiSelect":false}]}}}"#,
        )
        else {
            panic!("AskUserQuestion is a Question")
        };
        assert_eq!(request_id, "r2");
        assert_eq!(
            questions,
            vec![Question {
                question: "Do you prefer red or blue?".into(),
                header: "Color".into(),
                multi_select: false,
                options: vec![
                    ("Red".into(), "Choose red".into()),
                    ("Blue".into(), "Choose blue".into())
                ],
                previews: vec![String::new(), String::new()],
            }]
        );

        assert_eq!(
            read(
                r#"{"type":"control_request","request_id":"r3","request":{"subtype":"hook_callback","callback_id":"x"}}"#
            ),
            Msg::OtherRequest {
                request_id: "r3".into(),
                subtype: "hook_callback".into()
            }
        );
        assert_eq!(
            read(r#"{"type":"control_cancel_request","request_id":"r1"}"#),
            Msg::Cancelled {
                request_id: "r1".into()
            }
        );
    }

    #[test]
    fn responses_and_results() {
        let Msg::Response {
            request_id,
            ok,
            body,
        } = read(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"c2","response":{"model":"claude-sonnet-5-5"}}}"#,
        )
        else {
            panic!("response")
        };
        assert!(ok);
        assert_eq!(request_id, "c2");
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some("claude-sonnet-5-5")
        );
        let Msg::Response { ok, body, .. } = read(
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"c9","error":"no such model"}}"#,
        ) else {
            panic!("response")
        };
        assert!(!ok);
        assert_eq!(body.as_str(), Some("no such model"));
        assert_eq!(
            read(
                r#"{"type":"result","subtype":"error_during_execution","is_error":false,"session_id":"s1"}"#
            ),
            Msg::Result {
                subtype: "error_during_execution".into(),
                is_error: false,
                session_id: "s1".into()
            }
        );
        assert_eq!(
            read(r#"{"type":"result","subtype":"error_max_turns","session_id":"s1"}"#),
            Msg::Result {
                subtype: "error_max_turns".into(),
                is_error: true,
                session_id: "s1".into()
            }
        );
    }

    #[test]
    fn what_is_sent_is_what_the_spike_sent() {
        // Each line as 2.1.311 accepted it on 2026-11-01; keys sort, which claude ignores.
        assert_eq!(
            to_line(&initialize("c1")),
            r#"{"request":{"subtype":"initialize"},"request_id":"c1","type":"control_request"}"#
        );
        assert_eq!(
            to_line(&user_text("/compact")),
            r#"{"message":{"content":"/compact","role":"user"},"type":"user"}"#
        );
        assert_eq!(
            to_line(&set_model("c2", "sonnet")),
            r#"{"request":{"model":"sonnet","subtype":"set_model"},"request_id":"c2","type":"control_request"}"#
        );
        assert_eq!(
            to_line(&set_effort("c4", Some("low"))),
            r#"{"request":{"settings":{"effortLevel":"low"},"subtype":"apply_flag_settings"},"request_id":"c4","type":"control_request"}"#
        );
        assert_eq!(
            to_line(&set_effort("c5", None)),
            r#"{"request":{"settings":{"effortLevel":null},"subtype":"apply_flag_settings"},"request_id":"c5","type":"control_request"}"#
        );
        assert_eq!(
            to_line(&rename("c6", "spike rename", "s1")),
            r#"{"request":{"session_id":"s1","source":"host","subtype":"rename_session","title":"spike rename"},"request_id":"c6","type":"control_request"}"#
        );
        assert_eq!(
            to_line(&interrupt("c8")),
            r#"{"request":{"subtype":"interrupt"},"request_id":"c8","type":"control_request"}"#
        );
        assert_eq!(
            to_line(&end_session("c9")),
            r#"{"request":{"subtype":"end_session"},"request_id":"c9","type":"control_request"}"#
        );
    }

    #[test]
    fn answers_carry_the_request_id_and_the_input() {
        let input = parse(r#"{"command":"touch x"}"#).unwrap();
        assert_eq!(
            to_line(&allow("r1", input.clone())),
            r#"{"response":{"request_id":"r1","response":{"behavior":"allow","updatedInput":{"command":"touch x"}},"subtype":"success"},"type":"control_response"}"#
        );
        assert_eq!(
            to_line(&deny("r1", "no", false)),
            r#"{"response":{"request_id":"r1","response":{"behavior":"deny","message":"no"},"subtype":"success"},"type":"control_response"}"#
        );
        assert_eq!(
            to_line(&deny("r1", "no", true)),
            r#"{"response":{"request_id":"r1","response":{"behavior":"deny","interrupt":true,"message":"no"},"subtype":"success"},"type":"control_response"}"#
        );
        let q = parse(r#"{"questions":[{"question":"Red or blue?"}]}"#).unwrap();
        let line = to_line(&answer_questions(
            "r2",
            &q,
            &[("Red or blue?".into(), "Red".into())],
        ));
        assert_eq!(
            line,
            r#"{"response":{"request_id":"r2","response":{"behavior":"allow","updatedInput":{"answers":{"Red or blue?":"Red"},"questions":[{"question":"Red or blue?"}]}},"subtype":"success"},"type":"control_response"}"#
        );
        assert_eq!(
            to_line(&refuse("r3", "not served")),
            r#"{"response":{"error":"not served","request_id":"r3","subtype":"error"},"type":"control_response"}"#
        );
    }
}
