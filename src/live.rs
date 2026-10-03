//! What Claude Code already knows about its own live sessions — and why a registry is still
//! needed.
//!
//! `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` holds, for each running session: `pid`,
//! `sessionId`, `cwd`, `startedAt`, `procStart` (the `/proc` start-time ticks), `version`,
//! `kind` (`interactive` | `bg`), `name` + `nameSource` + `nameSince`, and `status`
//! (`busy` | `idle`). Observed on 2026-10-01 against 2.1.286.
//!
//! **This is the fast path and the corroboration, never the contract.** It is worth being
//! explicit, because the obvious question about this whole tool is whether it rebuilds what
//! `claude --resume` and this file already give:
//!
//! 1. **They describe conversations and processes, not slots.** `--resume` lists conversations
//!    on disk and will happily open one that is ALREADY RUNNING in another process, which
//!    forks it. Nothing in either place says "this one is live, attach instead of resuming" —
//!    and that is the first thing a menu has to get right.
//! 2. **They are gone exactly when they are needed.** This file is pid-keyed and only exists
//!    while a session runs. The moment a slot is offloaded its file disappears, and an
//!    offloaded slot is precisely when you need to know which conversation belonged to it.
//! 3. **Neither records attention or absence.** Nothing says a permission prompt is waiting,
//!    nothing says a timer is pending, and nothing records *when you last looked* — which is
//!    the whole of `unread`. Those three are what the menu exists to show.
//!
//! It is also undocumented internal state that moves with a binary which updates itself in
//! place, so a renamed field here must degrade to "no evidence". It is read to know which
//! conversations are running — `doctor`, and the launcher's never-resume-a-running-
//! conversation check — and never depended on being there. **It is not read for titles**:
//! its `name` showed one of Claude's long replies on the boxes (0.3.1); titles come from the
//! transcript, as Claude Code's own session selector takes them (`transcript.rs`).

use crate::json::{self, Value};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct LiveSession {
    pub pid: u32,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// `/proc` start-time ticks — the same guard against a reused pid that our own records use.
    pub proc_start: Option<u64>,
    /// `interactive` or `bg`. A `bg` session is a background job, not a slot, and must never
    /// be listed as one.
    pub kind: Option<String>,
    pub status: Option<String>,
}

impl LiveSession {
    fn from_json(v: &Value) -> Option<LiveSession> {
        Some(LiveSession {
            pid: v
                .get("pid")
                .and_then(Value::as_u64_lenient)
                .and_then(|p| u32::try_from(p).ok())?,
            session_id: s(v, "sessionId"),
            cwd: s(v, "cwd"),
            // A string in the files Claude Code writes (issue #4); a number would do too.
            proc_start: v.get("procStart").and_then(Value::as_u64_lenient),
            kind: s(v, "kind"),
            status: s(v, "status"),
        })
    }
    pub fn is_interactive(&self) -> bool {
        self.kind.as_deref() != Some("bg")
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Claude Code's configuration directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .unwrap_or_else(|_| format!("{}/.claude", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(base)
}

fn sessions_dir() -> PathBuf {
    config_dir().join("sessions")
}

/// Every live session Claude Code currently claims, stale files skipped.
///
/// The files are pid-keyed and nothing cleans them up — a container running since August
/// holds months of them — so `procStart` is what separates a live session from a dead file
/// whose pid has since been handed to something else.
pub fn all() -> Vec<LiveSession> {
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().ends_with(".json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(v) = json::parse(&body) else { continue };
        let Some(sess) = LiveSession::from_json(&v) else {
            continue;
        };
        if sess.is_fresh() {
            out.push(sess);
        }
    }
    out
}

impl LiveSession {
    /// Whether the process this file describes is still the one running under its pid.
    fn is_fresh(&self) -> bool {
        match self.proc_start {
            Some(start) => crate::procinfo::is_alive(self.pid, start),
            // Without a start time the best available test is that the pid is a process at
            // all — `start_time` refuses a thread id, which a stale pid can turn into.
            None => crate::procinfo::start_time(self.pid).is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sessions file in the shape Claude Code 2.1.287 writes it, read on 2026-10-02 —
    /// field names and types as found, values made up. Note `procStart`: a STRING.
    fn fixture(pid: u32, proc_start: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"1bad2ea9-0000-4000-8000-000000000000",
            "cwd":"/workspace","startedAt":1790892170775,"procStart":"{proc_start}",
            "version":"2.1.287","peerProtocol":1,"peerFeatures":[],"kind":"interactive",
            "entrypoint":"cli","pidDomain":"x","messagingSocketPath":"/tmp/x.sock",
            "name":"workspace-07","nameSource":"derived","nameSince":1790892170775,
            "updatedAt":1790892170775,"status":"idle","statusUpdatedAt":1790892170775}}"#
        )
    }

    fn parse(body: &str) -> LiveSession {
        LiveSession::from_json(&json::parse(body).expect("fixture is json")).expect("a session")
    }

    #[test]
    fn a_string_proc_start_is_read_and_compared() {
        let me = std::process::id();
        let start = crate::procinfo::start_time(me).unwrap();
        let s = parse(&fixture(me, &start.to_string()));
        assert_eq!(
            s.proc_start,
            Some(start),
            "the string is read as the number it is"
        );
        assert!(
            s.is_fresh(),
            "calibration: our own process, our own start time"
        );
        let stale = parse(&fixture(me, &(start + 1).to_string()));
        assert!(
            !stale.is_fresh(),
            "a live pid with another start time is a dead file whose pid was reused"
        );
    }

    #[test]
    fn the_fixture_reads_as_claude_code_wrote_it() {
        let s = parse(&fixture(425, "503696084"));
        assert_eq!(s.pid, 425);
        assert_eq!(s.kind.as_deref(), Some("interactive"));
        assert_eq!(s.status.as_deref(), Some("idle"));
    }
}
