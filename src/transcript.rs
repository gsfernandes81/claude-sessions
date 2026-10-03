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
fn exchange_line(line: &str) -> bool {
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
