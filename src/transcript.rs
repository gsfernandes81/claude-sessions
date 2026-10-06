//! The titles Claude Code gives a conversation, read from the end of its transcript.
//!
//! **A row's title is what Claude Code's own session selector shows** (owner, 2026-10-03): the
//! name the owner gave the conversation, else the short title Claude Code generates for it.
//! Both live in the transcript, `~/.claude/projects/<dir>/<session>.jsonl`, as entries it
//! appends and re-appends as the conversation goes on — read on 2026-10-03 from 2.1.287:
//!
//! ```text
//! {"type":"custom-title","customTitle":"claude-sessions","sessionId":"…"}
//! {"type":"ai-title","aiTitle":"Pick up the handoff","sessionId":"…"}
//! ```
//!
//! The last of each wins. Until 0.3.1 the menu preferred the `name` in Claude Code's live
//! sessions file instead, and on the boxes that showed a long reply of Claude's in place of a
//! title; that file is undocumented, and is no longer read for titles at all.
//!
//! Only the tail is read: transcripts run to megabytes, the hook that reads this runs on
//! `Stop` — a blocking hook — and the titles are re-appended often enough that the latest of
//! each is near the end.

use crate::json::{self, Value};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the end of a transcript is read.
const TAIL: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Titles {
    /// The owner's own name for the conversation (`/rename`, or `--name`).
    pub custom: Option<String>,
    /// Claude Code's generated title.
    pub ai: Option<String>,
}

/// The latest titles in the last part of the transcript at `path`. Anything unreadable is
/// simply no title: the row falls back to the first prompt.
pub fn titles(path: &Path) -> Titles {
    let Ok(mut f) = File::open(path) else {
        return Titles::default();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return Titles::default();
    }
    let mut raw = Vec::new();
    if f.take(TAIL).read_to_end(&mut raw).is_err() {
        return Titles::default();
    }
    let text = String::from_utf8_lossy(&raw);
    // Starting mid-file, the first line is a fragment of one entry; it fails to parse and is
    // skipped like any other line that is not a title.
    titles_in(&text)
}

/// How far into a transcript [`has_exchange`] looks before giving it the benefit of the doubt.
const HEAD: u64 = 4 * 1024 * 1024;

/// Whether the transcript at `path` holds a conversation worth resuming: a reply from Claude,
/// or a prompt the owner typed. **A transcript can exist with neither** (issue #5, as infra
/// read it on 2026-10-03): `/clear` writes the new conversation's file at once, holding only
/// its own bookkeeping — a mode line, a file-history snapshot, the `/clear` command's local
/// entries, a system line — so the file alone says nothing. A brand-new session writes no
/// file until its first prompt.
///
/// Read from the start, stopping at the first line that settles it: in a conversation that
/// is a line or two in. A file that cannot be opened is no conversation; one too long to
/// settle within [`HEAD`] is taken as one, because hiding a real conversation from the menu
/// is the worse mistake.
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
        if exchange_line(&String::from_utf8_lossy(&line)) {
            return true;
        }
    }
}

/// Whether `text`'s transcript lines hold a reply or a typed prompt. Pure; the rule
/// [`has_exchange`] applies line by line.
#[cfg_attr(not(test), allow(dead_code))]
pub fn exchange_in(text: &str) -> bool {
    text.lines().any(exchange_line)
}

/// One transcript line: any `assistant` entry is a reply; a `user` entry is a prompt when it
/// is text the owner typed — not marked meta (Claude Code's own injected context), and not a
/// local command's markup, which opens with `<` (`<command-name>/clear</command-name>`,
/// `<local-command-stdout>`). A tool result is a `user` entry too, but only ever after a
/// reply, so it never has to decide anything.
pub fn exchange_line(line: &str) -> bool {
    // Cheap filter first: most lines of a long transcript are neither.
    if !line.contains("\"assistant\"") && !line.contains("\"user\"") {
        return false;
    }
    let Ok(v) = json::parse(line.trim_end()) else {
        return false;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => true,
        Some("user") => {
            let meta = matches!(v.get("isMeta"), Some(Value::Bool(true)));
            let typed = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .is_some_and(|c| {
                    let c = c.trim_start();
                    !c.is_empty() && !c.starts_with('<')
                });
            !meta && typed
        }
        _ => false,
    }
}

/// How much of the end of a transcript is read for an interrupt. The marker is the last
/// conversational line and the bookkeeping after it is small; a reply longer than this that is
/// itself the last line reads as no marker, which is the safe answer.
const INTERRUPT_TAIL: u64 = 64 * 1024;

/// What Claude Code writes when the owner presses Esc: a `user` entry whose text begins with
/// this — `[Request interrupted by user]` mid-reply, `… for tool use]` mid-tool — and **no
/// hook at all**: not `Stop`, not `StopFailure`, not the tool's `PostToolUse`. Seen on
/// 2.1.291 under a pty, with every hook logging, on 2026-10-06.
const INTERRUPTED: &str = "[Request interrupted by user";

/// When the conversation's last turn was ended by an Esc: the timestamp of a trailing
/// interrupt marker, if no reply or prompt follows it. `None` for a turn that ended any other
/// way, is still going, or cannot be read — all of which leave the slot as its hooks last said.
pub fn interrupted_at(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(INTERRUPT_TAIL);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut raw = Vec::new();
    f.take(INTERRUPT_TAIL).read_to_end(&mut raw).ok()?;
    interrupted_in(&String::from_utf8_lossy(&raw), from > 0)
}

/// [`interrupted_at`], remembered per transcript until its size or modification time changes:
/// the menu asks every two seconds for every busy row, and a working turn's transcript is the
/// only thing that moves.
pub fn interrupted_at_cached(path: &Path) -> Option<u64> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    // Size and modification time, then the answer they gave.
    type Seen = HashMap<std::path::PathBuf, ((u64, Option<std::time::SystemTime>), Option<u64>)>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let meta = std::fs::metadata(path).ok()?;
    let key = (meta.len(), meta.modified().ok());
    let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = seen.lock() {
        if let Some((k, at)) = map.get(path) {
            if *k == key {
                return *at;
            }
        }
    }
    let at = interrupted_at(path);
    if let Ok(mut map) = seen.lock() {
        map.insert(path.to_path_buf(), (key, at));
    }
    at
}

/// [`interrupted_at`]'s rule over transcript text, read from the end. Pure. `cut` says the
/// text starts mid-file, so its first line may be a fragment and is not read.
pub fn interrupted_in(text: &str, cut: bool) -> Option<u64> {
    let mut lines: Vec<&str> = text.lines().collect();
    if cut && !lines.is_empty() {
        lines.remove(0);
    }
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Not one whole line we can read: the last turn's shape is unknown, so no answer.
        let v = json::parse(line).ok()?;
        match v.get("type").and_then(Value::as_str) {
            // A reply after any marker: the turn that matters ended some other way.
            Some("assistant") => return None,
            Some("user") => {
                let content = v.get("message").and_then(|m| m.get("content"));
                let marked = match content {
                    Some(Value::Str(c)) => c.trim_start().starts_with(INTERRUPTED),
                    Some(Value::Arr(parts)) => parts.iter().any(|p| {
                        p.get("type").and_then(Value::as_str) == Some("text")
                            && p.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|t| t.trim_start().starts_with(INTERRUPTED))
                    }),
                    _ => false,
                };
                return if marked {
                    crate::store::iso_ms(v.get("timestamp")?.as_str()?)
                } else {
                    None
                };
            }
            // Bookkeeping — modes, snapshots, titles, attachments, system lines — says nothing
            // about how the turn ended.
            _ => continue,
        }
    }
    None
}

/// The latest titles among the transcript lines in `text`. Pure.
pub fn titles_in(text: &str) -> Titles {
    let mut out = Titles::default();
    for line in text.lines() {
        // Cheap filter first: almost every line is a message, not a title.
        if !line.contains("-title\"") {
            continue;
        }
        let Ok(v) = json::parse(line) else { continue };
        let field = match v.get("type").and_then(Value::as_str) {
            Some("custom-title") => "customTitle",
            Some("ai-title") => "aiTitle",
            _ => continue,
        };
        let Some(title) = v
            .get(field)
            .and_then(Value::as_str)
            .and_then(crate::events::one_line)
        else {
            continue;
        };
        if field == "customTitle" {
            out.custom = Some(title);
        } else {
            out.ai = Some(title);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines an Esc left, from a 2.1.291 transcript (2026-10-06), trimmed to the fields
    /// read: the reply it cut off, the marker, and the bookkeeping written after.
    const CUT_REPLY: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"1. The ocean covers about 71%"}],"stop_reason":null},"timestamp":"2026-10-06T09:23:35.508Z"}"#;
    const MARK_REPLY: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"timestamp":"2026-10-06T09:23:35.516Z","isSidechain":false,"userType":"external"}"#;
    const REJECTED_TOOL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"The user doesn't want to proceed with this tool use.","is_error":true}]},"timestamp":"2026-10-06T09:23:47.336Z"}"#;
    const MARK_TOOL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]},"timestamp":"2026-10-06T09:23:47.340Z","isSidechain":false,"userType":"external"}"#;
    const SNAPSHOT: &str = r#"{"type":"file-history-snapshot","messageId":"m","snapshot":{},"isSnapshotUpdate":false}"#;
    const LAST_PROMPT: &str =
        r#"{"type":"last-prompt","lastPrompt":"x","leafUuid":"u","sessionId":"s"}"#;

    fn lines(ls: &[&str]) -> String {
        ls.join("\n") + "\n"
    }

    #[test]
    fn an_esc_mid_reply_or_mid_tool_is_read_from_the_tail() {
        let at = interrupted_in(&lines(&[CUT_REPLY, MARK_REPLY, SNAPSHOT]), false);
        assert_eq!(at, crate::store::iso_ms("2026-10-06T09:23:35.516Z"));
        let at = interrupted_in(
            &lines(&[REJECTED_TOOL, MARK_TOOL, LAST_PROMPT, SNAPSHOT]),
            false,
        );
        assert_eq!(at, crate::store::iso_ms("2026-10-06T09:23:47.340Z"));
    }

    #[test]
    fn a_turn_that_ended_otherwise_or_went_on_is_not_an_interrupt() {
        // Calibration for the test above: the same transcript up to the cut-off reply.
        assert_eq!(interrupted_in(&lines(&[CUT_REPLY, SNAPSHOT]), false), None);
        // A marker followed by a new prompt and its reply: that later turn is what matters.
        let typed = r#"{"type":"user","message":{"role":"user","content":"go on"},"timestamp":"2026-10-06T09:24:00.000Z"}"#;
        assert_eq!(interrupted_in(&lines(&[MARK_REPLY, typed]), false), None);
        assert_eq!(
            interrupted_in(&lines(&[MARK_REPLY, typed, CUT_REPLY]), false),
            None
        );
        // A tool's result with no marker is a turn still going.
        assert_eq!(interrupted_in(&lines(&[REJECTED_TOOL]), false), None);
    }

    #[test]
    fn what_cannot_be_read_whole_is_no_answer() {
        // The tail began inside a line: that fragment is skipped, the marker after it read.
        let fragment = &CUT_REPLY[40..];
        assert!(interrupted_in(&lines(&[fragment, MARK_REPLY]), true).is_some());
        // The last line is itself a fragment — a reply longer than the tail — so nothing is
        // known about how the turn ended, and an older marker must not answer for it.
        let torn = &CUT_REPLY[..60];
        assert_eq!(interrupted_in(&lines(&[MARK_REPLY, torn]), false), None);
    }

    /// Lines in the shape 2.1.287 writes them (read 2026-10-03), messages abbreviated.
    const FIXTURE: &str = r#"{"type":"user","message":{"role":"user","content":"Pick up the handoff"},"sessionId":"s"}
{"type":"ai-title","aiTitle":"Pick up the handoff","sessionId":"s"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"A very long reply about everything that was done, which must never become a title"}]},"sessionId":"s"}
{"type":"last-prompt","lastPrompt":"The only issue that I'd like fixed with a 0.3.1 is ...","leafUuid":"u","sessionId":"s"}
{"type":"custom-title","customTitle":"claude-sessions","sessionId":"s"}
{"type":"ai-title","aiTitle":"Fix the session titles","sessionId":"s"}
"#;

    #[test]
    fn the_latest_of_each_title_wins_and_nothing_else_is_a_title() {
        let t = titles_in(FIXTURE);
        assert_eq!(t.custom.as_deref(), Some("claude-sessions"));
        assert_eq!(
            t.ai.as_deref(),
            Some("Fix the session titles"),
            "the last ai-title"
        );
        // Calibration: with no title entries there are no titles — a reply or a last prompt
        // is never taken for one.
        let none: String = FIXTURE
            .lines()
            .filter(|l| !l.contains("-title\""))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(titles_in(&none), Titles::default());
    }

    /// A `/clear`ed conversation's whole file, in the shape infra read on 2026-10-03 from
    /// 2.1.287 — a mode line, a file-history snapshot, the command's two local entries, a
    /// system line — abbreviated.
    const CLEARED: &str = r#"{"type":"mode","mode":"default","sessionId":"c"}
{"type":"file-history-snapshot","messageId":"m","snapshot":{"trackedFileBackups":{}}}
{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"},"sessionId":"c"}
{"type":"user","message":{"role":"user","content":"<local-command-stdout></local-command-stdout>"},"sessionId":"c"}
{"type":"system","subtype":"local_command","content":"","sessionId":"c"}
"#;

    #[test]
    fn a_cleared_conversation_has_a_file_and_no_exchange() {
        assert!(!exchange_in(CLEARED));
        // Claude Code's own injected context is not a prompt either.
        let meta = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Caveat: the messages below were generated by the user"}}"#;
        assert!(!exchange_in(meta));
        // Calibration: the same file once the owner types, and once Claude replies.
        let prompted = format!(
            "{CLEARED}{}\n",
            r#"{"type":"user","message":{"role":"user","content":"retire the old tunnel"}}"#
        );
        assert!(exchange_in(&prompted), "a typed prompt");
        let replied = format!(
            "{CLEARED}{}\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#
        );
        assert!(exchange_in(&replied), "a reply");
        // And the fixture of real 2.1.287 lines above has both.
        assert!(exchange_in(FIXTURE));
    }

    #[test]
    fn has_exchange_reads_the_file_and_doubts_in_the_conversations_favour() {
        let dir = std::env::temp_dir().join(format!("cs-exchange-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        assert!(!has_exchange(&path), "no file, no conversation");
        std::fs::write(&path, CLEARED).unwrap();
        assert!(!has_exchange(&path));
        std::fs::write(&path, FIXTURE).unwrap();
        assert!(has_exchange(&path), "calibration: a real conversation");
        // Too long to settle: taken as a conversation rather than hidden.
        let filler = format!(
            "{{\"type\":\"system\",\"content\":\"{}\"}}\n",
            "x".repeat(1000)
        );
        let mut body = String::new();
        while (body.len() as u64) < HEAD + 10_000 {
            body.push_str(&filler);
        }
        std::fs::write(&path, &body).unwrap();
        assert!(has_exchange(&path));
        // Calibration: the same filler, short of the limit, settles as no conversation.
        body.truncate(filler.len() * 10);
        std::fs::write(&path, &body).unwrap();
        assert!(!has_exchange(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_tail_of_a_long_transcript_is_needed() {
        let dir = std::env::temp_dir().join(format!("cs-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // An old title, then more than the tail's worth of messages, then the current one.
        let filler = format!(
            "{{\"type\":\"assistant\",\"message\":\"{}\"}}\n",
            "x".repeat(1000)
        );
        let mut body = String::from("{\"type\":\"ai-title\",\"aiTitle\":\"old title\"}\n");
        for _ in 0..(TAIL as usize / filler.len() + 10) {
            body.push_str(&filler);
        }
        body.push_str("{\"type\":\"ai-title\",\"aiTitle\":\"current title\"}\n");
        std::fs::write(&path, &body).unwrap();
        assert_eq!(titles(&path).ai.as_deref(), Some("current title"));
        assert_eq!(titles(&dir.join("missing.jsonl")), Titles::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
