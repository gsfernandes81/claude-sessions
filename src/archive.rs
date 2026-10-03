//! The menu's archive: which closed conversations are put away under Archived.
//!
//! **Archiving hides a conversation in the menu and nothing else** (owner, 2026-10-03). Its
//! transcript is Claude Code's and is never moved or changed, so `claude --resume` and Claude
//! Code's own `/resume` still find it. The archive is one small file per conversation id in
//! `archive/` beside the registry, saying `archived` or `kept` and when; writing one is a
//! single rename, so two menus need no lock between them.
//!
//! **A conversation is archived when it has gone 30 days unused, or when `c` archived it and
//! it has not been used since** (owner, 2026-10-03). Age is worked out as the list is read, so
//! nothing is written for it. Each mark carries its time, because the conversation can go on
//! without the menu: one archived and then resumed by hand with `claude --resume` is in use
//! again and leaves the archive, and `kept` — what taking one out writes — gives it a fresh
//! 30 days rather than holding it out of the archive for good.

use crate::clock::Millis;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// How long a closed conversation goes unused before it is archived on its own.
pub const AUTO_AFTER: Millis = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Put away with `c`, at this time.
    Archived(Millis),
    /// Taken out of the archive at this time: 30 days from then, or from its last use if
    /// later, before age archives it again.
    Kept(Millis),
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
        let Ok(body) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let mut words = body.split_whitespace();
        let (Some(word), Some(Ok(at))) = (words.next(), words.next().map(str::parse::<Millis>))
        else {
            continue;
        };
        let mark = match word {
            "archived" => Mark::Archived(at),
            "kept" => Mark::Kept(at),
            _ => continue,
        };
        out.insert(name, mark);
    }
    out
}

/// Whether a conversation last used at `last_ms` is archived, given its mark.
pub fn is_archived(mark: Option<Mark>, last_ms: Millis, now: Millis) -> bool {
    let unused_since = |from: Millis| now.saturating_sub(from) > AUTO_AFTER;
    match mark {
        // Used after it was archived — resumed by hand — it is in use again.
        Some(Mark::Archived(at)) => last_ms <= at || unused_since(last_ms),
        Some(Mark::Kept(at)) => unused_since(last_ms.max(at)),
        None => unused_since(last_ms),
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
    let (word, at) = match mark {
        Mark::Archived(at) => ("archived", at),
        Mark::Kept(at) => ("kept", at),
    };
    std::fs::write(&tmp, format!("{word} {at}\n"))?;
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
        let used = now - DAY;
        assert!(
            is_archived(Some(Mark::Archived(now)), used, now),
            "put away with c a day after its last use"
        );
        assert!(
            !is_archived(Some(Mark::Archived(now - 2 * DAY)), used, now),
            "resumed by hand after it was archived: in use again"
        );
    }

    #[test]
    fn taking_one_out_gives_it_thirty_days_not_forever() {
        let now = 100 * DAY;
        let old = now - 90 * DAY;
        assert!(
            !is_archived(Some(Mark::Kept(now)), old, now),
            "out, though old"
        );
        assert!(
            !is_archived(Some(Mark::Kept(now - 30 * DAY)), old, now),
            "for 30 days"
        );
        assert!(
            is_archived(Some(Mark::Kept(now - 31 * DAY)), old, now),
            "then age archives it again"
        );
        // A young one taken out ages from its last use, as it would have anyway.
        assert!(!is_archived(
            Some(Mark::Kept(now - 40 * DAY)),
            now - DAY,
            now
        ));
    }

    #[test]
    fn a_mark_written_is_the_mark_read_and_the_last_one_wins() {
        let d = std::env::temp_dir().join(format!("cs-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(marks_in(&d).is_empty(), "no archive yet");
        set_in(&d, "conv-1", Mark::Archived(7)).unwrap();
        set_in(&d, "conv-2", Mark::Archived(8)).unwrap();
        set_in(&d, "conv-2", Mark::Kept(9)).unwrap();
        let m = marks_in(&d);
        assert_eq!(m.get("conv-1"), Some(&Mark::Archived(7)));
        assert_eq!(m.get("conv-2"), Some(&Mark::Kept(9)));
        assert_eq!(m.len(), 2, "no temporary files left behind");
        assert!(set_in(&d, "../escape", Mark::Archived(1)).is_err());
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
