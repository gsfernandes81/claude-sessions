//! Claude Code's own record of every conversation: `$CLAUDE_CONFIG_DIR/projects/<dir>/<id>.jsonl`.
//!
//! **The Closed group is read from here** (owner, 2026-10-03), not from the registry: every
//! conversation on disk that is not running, whoever started it — a slot of ours, a plain
//! `claude`, the old `ssh` path, a conversation a slot held before a `/clear`. The registry
//! only knows what ran in our slots, and only the conversation each held last; Claude Code's
//! own `/resume` picker reads this store, and so does the menu now. **Only conversations
//! started in the workspace are listed** (owner, 2026-10-03), as `/resume` lists one
//! directory's.
//!
//! What a transcript tells us, read on 2026-10-03 from 2.1.288: every entry carries
//! `sessionId`, `cwd` and `timestamp`. The file sits in the directory named for the `cwd` the
//! conversation started in, every character but a letter, a digit or `-` turned into `-`, and
//! `claude --resume <id>` finds it from that directory. A conversation can move: its entries
//! then carry other `cwd`s, and Claude Code writes a copy under the new directory too, so one
//! id can be in two places. The newest copy wins.
//!
//! **Modification times are not used for ages.** Infra saw `/clear` touch other
//! conversations' files (2026-10-03); the last entry's own `timestamp` is what a row's age is
//! measured from. They are used to know a file has not changed since it was last read: the
//! menu polls every two seconds, and reading every transcript each time would be the costly
//! part of a poll.

use crate::clock::Millis;
use crate::json::{self, Value};
use crate::transcript;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// A conversation on disk, as the menu lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    /// The directory it started in: where `claude --resume` is run from.
    pub cwd: String,
    pub path: PathBuf,
    /// What Claude Code's own picker shows: the owner's name for it, else the generated
    /// title, else the first prompt.
    pub title: String,
    /// When its last entry was written.
    pub last_ms: Millis,
}

/// How much of a transcript's start is read for its directory and first prompt.
const HEAD: u64 = 1024 * 1024;
/// How much of its end is read for its titles and last timestamp.
const TAIL: u64 = 256 * 1024;

/// The directory name Claude Code keeps a conversation started in `cwd` under.
pub fn slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Whether `cwd` is `workspace` or under it.
pub fn within(cwd: &str, workspace: &str) -> bool {
    let ws = workspace.trim_end_matches('/');
    if ws.is_empty() {
        return true;
    }
    cwd == ws
        || cwd
            .strip_prefix(ws)
            .is_some_and(|rest| rest.starts_with('/'))
}

type Cache = HashMap<PathBuf, (u64, Option<SystemTime>, Option<Conversation>)>;

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every conversation started in `workspace` with an exchange in it, one per id, most
/// recent first. A file is read again only when its size or modification time has changed.
pub fn in_workspace(config_dir: &Path, workspace: &str) -> Vec<Conversation> {
    let projects = config_dir.join("projects");
    let prefix = slug(workspace.trim_end_matches('/'));
    let mut files = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(&projects) {
        for d in dirs.flatten() {
            // The directory name is the start directory's slug, so it must begin with the
            // workspace's. Lossy — `/workspace-old` begins with it too — so the transcript's
            // own `cwd` decides; this only skips reading what cannot qualify.
            if !d.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(d.path()) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                // Subagents' transcripts live in subdirectories; only the files here are
                // conversations.
                if p.extension().is_some_and(|x| x == "jsonl") && p.is_file() {
                    files.push(p);
                }
            }
        }
    }

    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|p, _| files.contains(p));
    let mut found: HashMap<String, Conversation> = HashMap::new();
    for p in files {
        let meta = std::fs::metadata(&p).ok();
        let size = meta.as_ref().map_or(0, |m| m.len());
        let mtime = meta.and_then(|m| m.modified().ok());
        let fresh = match cache.get(&p) {
            Some((s, m, c)) if *s == size && *m == mtime => c.clone(),
            _ => {
                let c = read(&p);
                cache.insert(p.clone(), (size, mtime, c.clone()));
                c
            }
        };
        let Some(c) = fresh else { continue };
        if !within(&c.cwd, workspace) {
            continue;
        }
        match found.get(&c.id) {
            Some(have) if have.last_ms >= c.last_ms => {}
            _ => {
                found.insert(c.id.clone(), c);
            }
        }
    }
    let mut out: Vec<Conversation> = found.into_values().collect();
    out.sort_by(|a, b| b.last_ms.cmp(&a.last_ms).then_with(|| a.id.cmp(&b.id)));
    out
}

/// One transcript, or `None` when it holds no exchange (`transcript::has_exchange`) or no
/// directory to resume it from.
pub fn read(path: &Path) -> Option<Conversation> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    let dir = path.parent()?.file_name()?.to_string_lossy().to_string();
    let (cwd, first_prompt) = head(path, &dir)?;
    let tail = tail(path);
    let titles = transcript::titles_in(&tail);
    let last_ms = tail
        .lines()
        .rev()
        .find_map(timestamp_of)
        .or_else(|| {
            let t = std::fs::metadata(path).ok()?.modified().ok()?;
            Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as Millis)
        })
        .unwrap_or(0);
    let title = titles
        .custom
        .or(titles.ai)
        .or(first_prompt)
        .unwrap_or_else(|| "(no title yet)".to_string());
    Some(Conversation {
        id,
        cwd,
        path: path.to_path_buf(),
        title,
        last_ms,
    })
}

/// From the start of the file: the directory it started in — the first `cwd` whose name is
/// the directory the file sits in, else the first `cwd` at all — and the first prompt typed.
/// `None` when nothing in it is an exchange, or no entry says where it ran.
fn head(path: &Path, dir: &str) -> Option<(String, Option<String>)> {
    let f = File::open(path).ok()?;
    let mut lines = BufReader::new(f.take(HEAD));
    let mut buf = Vec::new();
    let (mut first_cwd, mut matching_cwd) = (None, None);
    let (mut exchange, mut prompt) = (false, None);
    loop {
        buf.clear();
        match lines.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        if !line.contains("\"cwd\"")
            && !line.contains("\"assistant\"")
            && !line.contains("\"user\"")
        {
            continue;
        }
        let Ok(v) = json::parse(line.trim_end()) else {
            continue;
        };
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            if first_cwd.is_none() {
                first_cwd = Some(cwd.to_string());
            }
            if matching_cwd.is_none() && slug(cwd) == dir {
                matching_cwd = Some(cwd.to_string());
            }
        }
        if transcript::exchange_line(&line) {
            exchange = true;
            if prompt.is_none() && v.get("type").and_then(Value::as_str) == Some("user") {
                prompt = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .and_then(crate::events::one_line);
            }
        }
        if exchange && prompt.is_some() && matching_cwd.is_some() {
            break;
        }
    }
    if !exchange {
        return None;
    }
    Some((matching_cwd.or(first_cwd)?, prompt))
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

/// The `timestamp` of one transcript line, in milliseconds since the epoch.
fn timestamp_of(line: &str) -> Option<Millis> {
    if !line.contains("\"timestamp\"") {
        return None;
    }
    let v = json::parse(line).ok()?;
    iso_ms(v.get("timestamp")?.as_str()?)
}

/// `2026-10-03T13:24:22.511Z` in milliseconds since the epoch. Only the UTC form Claude Code
/// writes; anything else is `None`, and the row falls back to the file's own time.
pub(crate) fn iso_ms(s: &str) -> Option<Millis> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let ms = match s.get(19..s.len() - 1) {
        Some("") => 0,
        Some(frac) if frac.starts_with('.') => {
            let digits: String = frac[1..].chars().take(3).collect();
            format!("{digits:0<3}").parse::<i64>().ok()?
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm), for the proleptic Gregorian
    // calendar.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = ((days * 24 + h) * 60 + mi) * 60 + se;
    u64::try_from(total * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, cwd: &str, ts: &str, content: &str) -> String {
        match kind {
            "user" => format!(
                r#"{{"type":"user","cwd":"{cwd}","sessionId":"s","timestamp":"{ts}","message":{{"role":"user","content":"{content}"}}}}"#
            ),
            "assistant" => format!(
                r#"{{"type":"assistant","cwd":"{cwd}","sessionId":"s","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"text","text":"{content}"}}]}}}}"#
            ),
            _ => format!(r#"{{"type":"{kind}","cwd":"{cwd}","sessionId":"s","timestamp":"{ts}"}}"#),
        }
    }

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("cs-store-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("projects")).unwrap();
            Dir(d)
        }
        fn write(&self, cwd: &str, id: &str, lines: &[String]) -> PathBuf {
            let dir = self.0.join("projects").join(slug(cwd));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("{id}.jsonl"));
            std::fs::write(&p, lines.join("\n") + "\n").unwrap();
            p
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn timestamps_read_as_claude_code_writes_them() {
        assert_eq!(iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_ms("1970-01-01T00:00:01Z"), Some(1000));
        // 2026-10-03T13:24:22.511Z, checked against `date -u -d … +%s%3N`.
        assert_eq!(iso_ms("2026-10-03T13:24:22.511Z"), Some(1_791_033_862_511));
        assert_eq!(iso_ms("2000-02-29T12:00:00.5Z"), Some(951_825_600_500));
        assert_eq!(iso_ms("2026-10-03 13:24:22"), None, "not the form written");
        assert_eq!(iso_ms("2026-10-03T13:24:22+08:00"), None);
    }

    #[test]
    fn a_conversation_reads_its_start_directory_title_and_last_entry() {
        let d = Dir::new("read");
        let p = d.write(
            "/workspace",
            "abc",
            &[
                entry("mode", "/workspace", "2026-10-01T00:00:00.000Z", ""),
                entry(
                    "user",
                    "/workspace",
                    "2026-10-01T00:00:01.000Z",
                    "retire the old tunnel",
                ),
                entry(
                    "assistant",
                    "/workspace/sub",
                    "2026-10-01T00:00:02.000Z",
                    "ok",
                ),
                r#"{"type":"ai-title","aiTitle":"Retire the tunnel","sessionId":"abc"}"#.into(),
                entry(
                    "assistant",
                    "/workspace/sub",
                    "2026-10-02T00:00:00.000Z",
                    "done",
                ),
            ],
        );
        let c = read(&p).unwrap();
        assert_eq!(c.id, "abc");
        assert_eq!(c.cwd, "/workspace", "where it started, not where it went");
        assert_eq!(c.title, "Retire the tunnel");
        assert_eq!(c.last_ms, iso_ms("2026-10-02T00:00:00.000Z").unwrap());
        // With no title entry, the first prompt.
        let p = d.write(
            "/workspace",
            "def",
            &[entry(
                "user",
                "/workspace",
                "2026-10-01T00:00:01.000Z",
                "fix the dns",
            )],
        );
        assert_eq!(read(&p).unwrap().title, "fix the dns");
    }

    #[test]
    fn a_transcript_with_no_exchange_is_not_a_conversation() {
        let d = Dir::new("empty");
        let cleared = d.write(
            "/workspace",
            "c",
            &[
                entry("mode", "/workspace", "2026-10-01T00:00:00.000Z", ""),
                entry(
                    "user",
                    "/workspace",
                    "2026-10-01T00:00:00.100Z",
                    "<command-name>/clear</command-name>",
                ),
                entry("system", "/workspace", "2026-10-01T00:00:00.200Z", ""),
            ],
        );
        assert_eq!(read(&cleared), None);
        // Calibration: the same file once prompted is one.
        let mut lines = vec![entry("mode", "/workspace", "2026-10-01T00:00:00.000Z", "")];
        lines.push(entry(
            "user",
            "/workspace",
            "2026-10-01T00:00:01.000Z",
            "hello",
        ));
        let p = d.write("/workspace", "c", &lines);
        assert!(read(&p).is_some());
    }

    #[test]
    fn only_the_workspace_is_listed_one_row_per_id_newest_first() {
        let d = Dir::new("list");
        let t = |s: &str| format!("2026-10-0{s}T00:00:00.000Z");
        d.write(
            "/workspace",
            "a",
            &[entry("user", "/workspace", &t("1"), "a")],
        );
        d.write(
            "/workspace/x",
            "b",
            &[entry("user", "/workspace/x", &t("3"), "b")],
        );
        d.write(
            "/elsewhere",
            "c",
            &[entry("user", "/elsewhere", &t("4"), "c")],
        );
        // Lossy: `/workspace-old` shares the prefix, and its own cwd rules it out.
        d.write(
            "/workspace-old",
            "e",
            &[entry("user", "/workspace-old", &t("5"), "e")],
        );
        // One conversation in two places, after its directory changed: the newer copy wins.
        d.write(
            "/workspace",
            "m",
            &[entry("user", "/workspace", &t("2"), "m")],
        );
        d.write(
            "/workspace/y",
            "m",
            &[
                entry("user", "/workspace/y", &t("2"), "m"),
                entry("assistant", "/workspace/y", &t("6"), "later"),
            ],
        );
        let got = in_workspace(&d.0, "/workspace");
        let ids: Vec<(&str, &str)> = got
            .iter()
            .map(|c| (c.id.as_str(), c.cwd.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("m", "/workspace/y"),
                ("b", "/workspace/x"),
                ("a", "/workspace")
            ]
        );
        // Calibration: with the root as the workspace, everything is.
        assert_eq!(in_workspace(&d.0, "/").len(), 5);
    }

    #[test]
    fn a_changed_file_is_read_again_and_an_unchanged_one_is_not() {
        let d = Dir::new("cache");
        let p = d.write(
            "/workspace",
            "a",
            &[entry(
                "user",
                "/workspace",
                "2026-10-01T00:00:00.000Z",
                "first",
            )],
        );
        assert_eq!(in_workspace(&d.0, "/workspace")[0].title, "first");
        // Same size and time: the cached reading stands even though the bytes differ, which
        // is how the test can see the cache at all.
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        let body = std::fs::read_to_string(&p)
            .unwrap()
            .replace("first", "FIRST");
        std::fs::write(&p, &body).unwrap();
        File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert_eq!(in_workspace(&d.0, "/workspace")[0].title, "first");
        // Appended to, as a live conversation is: read again.
        let mut more = body;
        more.push_str(&entry(
            "user",
            "/workspace",
            "2026-10-02T00:00:00.000Z",
            "second",
        ));
        more.push('\n');
        std::fs::write(&p, more).unwrap();
        let c = &in_workspace(&d.0, "/workspace")[0];
        assert_eq!(c.title, "FIRST");
        assert_eq!(c.last_ms, iso_ms("2026-10-02T00:00:00.000Z").unwrap());
    }

    #[test]
    fn within_means_the_workspace_or_under_it() {
        assert!(within("/workspace", "/workspace"));
        assert!(within("/workspace/a/b", "/workspace/"));
        assert!(!within("/workspace-old", "/workspace"));
        assert!(!within("/work", "/workspace"));
    }
}
