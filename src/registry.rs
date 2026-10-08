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

use crate::clock::{Millis, Moment};
use crate::json::{self, Value};
use crate::procinfo;
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

/// A field two hook events can race on.
#[derive(Debug, Clone, Copy)]
pub enum Field {
    Conversation,
    /// `state`, between a start and an end.
    Life,
    Busy,
    NeedsYou,
    /// `first_prompt`, which the earliest prompt takes rather than the latest.
    Prompt,
}

impl Field {
    pub const ALL: [Field; 5] = [
        Field::Conversation,
        Field::Life,
        Field::Busy,
        Field::NeedsYou,
        Field::Prompt,
    ];

    fn name(self) -> &'static str {
        match self {
            Field::Conversation => "conversation",
            Field::Life => "life",
            Field::Busy => "busy",
            Field::NeedsYou => "needs_you",
            Field::Prompt => "prompt",
        }
    }
}

/// The clock tick of the event that last wrote each `Field`. Hooks run async and land in any
/// order, so a field takes a write only from an event fired in the same tick or later
/// (`events`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    ticks: [u64; Field::ALL.len()],
    /// The kernel's boot id: ticks count from boot, so another boot's mean nothing.
    boot: String,
}

impl Written {
    /// Nothing fired before `tick` may write a record made then.
    pub fn new(tick: u64) -> Written {
        Written {
            ticks: [tick; Field::ALL.len()],
            boot: procinfo::boot_id(),
        }
    }

    /// Takes `f` for an event fired at `tick`, unless a later one has.
    pub fn claim(&mut self, f: Field, tick: u64) -> bool {
        let newer = tick >= self[f];
        if newer {
            self.stamp(f, tick);
        }
        newer
    }

    pub fn stamp(&mut self, f: Field, tick: u64) {
        self.ticks[f as usize] = tick;
    }

    /// Stamps made in another boot are forgotten.
    pub fn forget_other_boot(&mut self) {
        if self.boot != procinfo::boot_id() {
            *self = Written::new(0);
        }
    }

    fn to_json(&self) -> Value {
        let mut o = Value::obj();
        for f in Field::ALL {
            o.set(f.name(), Value::num(self[f] as f64));
        }
        o.set("boot", Value::string(&self.boot));
        o
    }

    fn from_json(v: Option<&Value>) -> Written {
        let tick = |f: Field| v.and_then(|v| v.get(f.name())).and_then(Value::as_u64);
        Written {
            ticks: Field::ALL.map(|f| tick(f).unwrap_or(0)),
            boot: v
                .and_then(|v| v.get("boot"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }
}

impl std::ops::Index<Field> for Written {
    type Output = u64;
    fn index(&self, f: Field) -> &u64 {
        &self.ticks[f as usize]
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
    pub fn new(slot: &str, made: Moment) -> Self {
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
            last_activity_ms: made.at,
            last_attach_ms: 0,
            last_stop_ms: None,
            registered: true,
            updated_ms: made.at,
            last_event_ms: BTreeMap::new(),
            written: Written::new(made.tick),
        }
    }

    /// When an Esc ended the turn, given the transcript's trailing interrupt marker: the marker,
    /// if it is no older than the latest activity the hooks recorded (an Esc fires no hook).
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
        o.set("updated_ms", Value::num(self.updated_ms as f64));
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

/// Write a record, atomically: temp then rename, under the slot's lock, so a reader never sees
/// a half-written file and a killed writer's temp is reused by the next.
pub fn store(rec: &SlotRecord) -> std::io::Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.tmp", rec.slot));
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

    /// Every stamp distinct.
    fn stamped() -> Written {
        let mut w = Written::new(0);
        for (i, f) in Field::ALL.into_iter().enumerate() {
            w.stamp(f, i as u64 + 1);
        }
        w
    }

    #[test]
    fn stamps_from_another_boot_are_forgotten() {
        let mut kept = stamped();
        kept.forget_other_boot();
        assert_eq!(kept, stamped(), "calibration: this boot's stamps stay");
        let mut other = stamped();
        other.boot = "another".into();
        other.forget_other_boot();
        assert_eq!(other, Written::new(0));
    }

    #[test]
    fn a_record_survives_a_round_trip() {
        let mut rec = SlotRecord::new("claude-3", Moment::ms(1_000));
        rec.pid = Some(42);
        rec.proc_start = Some(99);
        rec.session_id = Some("abc".into());
        rec.title = Some("retire the old tunnel".into());
        rec.first_prompt = Some("move the tunnel to the new box".into());
        rec.written = stamped();
        rec.state = State::Offloaded;
        rec.needs_you = true;
        rec.last_stop_ms = Some(2_000);
        rec.last_event_ms.insert("Stop".into(), 2_000);

        let back = SlotRecord::from_json(&rec.to_json(), "wrong").expect("parses");
        assert_eq!(back.slot, "claude-3");
        assert_eq!(back.pid, Some(42));
        assert_eq!(back.proc_start, Some(99));
        assert_eq!(back.state, State::Offloaded);
        assert_eq!(back.first_prompt, rec.first_prompt);
        assert_eq!(back.written, rec.written);
        assert!(back.needs_you);
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
    }

    #[test]
    fn an_unknown_state_reads_as_live() {
        // The direction matters: calling a live slot offloaded would have the menu resume it
        // into a second process on the same conversation, which forks it.
        assert_eq!(State::parse("something-new"), State::Live);
    }

    #[test]
    fn a_row_shows_the_title_claude_codes_own_selector_would() {
        let mut rec = SlotRecord::new("claude-1", Moment::ms(0));
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
        let mut rec = SlotRecord::new("claude-1", Moment::ms(0));
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
        let mut rec = SlotRecord::new("claude-1", Moment::ms(0));
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
        let mut rec = SlotRecord::new("claude-1", Moment::ms(0));
        assert!(!rec.unread(), "nothing has finished yet");
        rec.last_stop_ms = Some(100);
        rec.last_attach_ms = 50;
        assert!(rec.unread(), "it finished after you last looked");
        rec.last_attach_ms = 150;
        assert!(!rec.unread(), "you have looked since");
    }
}
