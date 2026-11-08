//! The phone's unsent prompts: what was typed in a conversation and not sent, kept when the
//! view leaves it — offloaded, archived or still running — and put back when it is opened
//! again (owner, 2026-11-08). One file on the device the client runs on, never the hub's: it
//! costs the metered link nothing, and what the hub or a container does with the session in
//! between does not touch it.
//!
//! **Keyed by container and conversation id**, never by slot: a slot's name is reused once it
//! closes, and a draft must not turn up in a stranger's conversation. A conversation keeps
//! its id through offload and resume, which is what makes an offloaded one work.
//!
//! A draft is the prompt as it stood, its pastes collapsed — `[Pasted text #N +L lines]` with
//! each paste's text beside it — and the cursor. Read tolerantly: a file or an entry this
//! cannot read is no draft, never an error the owner sees. Written whole, temp then rename,
//! under a lock with a timeout, re-reading the file first so two clients on one device keep
//! each other's drafts.

use crate::json::{self, Value};
use crate::lockfile::SlotLock;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A draft unsaved for this long is dropped: the age at which the menu archives a session.
const KEEP_MS: u64 = 30 * 24 * 3600 * 1000;
/// The most the file holds; past it, the oldest drafts go. A paste can be large.
const MOST_BYTES: usize = 4 << 20;
/// What a save may wait for another client's: the client draws nothing meanwhile.
const LOCK_WAIT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub text: String,
    /// In chars into `text`.
    pub cursor: usize,
    /// Each paste standing in `text`, by number.
    pub pastes: Vec<(u32, String)>,
}

/// Where the drafts are kept: `$CLAUDE_REMOTE_DRAFTS`, else under `$XDG_STATE_HOME`, else
/// `~/.local/state`. None without a home to put them in.
pub fn path(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(p) = set("CLAUDE_REMOTE_DRAFTS") {
        return Some(PathBuf::from(p));
    }
    let state = set("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|h| Path::new(&h).join(".local/state")))?;
    Some(state.join("claude-remote/drafts.json"))
}

/// The draft kept for a conversation, if any.
pub fn load(file: &Path, host: &str, sid: &str) -> Option<Draft> {
    let all = read(file);
    all.iter()
        .find(|e| str_of(e, "c") == host && str_of(e, "sid") == sid)
        .and_then(draft_of)
}

/// Keep `draft` for a conversation, or drop its entry when there is none or it is empty;
/// `now` in epoch milliseconds, for the age.
pub fn save(file: &Path, host: &str, sid: &str, draft: Option<&Draft>, now: u64) -> bool {
    let lock = file.with_extension("lock");
    let Ok(_held) = SlotLock::acquire(&lock, LOCK_WAIT) else {
        return false;
    };
    let mut all: Vec<Value> = read(file)
        .into_iter()
        .filter(|e| !(str_of(e, "c") == host && str_of(e, "sid") == sid))
        .filter(|e| {
            e.get("at")
                .and_then(Value::as_u64)
                .is_some_and(|at| now.saturating_sub(at) < KEEP_MS)
        })
        .collect();
    if let Some(d) = draft.filter(|d| !d.text.trim().is_empty()) {
        all.push(entry(host, sid, d, now));
    }
    // Newest last; the oldest go first while the whole is too big.
    all.sort_by_key(|e| e.get("at").and_then(Value::as_u64).unwrap_or(0));
    let mut doc = whole(all.clone());
    while json::to_line(&doc).len() > MOST_BYTES && !all.is_empty() {
        all.remove(0);
        doc = whole(all.clone());
    }
    write(file, &doc).is_ok()
}

fn whole(drafts: Vec<Value>) -> Value {
    let mut doc = Value::obj();
    doc.set("v", Value::num(1.0));
    doc.set("drafts", Value::Arr(drafts));
    doc
}

fn write(file: &Path, doc: &Value) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, json::to_line(doc) + "\n")?;
    std::fs::rename(&tmp, file)
}

/// Every entry the file holds that is an object; nothing, if it cannot be read.
fn read(file: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let Ok(doc) = json::parse(text.trim()) else {
        return Vec::new();
    };
    doc.get("drafts")
        .and_then(Value::as_arr)
        .unwrap_or(&[])
        .iter()
        .filter(|e| matches!(e, Value::Obj(_)))
        .cloned()
        .collect()
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn entry(host: &str, sid: &str, d: &Draft, now: u64) -> Value {
    let mut e = Value::obj();
    e.set("c", Value::string(host));
    e.set("sid", Value::string(sid));
    e.set("at", Value::num(now as f64));
    e.set("x", Value::string(&d.text));
    e.set("cur", Value::num(d.cursor as f64));
    let pastes = d
        .pastes
        .iter()
        .map(|(n, t)| {
            let mut p = Value::obj();
            p.set("n", Value::num(*n));
            p.set("x", Value::string(t));
            p
        })
        .collect();
    e.set("p", Value::Arr(pastes));
    e
}

fn draft_of(e: &Value) -> Option<Draft> {
    let text = e.get("x").and_then(Value::as_str)?.to_string();
    let pastes = e
        .get("p")
        .and_then(Value::as_arr)
        .unwrap_or(&[])
        .iter()
        .filter_map(|p| {
            Some((
                p.get("n").and_then(Value::as_u32)?,
                p.get("x").and_then(Value::as_str)?.to_string(),
            ))
        })
        .collect();
    let most = text.chars().count();
    let cursor = e
        .get("cur")
        .and_then(Value::as_u64)
        .map_or(most, |c| (c as usize).min(most));
    Some(Draft {
        text,
        cursor,
        pastes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "claude-remote-drafts-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d.join("drafts.json")
    }

    fn draft(text: &str) -> Draft {
        Draft {
            text: text.into(),
            cursor: 3,
            pastes: Vec::new(),
        }
    }

    #[test]
    fn a_draft_comes_back_with_its_pastes_for_its_conversation_only() {
        let f = scratch("back");
        let d = Draft {
            text: "look: [Pasted text #2 +3 lines] and".into(),
            cursor: 9,
            pastes: vec![(2, "a\nb\nc\nd".into())],
        };
        assert!(save(&f, "dev", "s1", Some(&d), 1_000));
        assert_eq!(load(&f, "dev", "s1"), Some(d));
        assert_eq!(load(&f, "dev", "s2"), None, "another conversation");
        assert_eq!(load(&f, "other", "s1"), None, "another container");
    }

    #[test]
    fn an_empty_draft_drops_the_entry_and_others_are_kept() {
        let f = scratch("drop");
        save(&f, "dev", "s1", Some(&draft("one")), 1_000);
        save(&f, "dev", "s2", Some(&draft("two")), 1_000);
        save(&f, "dev", "s1", Some(&draft("  \n")), 2_000);
        assert_eq!(load(&f, "dev", "s1"), None);
        save(&f, "dev", "s2", None, 2_000);
        assert_eq!(load(&f, "dev", "s2"), None);
        save(&f, "dev", "s3", Some(&draft("three")), 2_000);
        save(&f, "dev", "s4", Some(&draft("four")), 2_000);
        assert_eq!(load(&f, "dev", "s3").map(|d| d.text), Some("three".into()));
    }

    #[test]
    fn a_month_old_draft_goes_at_the_next_save() {
        let f = scratch("old");
        save(&f, "dev", "s1", Some(&draft("old")), 1_000);
        save(&f, "dev", "s2", Some(&draft("new")), 1_000 + KEEP_MS);
        assert_eq!(load(&f, "dev", "s1"), None);
        assert!(load(&f, "dev", "s2").is_some());
    }

    #[test]
    fn a_draft_is_kept_for_thirty_days() {
        const DAY: u64 = 24 * 3600 * 1000;
        let f = scratch("days");
        save(&f, "dev", "s1", Some(&draft("kept")), 1_000);
        save(&f, "dev", "s2", Some(&draft("later")), 1_000 + 29 * DAY);
        assert!(load(&f, "dev", "s1").is_some(), "29 days on");
        save(&f, "dev", "s3", Some(&draft("later")), 1_000 + 31 * DAY);
        assert_eq!(load(&f, "dev", "s1"), None, "31 days on");
    }

    #[test]
    fn past_the_cap_the_oldest_go_first() {
        let f = scratch("cap");
        let big = "x".repeat(MOST_BYTES / 3);
        for (i, sid) in ["s1", "s2", "s3", "s4"].iter().enumerate() {
            save(&f, "dev", sid, Some(&draft(&big)), 1_000 + i as u64);
        }
        assert_eq!(load(&f, "dev", "s1"), None);
        assert_eq!(load(&f, "dev", "s2"), None);
        assert!(load(&f, "dev", "s3").is_some() && load(&f, "dev", "s4").is_some());
    }

    #[test]
    fn what_cannot_be_read_is_no_draft_and_a_save_starts_it_again() {
        let f = scratch("junk");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "{not json").unwrap();
        assert_eq!(load(&f, "dev", "s1"), None);
        assert!(save(&f, "dev", "s1", Some(&draft("fresh")), 1_000));
        assert_eq!(load(&f, "dev", "s1").map(|d| d.text), Some("fresh".into()));
        // An entry missing what it needs is skipped; a cursor past the end is the end.
        std::fs::write(
            &f,
            r#"{"v":1,"drafts":[7,{"c":"dev","sid":"s1"},{"c":"dev","sid":"s2","at":1,"x":"ab","cur":99,"p":[{"n":"x"}]}]}"#,
        )
        .unwrap();
        assert_eq!(load(&f, "dev", "s1"), None);
        assert_eq!(
            load(&f, "dev", "s2"),
            Some(Draft {
                text: "ab".into(),
                cursor: 2,
                pastes: Vec::new()
            })
        );
    }

    #[test]
    fn the_file_is_where_the_environment_says() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            path(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.local/state/claude-remote/drafts.json"))
        );
        assert_eq!(
            path(env(&[("HOME", "/h"), ("XDG_STATE_HOME", "/s")])),
            Some(PathBuf::from("/s/claude-remote/drafts.json"))
        );
        assert_eq!(
            path(env(&[("HOME", "/h"), ("CLAUDE_REMOTE_DRAFTS", "/d.json")])),
            Some(PathBuf::from("/d.json"))
        );
        assert_eq!(path(env(&[])), None);
    }
}
