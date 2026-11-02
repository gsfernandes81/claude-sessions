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
    },
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
    /// Known and of no use to the view: hook progress, thinking-token estimates, rate-limit
    /// readings, keep-alives and the stream events other than text.
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
    /// `system/task_started`, `task_progress` and `task_notification` for a subagent or a
    /// workflow, said under the tool call that started it (`tool_use_id`).
    Task { tool_use_id: String, event: Task },
    /// A line of a type this version does not know, kept by its name so a log can say so.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Task {
    /// `task_type` is `local_agent`, `local_workflow`, `local_bash`, …
    Started {
        task_type: String,
        description: String,
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

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowAgent {
    pub label: String,
    /// 1-based, into the phases.
    pub phase: u64,
    /// `queued`, `running`, `done`, `failed`, …
    pub state: String,
    pub last_tool: String,
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

fn task(v: &Value, sub: &str) -> Msg {
    let tool_use_id = s(v, "tool_use_id");
    if tool_use_id.is_empty() {
        return Msg::Noise;
    }
    let event = match sub {
        "task_started" => Task::Started {
            task_type: s(v, "task_type"),
            description: s(v, "description"),
        },
        "task_progress" => {
            let mut phases = Vec::new();
            let mut agents = Vec::new();
            if let Some(Value::Arr(items)) = v.get("workflow_progress") {
                for it in items {
                    match it.get("type").and_then(Value::as_str) {
                        Some("workflow_phase") => phases.push(s(it, "title")),
                        Some("workflow_agent") => agents.push(WorkflowAgent {
                            label: s(it, "label"),
                            phase: it.get("phaseIndex").and_then(Value::as_u64).unwrap_or(0),
                            state: s(it, "state"),
                            last_tool: s(it, "lastToolName"),
                        }),
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
    Msg::Task { tool_use_id, event }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Thinking, redacted thinking, images: drawn as their kind, not their content.
    Other(String),
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
            "task_started" | "task_progress" | "task_notification" => task(v, sub),
            "hook_started"
            | "hook_response"
            | "hook_progress"
            | "thinking_tokens"
            | "task_updated"
            | "background_tasks_changed" => Msg::Noise,
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
            let blocks = msg
                .and_then(|m| m.get("content"))
                .and_then(Value::as_arr)
                .unwrap_or(&[])
                .iter()
                .map(block)
                .collect();
            Msg::Assistant {
                message_id: msg.map(|m| s(m, "id")).unwrap_or_default(),
                blocks,
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
        "rate_limit_event" | "keep_alive" | "tool_progress" | "prompt_suggestion" => Msg::Noise,
        other => Msg::Unknown(other.to_string()),
    }
}

/// The tool call a line belongs to, when it comes from inside one: a subagent's messages carry
/// the `Task` call's id as `parent_tool_use_id`. `None` for the conversation's own lines.
pub fn parent_tool_use_id(v: &Value) -> Option<&str> {
    v.get("parent_tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
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

const LOCAL_OPEN: &str = "<local-command-stdout>";
const LOCAL_CLOSE: &str = "</local-command-stdout>";

fn user(v: &Value) -> Msg {
    let content = v.get("message").and_then(|m| m.get("content"));
    match content {
        Some(Value::Str(t)) => match t.strip_prefix(LOCAL_OPEN) {
            Some(rest) => {
                Msg::LocalOutput(rest.strip_suffix(LOCAL_CLOSE).unwrap_or(rest).to_string())
            }
            None if v.get("isReplay").and_then(Value::as_bool) == Some(true) => {
                Msg::Prompt(t.clone())
            }
            None => Msg::UserText(t.clone()),
        },
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
                Msg::UserText(result_text(content))
            } else {
                Msg::ToolResults(results)
            }
        }
        _ => Msg::UserText(String::new()),
    }
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
    control(
        request_id,
        subtype(
            "rename_session",
            vec![
                ("title", Value::string(title)),
                ("source", Value::string("host")),
                ("session_id", Value::string(session_id)),
            ],
        ),
    )
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

/// Deny a tool call; `message` is what claude is told.
pub fn deny(request_id: &str, message: &str) -> Value {
    answer(
        request_id,
        obj(vec![
            ("behavior", Value::string("deny")),
            ("message", Value::string(message)),
        ]),
    )
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
                deny("r3", "no"),
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
        let Msg::Assistant { message_id, blocks } = read(
            r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo spike-ok"}},{"type":"thinking","thinking":"x"},{"type":"text","text":"done"}]}}"#,
        ) else {
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
            to_line(&deny("r1", "no")),
            r#"{"response":{"request_id":"r1","response":{"behavior":"deny","message":"no"},"subtype":"success"},"type":"control_response"}"#
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
