//! The registry: one JSON file per slot, and the only thing that knows what a session is
//! doing between logins.
//!
//! **It is written by `claude-sessions hook` and nobody else.** Every other subcommand reads
//! it, or changes a field the hooks cannot know about — `last_attach_ms`, and the
//! `offloading` state. That split is what keeps "what happened" and "what we did about it"
//! from racing.
//!
//! **Attached is not stored.** Whether a client is on a slot right now is a fact zmx
//! reports (`zmx list`'s `clients`), and a stored copy would be stale every time a
//! session is attached from a login this process never saw.
//!
//! **Reading is tolerant and writing is exact.** A record from an older version parses with
//! its missing fields defaulted, because the alternative is a menu that refuses to draw. A
//! record this version cannot parse at all is reported, not guessed at.

use crate::clock::Millis;
use crate::json::{self, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the registry lives. `~/.local/share` is a persisted volume in every dev container
/// this runs in, and keeping out of `$CLAUDE_CONFIG_DIR` leaves Claude's directory Claude's.
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_SESSIONS_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join(".local/share/claude-sessions")
}

/// The states that are *stored*. `attached` and `detached` are read from zmx and are
/// deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A claude is running in this slot.
    Live,
    /// The offloader has decided to stop it and has not finished. A `SessionEnd` arriving now
    /// means `Offloaded` rather than `Closed` — which is the entire reason this state is
    /// written down *before* the signal rather than after it.
    Offloading,
    /// Stopped to save memory. The conversation is on disk; opening the row resumes it.
    Offloaded,
    /// Ended deliberately. Never listed.
    Closed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Offloading => "offloading",
            State::Offloaded => "offloaded",
            State::Closed => "closed",
        }
    }
    /// An unknown state reads as `live`, which is the reading that cannot lose a session: a
    /// live slot wrongly called offloaded would be resumed into a second process on the same
    /// conversation, and that forks it.
    pub fn parse(s: &str) -> State {
        match s {
            "offloading" => State::Offloading,
            "offloaded" => State::Offloaded,
            "closed" => State::Closed,
            _ => State::Live,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    /// Stable within a slot: a wake-up replaces the previous wake-up, a cron is keyed by its
    /// own id.
    pub id: String,
    pub due_ms: Option<Millis>,
    pub recurring: bool,
}

/// A field two hook events can race on.
#[derive(Debug, Clone, Copy)]
pub enum Stamp {
    Conversation,
    /// `state`, between a start and an end.
    Life,
    Busy,
    NeedsYou,
    Background,
    Wakeup,
    /// `first_prompt`, which the earliest prompt takes rather than the latest.
    Prompt,
}

const STAMPS: [&str; 7] = [
    "conversation",
    "life",
    "busy",
    "needs_you",
    "background",
    "wakeup",
    "prompt",
];

/// When Claude Code fired the event that last wrote each raced field. Hooks run async and land
/// in any order, so a field takes a write only from an event at least as new as its stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    at: [Millis; STAMPS.len()],
    /// Crons whose delete landed before their create, by timer id.
    pub deleted: BTreeMap<String, Millis>,
}

impl Written {
    /// Nothing fired before `at` may write a record made then.
    pub fn new(at: Millis) -> Written {
        Written {
            at: [at; STAMPS.len()],
            deleted: BTreeMap::new(),
        }
    }

    fn to_json(&self) -> Value {
        let mut o = Value::obj();
        for (name, at) in STAMPS.iter().zip(self.at) {
            o.set(name, Value::num(at as f64));
        }
        let mut deleted = Value::obj();
        for (id, at) in &self.deleted {
            deleted.set(id, Value::num(*at as f64));
        }
        o.set("deleted", deleted);
        o
    }

    fn from_json(v: Option<&Value>) -> Written {
        let mut w = Written::new(0);
        for (name, at) in STAMPS.iter().zip(&mut w.at) {
            *at = v
                .and_then(|v| v.get(name))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if let Some(Value::Obj(m)) = v.and_then(|v| v.get("deleted")) {
            for (id, t) in m {
                if let Some(t) = t.as_u64() {
                    w.deleted.insert(id.clone(), t);
                }
            }
        }
        w
    }
}

impl std::ops::Index<Stamp> for Written {
    type Output = Millis;
    fn index(&self, s: Stamp) -> &Millis {
        &self.at[s as usize]
    }
}

impl std::ops::IndexMut<Stamp> for Written {
    fn index_mut(&mut self, s: Stamp) -> &mut Millis {
        &mut self.at[s as usize]
    }
}

#[derive(Debug, Clone)]
pub struct SlotRecord {
    pub slot: String,
    pub pid: Option<u32>,
    /// `/proc` start-time ticks for `pid`. Present whenever `pid` is: a pid without one is a
    /// pid that will eventually name the wrong process.
    pub proc_start: Option<u64>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// The owner's own name for the current conversation (its custom title).
    pub title: Option<String>,
    /// Claude Code's generated title for the current conversation — what its session
    /// selector shows when there is no custom title.
    pub ai_title: Option<String>,
    /// Where Claude Code keeps the current conversation's transcript, as its hooks report it.
    /// Whether it holds a conversation is `has_conversation`'s question (issue #5).
    pub transcript_path: Option<String>,
    /// The current conversation's first prompt, on one line and cut short — the title of last
    /// resort, for a conversation Claude Code has not named. Written once per conversation by
    /// `UserPromptSubmit`.
    pub first_prompt: Option<String>,
    pub state: State,
    pub busy: bool,
    pub needs_you: bool,
    pub last_activity_ms: Millis,
    /// When the owner last had this slot on their screen. Written by the menu, never by a
    /// hook — a hook cannot know it.
    pub last_attach_ms: Millis,
    /// The most recent `Stop`. `unread` is this being later than `last_attach_ms`, which is
    /// why neither is derived from the other.
    pub last_stop_ms: Option<Millis>,
    /// The most recent `SessionStart` that opened a conversation at its prompt — `startup`,
    /// `resume`, `clear` or `fork`. A claude in that state is idle as surely as one after a
    /// `Stop`, so the offloader counts idleness from whichever is later. Not `compact`: that
    /// can fire mid-turn, and nothing in the vendor docs says otherwise.
    pub ready_ms: Option<Millis>,
    pub timers: Vec<Timer>,
    /// The background work running in the slot's claude, one `type: description` per task
    /// (issues #9, #10). In-process work — a background subagent, a Workflow run, a teammate, a
    /// cloud session — has no process of its own to see, so the slot keeps a list, and while it
    /// is not empty the slot is never offloaded. Each `Stop` and each `SubagentStop` replaces it
    /// with Claude Code's own list; between them a `SubagentStart` adds the agent it announces —
    /// which is what keeps an agent started in a turn the owner ended with Esc, since an Esc
    /// fires no `Stop`.
    pub background: Vec<String>,
    /// False for a session this tool did not start — a `zmx attach work claude` somebody
    /// typed. Listed, marked, and never assumed to behave like one of ours.
    pub registered: bool,
    pub updated_ms: Millis,
    /// Last time each hook event was seen, for `doctor`. A slot whose `Stop` is months old
    /// while `UserPromptSubmit` is recent means a hook stopped being delivered, and that is
    /// invisible without this.
    pub last_event_ms: BTreeMap<String, Millis>,
    pub written: Written,
}

impl SlotRecord {
    pub fn new(slot: &str, now: Millis) -> Self {
        SlotRecord {
            slot: slot.to_string(),
            pid: None,
            proc_start: None,
            session_id: None,
            cwd: None,
            title: None,
            ai_title: None,
            transcript_path: None,
            first_prompt: None,
            state: State::Live,
            busy: false,
            needs_you: false,
            last_activity_ms: now,
            last_attach_ms: 0,
            last_stop_ms: None,
            ready_ms: None,
            timers: Vec::new(),
            background: Vec::new(),
            registered: true,
            updated_ms: now,
            last_event_ms: BTreeMap::new(),
            written: Written::new(now),
        }
    }

    /// When an Esc ended the turn, given the transcript's trailing interrupt marker: the marker,
    /// if it is no older than the latest activity the hooks recorded (an Esc fires no hook, and
    /// `SubagentStart`/`SubagentStop` are not activity). The one rule the offloader and the menu
    /// both read, so they cannot disagree about a slot.
    pub fn esc_ended(&self, interrupted_at: Option<Millis>) -> Option<Millis> {
        interrupted_at.filter(|&at| at >= self.last_activity_ms)
    }

    /// Where the current conversation's transcript is: the path the hooks reported, else —
    /// for a record written before 0.3.3 recorded it — where Claude Code puts one, under
    /// `projects/` named for the directory with every character that is not a letter, a
    /// digit or `-` turned into `-` (`/home/user/x` → `-home-user-x`).
    pub fn conversation_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.transcript_path {
            return Some(PathBuf::from(p));
        }
        let (id, cwd) = (self.session_id.as_deref()?, self.cwd.as_deref()?);
        let slug = crate::store::slug(cwd);
        Some(
            crate::live::config_dir()
                .join("projects")
                .join(slug)
                .join(format!("{id}.jsonl")),
        )
    }

    /// Whether there is a conversation on disk to resume. **A recorded `session_id` is not
    /// enough, and nor is a file** (issue #5): a new session's transcript appears only at its
    /// first prompt, and a `/clear`ed one's appears at once with nothing in it to come back
    /// to. So the transcript must hold an exchange (`transcript::has_exchange`).
    pub fn has_conversation(&self) -> bool {
        self.conversation_path()
            .is_some_and(|p| crate::transcript::has_exchange(&p))
    }

    /// The title a row shows: what Claude Code's own session selector would — the custom
    /// title, else the generated one — then the first prompt, then nothing yet.
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.ai_title.clone())
            .or_else(|| self.first_prompt.clone())
            .unwrap_or_else(|| "(no title yet)".into())
    }

    /// Derived, never stored: it finished something while you were away.
    pub fn unread(&self) -> bool {
        match self.last_stop_ms {
            Some(stop) => stop > self.last_attach_ms,
            None => false,
        }
    }

    /// A timer that has not fired yet, by our clock. A slot with one is never offloaded,
    /// whoever set it.
    ///
    /// A timer with no due time counts as pending. That is the deliberate direction: an
    /// unreadable payload shape should keep a session alive rather than let it be stopped
    /// while something is still waiting to fire.
    pub fn has_pending_timer(&self, now: Millis) -> bool {
        self.timers
            .iter()
            .any(|t| t.recurring || t.due_ms.is_none_or(|due| due > now))
    }

    pub fn path(&self) -> PathBuf {
        slot_path(&self.slot)
    }

    pub fn to_json(&self) -> Value {
        let mut o = Value::obj();
        o.set("slot", Value::string(&self.slot));
        o.set("state", Value::string(self.state.as_str()));
        set_opt_u64(&mut o, "pid", self.pid.map(u64::from));
        set_opt_u64(&mut o, "proc_start", self.proc_start);
        set_opt_str(&mut o, "session_id", self.session_id.as_deref());
        set_opt_str(&mut o, "cwd", self.cwd.as_deref());
        set_opt_str(&mut o, "title", self.title.as_deref());
        set_opt_str(&mut o, "ai_title", self.ai_title.as_deref());
        set_opt_str(&mut o, "transcript_path", self.transcript_path.as_deref());
        set_opt_str(&mut o, "first_prompt", self.first_prompt.as_deref());
        o.set("busy", Value::Bool(self.busy));
        o.set("needs_you", Value::Bool(self.needs_you));
        o.set("registered", Value::Bool(self.registered));
        o.set("last_activity_ms", Value::num(self.last_activity_ms as f64));
        o.set("last_attach_ms", Value::num(self.last_attach_ms as f64));
        set_opt_u64(&mut o, "last_stop_ms", self.last_stop_ms);
        set_opt_u64(&mut o, "ready_ms", self.ready_ms);
        o.set("updated_ms", Value::num(self.updated_ms as f64));
        o.set(
            "timers",
            Value::Arr(
                self.timers
                    .iter()
                    .map(|t| {
                        let mut v = Value::obj();
                        v.set("id", Value::string(&t.id));
                        set_opt_u64(&mut v, "due_ms", t.due_ms);
                        v.set("recurring", Value::Bool(t.recurring));
                        v
                    })
                    .collect(),
            ),
        );
        if !self.background.is_empty() {
            o.set(
                "background",
                Value::Arr(self.background.iter().map(Value::string).collect()),
            );
        }
        let mut ev = Value::obj();
        for (k, at) in &self.last_event_ms {
            ev.set(k, Value::num(*at as f64));
        }
        o.set("last_event_ms", ev);
        o.set("written", self.written.to_json());
        o
    }

    /// Read a record, defaulting anything absent.
    ///
    /// Returns `None` only when there is no slot name to key it by — everything else has a
    /// safe default, because a record written by an older version must still produce a usable
    /// row rather than an error on the ssh path.
    pub fn from_json(v: &Value, fallback_slot: &str) -> Option<SlotRecord> {
        let slot = v
            .get("slot")
            .and_then(Value::as_str)
            .unwrap_or(fallback_slot);
        if slot.is_empty() {
            return None;
        }
        let timers = v
            .get("timers")
            .and_then(Value::as_arr)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(Timer {
                            id: t.get("id").and_then(Value::as_str)?.to_string(),
                            due_ms: t.get("due_ms").and_then(Value::as_u64),
                            recurring: t.get("recurring").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut last_event_ms = BTreeMap::new();
        if let Some(Value::Obj(m)) = v.get("last_event_ms") {
            for (k, at) in m {
                if let Some(at) = at.as_u64() {
                    last_event_ms.insert(k.clone(), at);
                }
            }
        }
        Some(SlotRecord {
            slot: slot.to_string(),
            pid: v.get("pid").and_then(Value::as_u32),
            proc_start: v.get("proc_start").and_then(Value::as_u64),
            session_id: str_of(v, "session_id"),
            cwd: str_of(v, "cwd"),
            title: str_of(v, "title"),
            ai_title: str_of(v, "ai_title"),
            transcript_path: str_of(v, "transcript_path"),
            first_prompt: str_of(v, "first_prompt"),
            state: State::parse(v.get("state").and_then(Value::as_str).unwrap_or("live")),
            busy: v.get("busy").and_then(Value::as_bool).unwrap_or(false),
            needs_you: v.get("needs_you").and_then(Value::as_bool).unwrap_or(false),
            last_activity_ms: v
                .get("last_activity_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            last_attach_ms: v.get("last_attach_ms").and_then(Value::as_u64).unwrap_or(0),
            last_stop_ms: v.get("last_stop_ms").and_then(Value::as_u64),
            ready_ms: v.get("ready_ms").and_then(Value::as_u64),
            timers,
            background: v
                .get("background")
                .and_then(Value::as_arr)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            registered: v.get("registered").and_then(Value::as_bool).unwrap_or(true),
            updated_ms: v.get("updated_ms").and_then(Value::as_u64).unwrap_or(0),
            last_event_ms,
            written: Written::from_json(v.get("written")),
        })
    }
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}
fn set_opt_str(o: &mut Value, key: &str, s: Option<&str>) {
    if let Some(s) = s {
        o.set(key, Value::string(s));
    }
}
fn set_opt_u64(o: &mut Value, key: &str, n: Option<u64>) {
    if let Some(n) = n {
        o.set(key, Value::num(n as f64));
    }
}

pub fn slot_path(slot: &str) -> PathBuf {
    dir().join(format!("{slot}.json"))
}

pub fn lock_path(slot: &str) -> PathBuf {
    dir().join(format!("{slot}.lock"))
}

pub fn load(slot: &str) -> std::io::Result<Option<SlotRecord>> {
    let p = slot_path(slot);
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            let v = json::parse(&s).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: {e}", p.display()),
                )
            })?;
            Ok(SlotRecord::from_json(&v, slot))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write a record, atomically.
///
/// Temp file then rename, because a reader that catches a half-written file gets invalid JSON
/// and this is read on the ssh path. The temp name carries the pid so two writers cannot
/// collide on it even if the lock were ever bypassed.
pub fn store(rec: &SlotRecord) -> std::io::Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.{}.tmp", rec.slot, std::process::id()));
    std::fs::write(&tmp, json::to_string_pretty(&rec.to_json()))?;
    std::fs::rename(&tmp, rec.path())
}

/// Every slot the registry knows about, in no particular order.
///
/// One unreadable record must not hide the others: the menu's job is to show what it can see,
/// and `doctor` is where a reader goes to find out what it cannot.
pub fn all() -> std::io::Result<Vec<SlotRecord>> {
    let mut out = Vec::new();
    let d = dir();
    if !d.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&d)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let slot = name.trim_end_matches(".json");
        if let Ok(Some(rec)) = load(slot) {
            out.push(rec);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_survives_a_round_trip() {
        let mut rec = SlotRecord::new("claude-3", 1_000);
        rec.pid = Some(42);
        rec.proc_start = Some(99);
        rec.session_id = Some("abc".into());
        rec.title = Some("retire the old tunnel".into());
        rec.first_prompt = Some("move the tunnel to the new box".into());
        rec.written[Stamp::Busy] = 1_500;
        rec.written.deleted.insert("cron:c1".into(), 1_600);
        rec.state = State::Offloaded;
        rec.needs_you = true;
        rec.last_stop_ms = Some(2_000);
        rec.timers.push(Timer {
            id: "wakeup".into(),
            due_ms: Some(5_000),
            recurring: false,
        });
        rec.last_event_ms.insert("Stop".into(), 2_000);

        let back = SlotRecord::from_json(&rec.to_json(), "wrong").expect("parses");
        assert_eq!(back.slot, "claude-3");
        assert_eq!(back.pid, Some(42));
        assert_eq!(back.proc_start, Some(99));
        assert_eq!(back.state, State::Offloaded);
        assert_eq!(back.first_prompt, rec.first_prompt);
        assert_eq!(back.written, rec.written);
        assert!(back.needs_you);
        assert_eq!(back.timers, rec.timers);
        assert_eq!(back.last_event_ms.get("Stop"), Some(&2_000));
    }

    #[test]
    fn a_record_from_an_older_version_still_reads() {
        // Only the fields that existed first. Everything else has to default rather than fail,
        // or an upgrade makes every existing session invisible.
        let v = crate::json::parse(r#"{"slot":"claude-1","state":"live"}"#).unwrap();
        let rec = SlotRecord::from_json(&v, "claude-1").expect("parses");
        assert_eq!(rec.state, State::Live);
        assert!(
            rec.registered,
            "a record with no `registered` field is one of ours"
        );
        assert!(!rec.unread());
        assert!(rec.timers.is_empty());
    }

    #[test]
    fn an_unknown_state_reads_as_live() {
        // The direction matters: calling a live slot offloaded would have the menu resume it
        // into a second process on the same conversation, which forks it.
        assert_eq!(State::parse("something-new"), State::Live);
    }

    #[test]
    fn a_row_shows_the_title_claude_codes_own_selector_would() {
        let mut rec = SlotRecord::new("claude-1", 0);
        assert_eq!(rec.display_title(), "(no title yet)");
        rec.first_prompt = Some("fix the tunnel please".into());
        assert_eq!(
            rec.display_title(),
            "fix the tunnel please",
            "the last resort"
        );
        rec.ai_title = Some("Retire the old tunnel".into());
        assert_eq!(
            rec.display_title(),
            "Retire the old tunnel",
            "the generated title"
        );
        rec.title = Some("tunnel".into());
        assert_eq!(rec.display_title(), "tunnel", "the owner's own name wins");
    }

    /// A transcript with an exchange in it.
    const PROMPTED: &str =
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";

    #[test]
    fn a_conversation_exists_only_once_its_transcript_does() {
        let dir = std::env::temp_dir().join(format!("cs-conv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.jsonl");
        let mut rec = SlotRecord::new("claude-1", 0);
        rec.session_id = Some("abc".into());
        rec.cwd = Some("/workspace".into());
        rec.transcript_path = Some(path.display().to_string());
        assert!(!rec.has_conversation(), "never prompted: no transcript yet");
        std::fs::write(&path, "{\"type\":\"mode\"}\n").unwrap();
        assert!(!rec.has_conversation(), "a file with no exchange: /clear's");
        std::fs::write(&path, PROMPTED).unwrap();
        assert!(
            rec.has_conversation(),
            "calibration: once prompted, it is there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_record_finds_its_transcript_where_claude_code_keeps_it() {
        let mut rec = SlotRecord::new("claude-1", 0);
        rec.session_id = Some("abc".into());
        rec.cwd = Some("/home/user/claude-sessions".into());
        let p = rec.conversation_path().unwrap();
        assert!(
            p.ends_with("projects/-home-user-claude-sessions/abc.jsonl"),
            "got {}",
            p.display()
        );
        rec.session_id = None;
        assert_eq!(rec.conversation_path(), None, "no id, no conversation");
    }

    #[test]
    fn unread_is_a_stop_later_than_the_last_look() {
        let mut rec = SlotRecord::new("claude-1", 0);
        assert!(!rec.unread(), "nothing has finished yet");
        rec.last_stop_ms = Some(100);
        rec.last_attach_ms = 50;
        assert!(rec.unread(), "it finished after you last looked");
        rec.last_attach_ms = 150;
        assert!(!rec.unread(), "you have looked since");
    }

    #[test]
    fn a_timer_with_no_due_time_counts_as_pending() {
        let mut rec = SlotRecord::new("claude-1", 0);
        rec.timers.push(Timer {
            id: "wakeup".into(),
            due_ms: None,
            recurring: false,
        });
        assert!(rec.has_pending_timer(10_000));
        rec.timers[0].due_ms = Some(5_000);
        assert!(!rec.has_pending_timer(10_000), "it has already fired");
        rec.timers[0].recurring = true;
        assert!(
            rec.has_pending_timer(10_000),
            "a recurring timer is always pending"
        );
    }
}
