//! The menu's archive: which closed conversations are put away under Archived.
//!
//! **Archiving hides a conversation in the menu and nothing else** (owner, 2026-10-03). Its
//! transcript is Claude Code's and is never moved or changed, so `claude --resume` and Claude
//! Code's own `/resume` still find it. The archive is one small file per conversation id in
//! `archive/` beside the registry, saying `archived` or `kept`; creating or removing one is a
//! single file operation, so two menus need no lock between them.
//!
//! **A conversation is archived when its file says so, or when it has none and its last
//! entry is more than 30 days old** (owner, 2026-10-03). Age is worked out as the list is
//! read, so nothing is written for it. `kept` is what unarchiving an old conversation writes,
//! so it does not fold away again on the next reading.

use crate::clock::Millis;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// How long a closed conversation goes unused before it is archived on its own.
pub const AUTO_AFTER: Millis = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Put away with `c`.
    Archived,
    /// Taken out of the archive, and kept out whatever its age.
    Kept,
}

pub fn dir() -> PathBuf {
    crate::registry::dir().join("archive")
}

/// Every mark in the archive. Unreadable entries are no mark: the conversation is then
/// archived by age alone, which loses nothing.
pub fn marks() -> HashMap<String, Mark> {
    marks_in(&dir())
}

fn marks_in(dir: &Path) -> HashMap<String, Mark> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !valid(&name) {
            continue;
        }
        let mark = match std::fs::read_to_string(e.path()).as_deref().map(str::trim) {
            Ok("archived") => Mark::Archived,
            Ok("kept") => Mark::Kept,
            _ => continue,
        };
        out.insert(name, mark);
    }
    out
}

/// Whether a conversation last used at `last_ms` is archived, given its mark.
pub fn is_archived(mark: Option<Mark>, last_ms: Millis, now: Millis) -> bool {
    match mark {
        Some(Mark::Archived) => true,
        Some(Mark::Kept) => false,
        None => now.saturating_sub(last_ms) > AUTO_AFTER,
    }
}

/// Mark a conversation. Written to a temporary name and renamed into place, so a reader sees
/// the old mark or the new one and never half of one.
pub fn set(id: &str, mark: Mark) -> io::Result<()> {
    set_in(&dir(), id, mark)
}

fn set_in(d: &Path, id: &str, mark: Mark) -> io::Result<()> {
    if !valid(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{id:?} is not a conversation id"),
        ));
    }
    std::fs::create_dir_all(d)?;
    let tmp = d.join(format!(".{id}.{}", std::process::id()));
    let word = match mark {
        Mark::Archived => "archived",
        Mark::Kept => "kept",
    };
    std::fs::write(&tmp, format!("{word}\n"))?;
    std::fs::rename(&tmp, d.join(id))
}

/// A conversation id as Claude Code names its transcripts: a file name, never a path.
fn valid(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Millis = 24 * 60 * 60 * 1000;

    #[test]
    fn archived_by_mark_or_by_thirty_days_unused() {
        let now = 100 * DAY;
        assert!(
            !is_archived(None, now - 30 * DAY, now),
            "exactly 30 days: not yet"
        );
        assert!(is_archived(None, now - 30 * DAY - 1, now), "past 30 days");
        assert!(
            is_archived(Some(Mark::Archived), now, now),
            "put away with c"
        );
        assert!(
            !is_archived(Some(Mark::Kept), now - 90 * DAY, now),
            "unarchived stays out, whatever its age"
        );
    }

    #[test]
    fn a_mark_written_is_the_mark_read_and_the_last_one_wins() {
        let d = std::env::temp_dir().join(format!("cs-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(marks_in(&d).is_empty(), "no archive yet");
        set_in(&d, "conv-1", Mark::Archived).unwrap();
        set_in(&d, "conv-2", Mark::Archived).unwrap();
        set_in(&d, "conv-2", Mark::Kept).unwrap();
        let m = marks_in(&d);
        assert_eq!(m.get("conv-1"), Some(&Mark::Archived));
        assert_eq!(m.get("conv-2"), Some(&Mark::Kept));
        assert_eq!(m.len(), 2, "no temporary files left behind");
        assert!(set_in(&d, "../escape", Mark::Archived).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_conversation_ids_are_marks() {
        assert!(valid("76220348-a7b8-5f43-a1cf-52085651df4d"));
        assert!(!valid("../registry"));
        assert!(!valid(".hidden"));
        assert!(!valid("a/b"));
        assert!(!valid(""));
    }
}
