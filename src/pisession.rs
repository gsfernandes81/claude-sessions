//! pi's sessions on disk: where they are, and what one says (`docs/design.md` § *Harnesses*).
//!
//! Read from the 1.4.0 package on 2026-10-30 (`dist/core/session-manager.js`, `docs/
//! session-format.md`), since pi is not installed in this container:
//!
//! - **Where.** `<agent dir>/sessions/--<cwd>--/<timestamp>_<id>.jsonl`, the agent dir
//!   `$PI_CODING_AGENT_DIR` else `~/.pi/agent` (infra sets it to `~/.local/share/pi`), the
//!   directory named for the cwd with its leading `/` dropped and every `/`, `\` or `:`
//!   made `-`, and the timestamp the header's with `:` and `.` made `-`.
//!   `$PI_CODING_AGENT_SESSION_DIR` replaces the whole of that with one flat directory for
//!   every cwd. **The file exists only once a user or assistant message is in it**: a pi
//!   opened and quit leaves nothing, so a slot's conversation path cannot be known at
//!   launch and is found by its id ([`conversation_path`]).
//! - **What.** A header line `{"type":"session","version":3,"id","timestamp","cwd"}`, then
//!   one entry a line, each with `type`, `id`, `parentId` and an ISO `timestamp`. A
//!   `message` entry's `message.role` is `user`, `assistant`, `toolResult` or `system`; a
//!   user message's `content` is a string, or a list of parts with `type` and `text`. The
//!   session's name — `/name`, `--name` — is the latest `session_info` entry's `name`, and
//!   an empty one clears it. pi generates no title, so the first user message is the title
//!   of last resort, as it is in pi's own `/resume` picker.
//!
//! The same questions `store.rs` and `transcript.rs` answer for Claude Code — is there an
//! exchange to resume, what is it called, when was it last active, which of them started in
//! the workspace — answered for pi, in pi's shapes, and nothing else.

use crate::clock::Millis;
use crate::harness::Harness;
use crate::json::{self, Value};
use crate::store::{self, Conversation};
use crate::transcript::Titles;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How much of a session's start is read for its header and first prompt.
const HEAD: u64 = 1024 * 1024;
/// How much of its end is read for its name and last timestamp.
const TAIL: u64 = 256 * 1024;

/// The directory name pi keeps a session started in `cwd` under.
pub fn dir_name(cwd: &str) -> String {
    let body: String = cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(cwd)
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{body}--")
}

/// Where pi keeps sessions, and whether in one flat directory: `$PI_CODING_AGENT_SESSION_DIR`
/// when it names one, else `sessions` in pi's agent directory — `$PI_CODING_AGENT_DIR`, else
/// `~/.pi/agent` — with a directory per cwd.
fn layout() -> (PathBuf, bool) {
    layout_from(|k| std::env::var_os(k))
}

/// [`layout`] from `env`; a variable set but empty is unset, as pi reads it.
fn layout_from(env: impl Fn(&str) -> Option<OsString>) -> (PathBuf, bool) {
    let var = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(d) = var("PI_CODING_AGENT_SESSION_DIR") {
        return (PathBuf::from(d), true);
    }
    let agent = var("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = var("HOME").unwrap_or_else(|| "/tmp".into());
            Path::new(&home).join(".pi/agent")
        });
    (agent.join("sessions"), false)
}

/// The directory the sessions of `cwd` are in, given the layout: the flat directory itself,
/// else `cwd`'s own under `root`.
fn project_dir((root, flat): &(PathBuf, bool), cwd: &str) -> PathBuf {
    if *flat {
        root.clone()
    } else {
        root.join(dir_name(cwd))
    }
}

/// The session file of `id` among `cwd`'s, if it has been written yet: the one named
/// `<timestamp>_<id>.jsonl`, as pi names it — and, failing that, the one whose header says
/// `id`, which is how pi itself looks (`SessionManager.findById`). `None` before the first
/// message, or when the directory cannot be read.
pub fn conversation_path(cwd: &str, id: &str) -> Option<PathBuf> {
    conversation_path_in(&layout(), cwd, id)
}

fn conversation_path_in(layout: &(PathBuf, bool), cwd: &str, id: &str) -> Option<PathBuf> {
    let dir = project_dir(layout, cwd);
    let suffix = format!("_{id}.jsonl");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
        .collect();
    if let Some(p) = files.iter().find(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
    }) {
        return Some(p.clone());
    }
    files
        .into_iter()
        .find(|p| header(p).is_some_and(|h| h.id == id))
}

/// Whether the session at `path` holds a conversation to resume: a user message or a reply.
/// pi writes the file only once it has one, so an existing file nearly always does; read
/// anyway, since "nearly" is how a resume ends up on an empty session. A file too long to
/// settle within [`HEAD`] is taken as a conversation, as `transcript::has_exchange` does.
pub fn has_exchange(path: &Path) -> bool {
    let Ok(f) = File::open(path) else {
        return false;
    };
    let mut lines = BufReader::new(f.take(HEAD));
    let mut line = Vec::new();
    let mut read = 0u64;
    loop {
        line.clear();
        match lines.read_until(b'\n', &mut line) {
            Ok(0) => return read >= HEAD,
            Ok(n) => read += n as u64,
            Err(_) => return true,
        }
        if matches!(
            message_role(&String::from_utf8_lossy(&line)),
            Some("user" | "assistant")
        ) {
            return true;
        }
    }
}

/// The session's name, as the latest `session_info` in the last part of the file says —
/// in `Titles::custom`, since it is the owner's own name, as a custom title is in Claude
/// Code. pi generates no title, so `ai` is never set.
pub fn titles(path: &Path) -> Titles {
    titles_in(&tail(path))
}

/// [`titles`], read incrementally for the rows the agent sends every two seconds
/// (`transcript::titles_cached_by`).
pub fn titles_cached(path: &Path) -> Titles {
    crate::transcript::titles_cached_by(path, titles_in)
}

/// [`titles`], pure, over whole lines. An empty name is a cleared one, as pi reads it.
pub fn titles_in(text: &str) -> Titles {
    let mut out = Titles::default();
    for line in text.lines() {
        if !line.contains("\"session_info\"") {
            continue;
        }
        let Ok(v) = json::parse(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("session_info") {
            continue;
        }
        out.custom = v
            .get("name")
            .and_then(Value::as_str)
            .and_then(crate::events::one_line);
    }
    out
}

/// Every pi conversation started in `workspace`, with an exchange in it, newest first. Only
/// the `--<cwd>--` directories whose name begins with the workspace's are read — lossy, so the
/// header's `cwd` decides — and under `$PI_CODING_AGENT_SESSION_DIR` the one flat directory.
pub fn in_workspace(workspace: &str) -> Vec<Conversation> {
    static CACHE: store::Cached = store::Cached::new();
    in_workspace_in(&CACHE, &layout(), workspace)
}

fn in_workspace_in(
    cache: &store::Cached,
    layout: &(PathBuf, bool),
    workspace: &str,
) -> Vec<Conversation> {
    let ws = workspace.trim_end_matches('/');
    let dirs: Vec<PathBuf> = if layout.1 {
        vec![project_dir(layout, ws)]
    } else {
        let prefix = dir_name(ws).trim_end_matches("--").to_string();
        std::fs::read_dir(&layout.0)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut files = Vec::new();
    for d in dirs {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "jsonl") && p.is_file() {
                files.push(p);
            }
        }
    }
    store::newest_per_id(
        cache
            .read_all(files, read)
            .into_iter()
            .filter(|c| store::within(&c.cwd, workspace))
            .collect(),
    )
}

/// One session, or `None` when it has no header to say where it ran, or no exchange.
pub fn read(path: &Path) -> Option<Conversation> {
    let (h, first_prompt, exchange) = head(path)?;
    if !exchange {
        return None;
    }
    let tail = tail(path);
    let last_ms = tail
        .lines()
        .rev()
        .find_map(timestamp_of)
        .or_else(|| store::iso_ms(&h.timestamp))
        .unwrap_or(0);
    let title = titles_in(&tail)
        .custom
        .or(first_prompt)
        .unwrap_or_else(|| "(no title yet)".to_string());
    Some(Conversation {
        id: h.id,
        harness: Harness::Pi,
        cwd: h.cwd,
        path: path.to_path_buf(),
        title,
        last_ms,
    })
}

/// The header line's fields.
struct Header {
    id: String,
    cwd: String,
    timestamp: String,
}

/// The first line, when it is a session header.
fn header(path: &Path) -> Option<Header> {
    let f = File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(f.take(64 * 1024))
        .read_line(&mut first)
        .ok()?;
    header_of(&first)
}

fn header_of(line: &str) -> Option<Header> {
    let v = json::parse(line.trim_end()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    Some(Header {
        id: v.get("id")?.as_str()?.to_string(),
        cwd: v.get("cwd")?.as_str()?.to_string(),
        timestamp: v
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// From the start of the file: the header, the first user message as a line, and whether
/// there is an exchange at all. Stops as soon as all three are settled.
fn head(path: &Path) -> Option<(Header, Option<String>, bool)> {
    let f = File::open(path).ok()?;
    let mut lines = BufReader::new(f.take(HEAD));
    let mut buf = Vec::new();
    let mut h = None;
    let (mut prompt, mut exchange) = (None, false);
    loop {
        buf.clear();
        match lines.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        if h.is_none() {
            // The first line is the header, or this is no session.
            h = Some(header_of(&line)?);
            continue;
        }
        match message_role(&line) {
            Some("user") => {
                exchange = true;
                if prompt.is_none() {
                    prompt = json::parse(line.trim_end())
                        .ok()
                        .and_then(|v| user_text(&v))
                        .and_then(|t| crate::events::one_line(&t));
                }
            }
            Some("assistant") => exchange = true,
            _ => {}
        }
        if exchange && prompt.is_some() {
            break;
        }
    }
    Some((h?, prompt, exchange))
}

/// A `message` entry's role, `user`, `assistant` or `other`, cheaply: the line is parsed only when it may be one.
fn message_role(line: &str) -> Option<&'static str> {
    if !line.contains("\"message\"") {
        return None;
    }
    let v = json::parse(line.trim_end()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    // Only these two make an exchange; `toolResult` and `system` are read alike.
    match v.get("message")?.get("role")?.as_str()? {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        _ => Some("other"),
    }
}

/// What the owner typed, from a user message's `content`: the string, or its text parts
/// joined.
fn user_text(entry: &Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let Value::Arr(parts) = content else {
        return None;
    };
    let texts: Vec<&str> = parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect();
    (!texts.is_empty()).then(|| texts.join(" "))
}

fn tail(path: &Path) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).is_err() {
        return String::new();
    }
    let mut raw = Vec::new();
    let _ = f.take(TAIL).read_to_end(&mut raw);
    String::from_utf8_lossy(&raw).into_owned()
}

/// An entry's own `timestamp`, in milliseconds.
fn timestamp_of(line: &str) -> Option<Millis> {
    if !line.contains("\"timestamp\"") {
        return None;
    }
    let v = json::parse(line).ok()?;
    store::iso_ms(v.get("timestamp")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session as 1.4.0 writes one (docs/session-format.md), trimmed to the fields read:
    /// the header, the system message pi puts first, a prompt, a reply, a tool result, a
    /// name.
    const HEADER: &str = r#"{"type":"session","version":3,"id":"a1b2c3d4-0000-4000-8000-000000000001","timestamp":"2026-10-30T10:00:00.000Z","cwd":"/workspace"}"#;
    const SYSTEM: &str = r#"{"type":"message","id":"s1","parentId":null,"timestamp":"2026-10-30T10:00:00.100Z","message":{"role":"system","content":"","sections":{"preamble":"You are"},"timestamp":1}}"#;
    const PROMPT: &str = r#"{"type":"message","id":"u1","parentId":"s1","timestamp":"2026-10-30T10:00:01.000Z","message":{"role":"user","content":"fix the   dns\nrecords","timestamp":2}}"#;
    const REPLY: &str = r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-30T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."}],"provider":"openrouter","model":"qwen","timestamp":3}}"#;
    const TOOL: &str = r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-10-30T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"c","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":4}}"#;
    const NAMED: &str = r#"{"type":"session_info","id":"n1","parentId":"t1","timestamp":"2026-10-30T10:05:00.000Z","name":"dns records"}"#;
    const UNNAMED: &str = r#"{"type":"session_info","id":"n2","parentId":"n1","timestamp":"2026-10-30T10:06:00.000Z","name":""}"#;

    fn lines(ls: &[&str]) -> String {
        ls.join("\n") + "\n"
    }

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("cs-pi-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
        fn write(&self, sub: &str, name: &str, body: &str) -> PathBuf {
            let dir = self.0.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_directory_is_named_as_pi_names_it() {
        assert_eq!(dir_name("/workspace"), "--workspace--");
        assert_eq!(dir_name("/home/dev/x:y"), "--home-dev-x-y--");
        // A windows path, for the record: each of `:` and `\` becomes a `-`, as pi does it.
        assert_eq!(dir_name("C:\\w"), "--C--w--");
    }

    #[test]
    fn a_session_reads_its_id_directory_title_and_last_entry() {
        let d = Dir::new("read");
        let p = d.write(
            "--workspace--",
            "2026-10-30T10-00-00-000Z_a1b2c3d4-0000-4000-8000-000000000001.jsonl",
            &lines(&[HEADER, SYSTEM, PROMPT, REPLY, TOOL]),
        );
        let c = read(&p).expect("a conversation");
        assert_eq!(c.id, "a1b2c3d4-0000-4000-8000-000000000001");
        assert_eq!(c.harness, Harness::Pi);
        assert_eq!(c.cwd, "/workspace");
        assert_eq!(
            c.title, "fix the dns records",
            "the first prompt, on one line"
        );
        assert_eq!(
            c.last_ms,
            store::iso_ms("2026-10-30T10:00:03.000Z").unwrap()
        );
        // Named: the name stands over the prompt; cleared: the prompt again.
        let p = d.write(
            "--workspace--",
            "named.jsonl",
            &lines(&[HEADER, PROMPT, NAMED]),
        );
        assert_eq!(read(&p).unwrap().title, "dns records");
        assert_eq!(titles(&p).custom.as_deref(), Some("dns records"));
        assert_eq!(titles_cached(&p).custom.as_deref(), Some("dns records"));
        let p = d.write(
            "--workspace--",
            "cleared.jsonl",
            &lines(&[HEADER, PROMPT, NAMED, UNNAMED]),
        );
        assert_eq!(read(&p).unwrap().title, "fix the dns records");
        assert_eq!(titles(&p), Titles::default());
    }

    #[test]
    fn a_session_with_no_exchange_or_no_header_is_not_a_conversation() {
        let d = Dir::new("empty");
        let p = d.write("--workspace--", "setup.jsonl", &lines(&[HEADER, SYSTEM]));
        assert!(read(&p).is_none());
        assert!(!has_exchange(&p));
        let p = d.write("--workspace--", "tool.jsonl", &lines(&[HEADER, TOOL]));
        assert!(!has_exchange(&p), "a tool result alone is no exchange");
        let p = d.write("--workspace--", "headless.jsonl", &lines(&[PROMPT, REPLY]));
        assert!(read(&p).is_none(), "nothing says where it ran");
        assert!(has_exchange(&p), "but it is an exchange");
        assert!(!has_exchange(&d.0.join("none.jsonl")), "no file");
        // Calibration: the real shape is both.
        let p = d.write(
            "--workspace--",
            "real.jsonl",
            &lines(&[HEADER, SYSTEM, PROMPT]),
        );
        assert!(has_exchange(&p));
        assert!(read(&p).is_some());
    }

    #[test]
    fn a_prompt_in_parts_is_its_text() {
        let parts = r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-10-30T10:00:01.000Z","message":{"role":"user","content":[{"type":"image","data":"…"},{"type":"text","text":"what is"},{"type":"text","text":"this"}]}}"#;
        let d = Dir::new("parts");
        let p = d.write("--workspace--", "p.jsonl", &lines(&[HEADER, parts]));
        assert_eq!(read(&p).unwrap().title, "what is this");
    }

    #[test]
    fn the_conversation_of_an_id_is_found_by_its_file_name_or_its_header() {
        let d = Dir::new("find");
        let sessions = d.0.join("sessions");
        let layout = (sessions.clone(), false);
        let id = "a1b2c3d4-0000-4000-8000-000000000001";
        let find = |cwd: &str| conversation_path_in(&layout, cwd, id);
        assert_eq!(find("/workspace"), None, "nothing written yet");
        std::fs::create_dir_all(sessions.join("--workspace--")).unwrap();
        assert_eq!(find("/workspace"), None, "directory, no file");
        let named = sessions
            .join("--workspace--")
            .join(format!("2026-10-30T10-00-00-000Z_{id}.jsonl"));
        std::fs::write(&named, lines(&[HEADER, PROMPT])).unwrap();
        assert_eq!(find("/workspace"), Some(named.clone()));
        assert_eq!(find("/elsewhere"), None, "another cwd's directory");
        // Renamed by hand: still found, by the header pi itself goes by.
        let moved = named.with_file_name("kept.jsonl");
        std::fs::rename(&named, &moved).unwrap();
        assert_eq!(find("/workspace"), Some(moved.clone()));
        // The one flat directory `$PI_CODING_AGENT_SESSION_DIR` makes: every cwd's in it.
        let flat = (sessions.join("--workspace--"), true);
        assert_eq!(conversation_path_in(&flat, "/elsewhere", id), Some(moved));
    }

    #[test]
    fn only_the_workspaces_sessions_are_listed_newest_first() {
        let d = Dir::new("list");
        let cache = store::Cached::new();
        let layout = (d.0.join("sessions"), false);
        let other_header = HEADER.replace("/workspace", "/workspace-old");
        let sub_header = HEADER
            .replace("/workspace", "/workspace/sub")
            .replace("000000000001", "000000000002");
        let later = PROMPT.replace("10:00:01.000Z", "11:00:00.000Z");
        d.write(
            "sessions/--workspace--",
            "a.jsonl",
            &lines(&[HEADER, PROMPT]),
        );
        d.write(
            "sessions/--workspace-sub--",
            "b.jsonl",
            &lines(&[&sub_header, &later]),
        );
        d.write(
            "sessions/--workspace-old--",
            "c.jsonl",
            &lines(&[&other_header, PROMPT]),
        );
        d.write(
            "sessions/--workspace--",
            "setup.jsonl",
            &lines(&[HEADER, SYSTEM]),
        );
        d.write("sessions/--workspace--", "notes.txt", "not a session");
        let listed = |ws: &str| -> Vec<String> {
            in_workspace_in(&cache, &layout, ws)
                .into_iter()
                .map(|c| c.id)
                .collect()
        };
        assert_eq!(
            listed("/workspace"),
            [
                "a1b2c3d4-0000-4000-8000-000000000002",
                "a1b2c3d4-0000-4000-8000-000000000001"
            ]
        );
        assert!(listed("/nowhere").is_empty());
        // Read again from the cache, the same; a file that changed is read afresh.
        assert_eq!(listed("/workspace").len(), 2);
        d.write(
            "sessions/--workspace--",
            "a.jsonl",
            &lines(&[HEADER, PROMPT, NAMED]),
        );
        let named = in_workspace_in(&cache, &layout, "/workspace")
            .into_iter()
            .find(|c| c.id.ends_with('1'))
            .unwrap();
        assert_eq!(named.title, "dns records");
    }

    #[test]
    fn a_long_system_prompt_or_long_tool_output_hides_neither_the_prompt_nor_the_name() {
        // pi's first entry carries its whole system prompt, many kilobytes; a turn's tool
        // output after a name can be as long. Both are well within what is read.
        let big = "x".repeat(16 * 1024);
        let system = SYSTEM.replace("You are", &big);
        let tool = TOOL.replace("\"ok\"", &format!("\"{big}\""));
        let d = Dir::new("long");
        let p = d.write(
            "--workspace--",
            "long.jsonl",
            &lines(&[HEADER, &system, PROMPT, REPLY]),
        );
        assert_eq!(read(&p).unwrap().title, "fix the dns records");
        let p = d.write(
            "--workspace--",
            "named.jsonl",
            &lines(&[HEADER, PROMPT, NAMED, &tool]),
        );
        assert_eq!(titles(&p).custom.as_deref(), Some("dns records"));
        assert_eq!(read(&p).unwrap().title, "dns records");
        // A header with a field this version does not know, however long, still says the id.
        let id = "a1b2c3d4-0000-4000-8000-000000000001";
        let forked = HEADER.replace("\"cwd\"", &format!("\"parentSession\":\"/{big}\",\"cwd\""));
        d.write("--workspace--", "forked.jsonl", &lines(&[&forked, PROMPT]));
        let layout = (d.0.clone(), false);
        std::fs::remove_file(d.0.join("--workspace--/long.jsonl")).unwrap();
        std::fs::remove_file(d.0.join("--workspace--/named.jsonl")).unwrap();
        assert_eq!(
            conversation_path_in(&layout, "/workspace", id),
            Some(d.0.join("--workspace--/forked.jsonl"))
        );
    }

    #[test]
    fn a_reply_alone_is_an_exchange_and_the_first_prompt_with_words_is_the_title() {
        let d = Dir::new("order");
        let p = d.write("--workspace--", "reply.jsonl", &lines(&[HEADER, REPLY]));
        assert!(has_exchange(&p));
        assert_eq!(read(&p).unwrap().title, "(no title yet)");
        // A reply before the prompt, and a pasted image before the words: the words title it.
        let image = r#"{"type":"message","id":"u0","parentId":null,"timestamp":"2026-10-30T10:00:00.500Z","message":{"role":"user","content":[{"type":"image","data":"…"}]}}"#;
        for (name, body) in [
            ("late.jsonl", lines(&[HEADER, REPLY, PROMPT])),
            ("image.jsonl", lines(&[HEADER, image, PROMPT])),
        ] {
            let p = d.write("--workspace--", name, &body);
            assert_eq!(read(&p).unwrap().title, "fix the dns records", "{name}");
        }
    }

    #[test]
    fn a_session_too_long_to_settle_is_taken_as_a_conversation() {
        // Nothing but a header and system entries for longer than is read: it may hold a
        // conversation further on, and a resume that finds none is cheap.
        let d = Dir::new("huge");
        let system = SYSTEM.replace("You are", &"x".repeat(64 * 1024));
        let mut body = vec![HEADER];
        body.extend(std::iter::repeat_n(system.as_str(), 20));
        let p = d.write("--workspace--", "huge.jsonl", &lines(&body));
        assert!(std::fs::metadata(&p).unwrap().len() > HEAD);
        assert!(has_exchange(&p));
        // Calibration: the same, short, is no exchange.
        let p = d.write("--workspace--", "short.jsonl", &lines(&body[..3]));
        assert!(!has_exchange(&p));
    }

    #[test]
    fn only_jsonl_files_are_sessions() {
        // A copy kept beside a session, with its header and messages, is not one.
        let d = Dir::new("bak");
        let id = "a1b2c3d4-0000-4000-8000-000000000001";
        d.write("--workspace--", "kept.jsonl.bak", &lines(&[HEADER, PROMPT]));
        let layout = (d.0.clone(), false);
        assert_eq!(conversation_path_in(&layout, "/workspace", id), None);
        let cache = store::Cached::new();
        assert!(in_workspace_in(&cache, &layout, "/workspace").is_empty());
        // Calibration: renamed to .jsonl, it is found and listed.
        std::fs::rename(
            d.0.join("--workspace--/kept.jsonl.bak"),
            d.0.join("--workspace--/kept.jsonl"),
        )
        .unwrap();
        assert!(conversation_path_in(&layout, "/workspace", id).is_some());
        assert_eq!(in_workspace_in(&cache, &layout, "/workspace").len(), 1);
    }

    #[test]
    fn sessions_are_where_pis_variables_say_else_under_home() {
        fn env<'a>(set: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
            move |k| {
                set.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| OsString::from(v))
            }
        }
        let home = [("HOME", "/home/dev")];
        assert_eq!(
            layout_from(env(&home)),
            (PathBuf::from("/home/dev/.pi/agent/sessions"), false)
        );
        let agent = [("HOME", "/home/dev"), ("PI_CODING_AGENT_DIR", "/d/pi")];
        assert_eq!(
            layout_from(env(&agent)),
            (PathBuf::from("/d/pi/sessions"), false)
        );
        let flat = [
            ("PI_CODING_AGENT_DIR", "/d/pi"),
            ("PI_CODING_AGENT_SESSION_DIR", "/s"),
        ];
        assert_eq!(layout_from(env(&flat)), (PathBuf::from("/s"), true));
        // Set but empty is unset.
        let empty = [
            ("HOME", "/home/dev"),
            ("PI_CODING_AGENT_DIR", ""),
            ("PI_CODING_AGENT_SESSION_DIR", ""),
        ];
        assert_eq!(layout_from(env(&empty)), layout_from(env(&home)));
        assert_eq!(
            layout_from(env(&[])),
            (PathBuf::from("/tmp/.pi/agent/sessions"), false)
        );
        // The live reading is the same function of this process's own environment.
        assert_eq!(layout(), layout_from(|k| std::env::var_os(k)));
    }
}
