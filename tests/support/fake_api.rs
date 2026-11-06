//! A stand-in for the Anthropic API, for driving a real `claude` without a model: it records
//! every request claude makes and answers each with what the test's script says — some text,
//! or one tool call. Claude Code is pointed at it with `ANTHROPIC_BASE_URL` and a made-up
//! `ANTHROPIC_API_KEY`, so nothing leaves the machine and no account is needed.
//!
//! **Scripted by what claude sent, never by how many times it asked**, so a Claude Code that
//! makes one more side request than the last one does not shift every answer. A script looks
//! at the request — its tools, its last message — and decides.
//!
//! Plain HTTP/1.1, one request a connection (`connection: close`). A streamed answer is the
//! Messages API's server-sent events; a request that does not ask for a stream gets one JSON
//! message; any other path gets a 404, which Claude Code treats as a feature not there.

use super::json::{self, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What the fake answers one request with.
#[derive(Debug, Clone)]
pub enum Reply {
    /// The assistant says this and ends its turn.
    Text(String),
    /// The assistant calls one tool with this input, and the turn waits on its result.
    Tool { name: String, input: Value },
}

/// One request claude made.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    pub body: Value,
}

impl Request {
    /// The names of the tools offered to the model.
    pub fn tools(&self) -> Vec<String> {
        self.body
            .get("tools")
            .and_then(Value::as_arr)
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    /// Every piece of text in the conversation sent, prompts and tool results alike, in order.
    pub fn texts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for m in self
            .body
            .get("messages")
            .and_then(Value::as_arr)
            .unwrap_or_default()
        {
            collect_text(m.get("content"), &mut out);
        }
        out
    }

    /// Whether any text in the conversation contains `needle`.
    pub fn mentions(&self, needle: &str) -> bool {
        self.texts().iter().any(|t| t.contains(needle))
    }

    /// Every tool result in the conversation, oldest first, by the name of the tool that was
    /// called. The whole conversation rather than its last message: Claude Code may put text
    /// of its own (a token count, a reminder) after a result.
    pub fn results(&self) -> Vec<(String, String)> {
        let messages = self
            .body
            .get("messages")
            .and_then(Value::as_arr)
            .unwrap_or_default();
        let name_of = |id: &str| -> String {
            for m in messages {
                for c in m.get("content").and_then(Value::as_arr).unwrap_or_default() {
                    if c.get("type").and_then(Value::as_str) == Some("tool_use")
                        && c.get("id").and_then(Value::as_str) == Some(id)
                    {
                        return c
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                    }
                }
            }
            String::new()
        };
        messages
            .iter()
            .flat_map(|m| m.get("content").and_then(Value::as_arr).unwrap_or_default())
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|c| {
                let id = c
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut text = Vec::new();
                collect_text(c.get("content"), &mut text);
                (name_of(id), text.join("\n"))
            })
            .collect()
    }
}

fn collect_text(content: Option<&Value>, out: &mut Vec<String>) {
    match content {
        Some(Value::Str(s)) => out.push(s.clone()),
        Some(Value::Arr(items)) => {
            for c in items {
                if let Some(t) = c.get("text").and_then(Value::as_str) {
                    out.push(t.to_string());
                }
                if c.get("type").and_then(Value::as_str) == Some("tool_result") {
                    collect_text(c.get("content"), out);
                }
            }
        }
        _ => {}
    }
}

pub type Script = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

struct Shared {
    seen: Mutex<Vec<Request>>,
    arrived: Condvar,
}

/// The fake, listening until it is dropped with the test.
pub struct FakeApi {
    pub url: String,
    shared: Arc<Shared>,
}

impl FakeApi {
    pub fn start(script: Script) -> FakeApi {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new(Shared {
            seen: Mutex::new(Vec::new()),
            arrived: Condvar::new(),
        });
        let s = shared.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let s = s.clone();
                let script = script.clone();
                std::thread::spawn(move || serve(conn, &s, &script));
            }
        });
        FakeApi { url, shared }
    }

    /// Every request to the Messages API so far.
    pub fn requests(&self) -> Vec<Request> {
        self.shared.seen.lock().unwrap().clone()
    }

    /// The first request `want` accepts, waiting up to `wait` for it; `None` if none came.
    pub fn wait_for(&self, wait: Duration, want: impl Fn(&Request) -> bool) -> Option<Request> {
        let until = Instant::now() + wait;
        let mut seen = self.shared.seen.lock().unwrap();
        loop {
            if let Some(r) = seen.iter().find(|r| want(r)) {
                return Some(r.clone());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            seen = self.shared.arrived.wait_timeout(seen, left).unwrap().0;
        }
    }
}

fn serve(conn: TcpStream, shared: &Shared, script: &Script) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let mut r = BufReader::new(&conn);
    let mut line = String::new();
    if r.read_line(&mut line).is_err() {
        return;
    }
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let mut length = 0usize;
    let mut chunked = false;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        let (k, v) = h.split_once(':').unwrap_or_default();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = v.trim().parse().unwrap_or(0),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            if r.read_line(&mut size).unwrap_or(0) == 0 {
                break;
            }
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut chunk = vec![0; n + 2];
            if r.read_exact(&mut chunk).is_err() {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else {
        body.resize(length, 0);
        let _ = r.read_exact(&mut body);
    }
    let mut out = &conn;
    let base = path.split('?').next().unwrap_or_default();
    if base == "/v1/messages/count_tokens" {
        let _ = respond(&mut out, "application/json", r#"{"input_tokens":10}"#);
        return;
    }
    if base != "/v1/messages" {
        let _ = out
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        return;
    }
    let body = json::parse(&String::from_utf8_lossy(&body)).unwrap_or_else(|_| Value::obj());
    let req = Request { path, body };
    let reply = script(&req);
    let stream = req.body.get("stream").and_then(Value::as_bool) == Some(true);
    let model = req
        .body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude-test")
        .to_string();
    {
        let mut seen = shared.seen.lock().unwrap();
        seen.push(req);
        shared.arrived.notify_all();
    }
    let _ = if stream {
        stream_reply(&mut out, &model, &reply)
    } else {
        respond(
            &mut out,
            "application/json",
            &json::to_line(&whole(&model, &reply)),
        )
    };
}

fn respond(out: &mut impl Write, ty: &str, body: &str) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 200 OK\r\ncontent-type: {ty}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut o = Value::obj();
    for (k, v) in pairs {
        o.set(k, v);
    }
    o
}

fn s(v: &str) -> Value {
    Value::string(v)
}

fn usage() -> Value {
    obj(vec![
        ("input_tokens", Value::num(1)),
        ("output_tokens", Value::num(1)),
    ])
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn tool_id() -> String {
    format!(
        "toolu_fake{:06}",
        NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// `reply` as one message, for a request that did not ask for a stream.
fn whole(model: &str, reply: &Reply) -> Value {
    let (content, stop) = match reply {
        Reply::Text(t) => (obj(vec![("type", s("text")), ("text", s(t))]), "end_turn"),
        Reply::Tool { name, input } => (
            obj(vec![
                ("type", s("tool_use")),
                ("id", s(&tool_id())),
                ("name", s(name)),
                ("input", input.clone()),
            ]),
            "tool_use",
        ),
    };
    obj(vec![
        ("id", s("msg_fake")),
        ("type", s("message")),
        ("role", s("assistant")),
        ("model", s(model)),
        ("content", Value::Arr(vec![content])),
        ("stop_reason", s(stop)),
        ("stop_sequence", Value::Null),
        ("usage", usage()),
    ])
}

/// `reply` as the Messages API streams it.
fn stream_reply(out: &mut impl Write, model: &str, reply: &Reply) -> std::io::Result<()> {
    out.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    let mut event = |name: &str, data: Value| -> std::io::Result<()> {
        write!(out, "event: {name}\ndata: {}\n\n", json::to_line(&data))
    };
    let mut start = whole(model, &Reply::Text(String::new()));
    start.set("content", Value::Arr(Vec::new()));
    start.set("stop_reason", Value::Null);
    event(
        "message_start",
        obj(vec![("type", s("message_start")), ("message", start)]),
    )?;
    let (block, delta, stop) = match reply {
        Reply::Text(t) => (
            obj(vec![("type", s("text")), ("text", s(""))]),
            obj(vec![("type", s("text_delta")), ("text", s(t))]),
            "end_turn",
        ),
        Reply::Tool { name, input } => (
            obj(vec![
                ("type", s("tool_use")),
                ("id", s(&tool_id())),
                ("name", s(name)),
                ("input", Value::obj()),
            ]),
            obj(vec![
                ("type", s("input_json_delta")),
                ("partial_json", s(&json::to_line(input))),
            ]),
            "tool_use",
        ),
    };
    let index = || Value::num(0);
    event(
        "content_block_start",
        obj(vec![
            ("type", s("content_block_start")),
            ("index", index()),
            ("content_block", block),
        ]),
    )?;
    event(
        "content_block_delta",
        obj(vec![
            ("type", s("content_block_delta")),
            ("index", index()),
            ("delta", delta),
        ]),
    )?;
    event(
        "content_block_stop",
        obj(vec![("type", s("content_block_stop")), ("index", index())]),
    )?;
    event(
        "message_delta",
        obj(vec![
            ("type", s("message_delta")),
            (
                "delta",
                obj(vec![
                    ("stop_reason", s(stop)),
                    ("stop_sequence", Value::Null),
                ]),
            ),
            ("usage", obj(vec![("output_tokens", Value::num(1))])),
        ]),
    )?;
    event("message_stop", obj(vec![("type", s("message_stop"))]))?;
    out.flush()
}
