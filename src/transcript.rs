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
use std::io::{Read, Seek, SeekFrom};
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
