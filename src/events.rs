//! The state machine: what each Claude Code hook event does to a slot's record.
//!
//! This module is the product. It is pure — a record in, a record out, a clock passed in — so
//! every row of the table in `docs/design.md` is a test rather than something you have to
//! reproduce by hand on a live session.
//!
//! Two rules that are not obvious from the table and cost a working session if missed:
//!
//! **`SessionEnd` with reason `clear` or `resume` must do nothing.** Both are followed by a
//! `SessionStart` in the SAME process. Treating either as an end marks a live slot closed, and
//! the menu then refuses to list the session you are sitting in.
//!
//! **A nested claude must never rebind the slot.** A `claude -p` from a Bash tool call, or a
//! subagent, inherits `CLAUDE_SESSIONS_SLOT` and fires the same hooks. Its events count as
//! *work running under* the slot, and nothing more.

use crate::clock::{Millis, Moment};
use crate::json::{self, Value};
use crate::lockfile;
use crate::registry::{Field, SlotRecord, State};
use std::time::Duration;

/// Whether the claude that fired this event is the slot's own process, or one nested under it.
/// Decided in `bind.rs` from `/proc`, never from the payload alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Own,
    Nested,
}

/// A hook payload, held as a JSON tree rather than parsed into a struct.
///
/// Deliberate: this is an interface that moves on its own, because Claude Code updates itself
/// in place in these containers. A new event type or a renamed field has to degrade to "no
/// evidence" rather than to a parse error on the ssh path, and a tree does that without a
/// struct full of `Option`s pretending to know the shape.
pub struct Event(pub Value);

impl Event {
    pub fn parse(body: &str) -> Result<Event, String> {
        json::parse(body).map(Event)
    }
    fn s(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }
    pub fn name(&self) -> &str {
        self.s("hook_event_name").unwrap_or("")
    }
    pub fn session_id(&self) -> Option<&str> {
        self.s("session_id")
    }
    pub fn cwd(&self) -> Option<&str> {
        self.s("cwd")
    }
    /// `SessionStart`: startup | resume | clear | compact | fork.
    pub fn source(&self) -> Option<&str> {
        self.s("source")
    }
    /// A `SessionStart` that leaves claude idle at its prompt. `compact` can fire mid-turn, and
    /// a start with no source is no evidence.
    pub fn opens_at_prompt(&self) -> bool {
        matches!(
            self.source(),
            Some("startup") | Some("resume") | Some("clear") | Some("fork")
        )
    }
    pub fn session_title(&self) -> Option<&str> {
        self.s("session_title")
    }
    /// Where Claude Code keeps this conversation's transcript, on every event.
    pub fn transcript_path(&self) -> Option<&str> {
        self.s("transcript_path")
    }
    pub fn prompt(&self) -> Option<&str> {
        self.s("prompt")
    }
    /// `SessionEnd`: clear | resume | logout | prompt_input_exit | other.
    pub fn reason(&self) -> Option<&str> {
        self.s("reason")
    }
    pub fn notification_type(&self) -> Option<&str> {
        self.s("notification_type")
    }
    /// Fired in a subagent's own context, which is a cheaper nested test than the `/proc` walk,
    /// though not a complete one: a `claude -p` from a Bash call carries no `agent_id`.
    pub fn fired_in_subagent(&self) -> bool {
        self.0.get("agent_id").is_some()
    }
}

/// What `apply` did, so the caller knows whether to write and `doctor` can say why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Changed,
    /// Deliberately nothing, with the reason — `idle_prompt`, a `clear` end, a late event.
    Ignored(&'static str),
}

/// The notification types that mean a person is needed.
///
/// `idle_prompt` is NOT one of them, and that is the single most important omission in this
/// file: it fires about a minute after every `Stop` nobody answers, so treating it as "needs
/// you" would put every unanswered session under Needs you, which then means nothing.
fn needs_you_type(t: &str) -> bool {
    matches!(
        t,
        "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input"
    )
}

/// How long the hook waits for the slot's lock. `SessionEnd` hooks share a 1.5 s budget. A
/// start at the slot's own prompt binds its process and waits out a stalled writer; any other
/// start holds up only claude's turn. A dropped `UserPromptSubmit` leaves a working claude
/// reading as idle (issue #1).
pub fn lock_wait(ev: &Event, binding: Binding) -> Duration {
    match ev.name() {
        "SessionEnd" => lockfile::SESSION_END_WAIT,
        "SessionStart" if binding != Binding::Own || !ev.opens_at_prompt() => {
            lockfile::INTERACTIVE_WAIT
        }
        _ => lockfile::HOOK_WAIT,
    }
}

/// Activity only moves forward: an event landing late does not make the slot look newer.
fn active(rec: &mut SlotRecord, at: Millis) {
    rec.last_activity_ms = rec.last_activity_ms.max(at);
}

/// A different conversation in the same slot (after /clear, a resume, a fork): what described
/// the old one must not describe the new one.
fn begin_conversation(rec: &mut SlotRecord, id: &str, tick: u64) {
    rec.session_id = Some(id.to_string());
    rec.title = None;
    rec.ai_title = None;
    rec.first_prompt = None;
    rec.transcript_path = None;
    rec.last_event_ms.clear();
    rec.written.stamp(Field::Conversation, tick);
}

/// Apply an event Claude Code fired at `fired` to a record. `pid`/`proc_start` describe the
/// slot's own claude, and are `None` when this event did not come from it.
pub fn apply(
    rec: &mut SlotRecord,
    ev: &Event,
    fired: Moment,
    binding: Binding,
    pid: Option<u32>,
    proc_start: Option<u64>,
) -> Outcome {
    let Moment { tick, at } = fired;
    // A nested claude is work, not a new identity, and its events keep the slot's activity
    // fresh.
    if binding == Binding::Nested {
        seen(rec, ev, at);
        active(rec, at);
        return Outcome::Changed;
    }

    if let Some(id) = ev
        .session_id()
        .filter(|id| rec.session_id.as_deref() != Some(id))
    {
        // A tie goes to the conversation on record: its start was synchronous, so nothing of
        // a newer one can have forked in the same tick.
        let started = rec.written[Field::Conversation];
        if tick < started || (tick == started && rec.session_id.is_some()) {
            return Outcome::Ignored("an event of an earlier conversation");
        }
        begin_conversation(rec, id, tick);
    }
    seen(rec, ev, at);

    // Every event from the slot's own claude names its current transcript; recorded so
    // "is there a conversation to resume" can be answered by the file existing (issue #5).
    if let Some(p) = ev.transcript_path() {
        rec.transcript_path = Some(p.to_string());
    }

    match ev.name() {
        "SessionStart" => {
            if let Some(cwd) = ev.cwd() {
                rec.cwd = Some(cwd.to_string());
            }
            if let Some(t) = ev.session_title() {
                rec.title = Some(t.to_string());
            }
            // Live even mid-offload: the process in front of the owner is the truth. A start
            // older than the slot's last start or end binds nothing.
            if rec.written.claim(Field::Life, tick) {
                rec.state = State::Live;
                if pid.is_some() {
                    // A keep-alive is the waiting process's, and a new process is not waiting.
                    if (pid, proc_start) != (rec.pid, rec.proc_start) {
                        rec.keep_until_ms = None;
                    }
                    rec.pid = pid;
                    rec.proc_start = proc_start;
                }
            }
            active(rec, at);
            if ev.opens_at_prompt() {
                if rec.written.claim(Field::Busy, tick) {
                    rec.busy = false;
                }
                if rec.written.claim(Field::NeedsYou, tick) {
                    rec.needs_you = false;
                }
            }
            Outcome::Changed
        }
        "UserPromptSubmit" => {
            // The earliest prompt fired, whichever lands first.
            let first = ev.prompt().and_then(one_line);
            if first.is_some() && (rec.first_prompt.is_none() || tick < rec.written[Field::Prompt])
            {
                rec.first_prompt = first;
                rec.written.stamp(Field::Prompt, tick);
            }
            active(rec, at);
            if rec.written.claim(Field::Busy, tick) {
                rec.busy = true;
            }
            if rec.written.claim(Field::NeedsYou, tick) {
                rec.needs_you = false;
            }
            Outcome::Changed
        }
        "Stop" => {
            active(rec, at);
            if rec.written.claim(Field::Busy, tick) {
                rec.busy = false;
                rec.last_stop_ms = Some(at);
            }
            Outcome::Changed
        }
        "Notification" => match ev.notification_type() {
            Some(t) if needs_you_type(t) => {
                if rec.written.claim(Field::NeedsYou, tick) {
                    rec.needs_you = true;
                    active(rec, at);
                    Outcome::Changed
                } else {
                    Outcome::Ignored("a newer prompt has landed")
                }
            }
            Some("idle_prompt") => {
                Outcome::Ignored("idle_prompt fires after every unanswered Stop")
            }
            _ => Outcome::Ignored("notification type does not mean a person is needed"),
        },
        "SessionEnd" => match ev.reason() {
            // Both are followed by a SessionStart in the same process.
            Some("clear") | Some("resume") => {
                Outcome::Ignored("clear and resume continue in the same process")
            }
            _ => {
                if !rec.written.claim(Field::Life, tick) {
                    return Outcome::Ignored("a newer start has landed");
                }
                rec.state = if rec.state == State::Offloading {
                    State::Offloaded
                } else {
                    State::Closed
                };
                if rec.written.claim(Field::Busy, tick) {
                    rec.busy = false;
                }
                Outcome::Changed
            }
        },
        _ => Outcome::Ignored("event not in the table"),
    }
}

/// When each event was last seen, for `doctor`.
fn seen(rec: &mut SlotRecord, ev: &Event, at: Millis) {
    let last = rec.last_event_ms.entry(ev.name().to_string()).or_insert(at);
    *last = (*last).max(at);
}

/// Titles read from the conversation's transcript (`transcript.rs`), applied after the event
/// itself so a new conversation's reset comes first. A title that was not found leaves what
/// is recorded alone: the tail of a transcript does not always reach back to one.
pub fn apply_titles(rec: &mut SlotRecord, titles: &crate::transcript::Titles) {
    if let Some(t) = &titles.custom {
        rec.title = Some(t.clone());
    }
    if let Some(t) = &titles.ai {
        rec.ai_title = Some(t.clone());
    }
}

/// The longest first prompt kept. The 80-column row has 69 columns of title, so this is
/// enough for any screen the menu draws, and keeps a pasted log out of the registry.
pub const FIRST_PROMPT_CHARS: usize = 120;

/// A prompt as a title: whitespace runs and control characters collapsed to single spaces,
/// cut at `FIRST_PROMPT_CHARS` on a character boundary. `None` when nothing printable is left.
pub fn one_line(prompt: &str) -> Option<String> {
    let words: Vec<&str> = prompt
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    let cut: String = joined.chars().take(FIRST_PROMPT_CHARS).collect();
    (!cut.is_empty()).then_some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(body: &str) -> Event {
        Event::parse(body).expect("the test payload is valid json")
    }
    /// A slot mid-session: bound, idle, nothing waiting.
    fn slot() -> SlotRecord {
        let mut r = SlotRecord::new("claude-1", Moment::ms(1_000));
        r.session_id = Some("first".into());
        r.pid = Some(100);
        r.proc_start = Some(7);
        r
    }
    fn own(rec: &mut SlotRecord, e: &Event, now: Millis) -> Outcome {
        apply(rec, e, Moment::ms(now), Binding::Own, Some(100), Some(7))
    }

    // ── hooks landing out of order ──────────────────────────────────────────
    // Each event is applied in the order it lands, with the time Claude Code fired it.

    const PROMPT: &str = r#"{"hook_event_name":"UserPromptSubmit","prompt":"go"}"#;
    const STOP: &str = r#"{"hook_event_name":"Stop"}"#;

    fn land(rec: &mut SlotRecord, events: &[(&str, Millis)]) -> Vec<Outcome> {
        events
            .iter()
            .map(|&(body, at)| own(rec, &ev(body), at))
            .collect()
    }

    #[test]
    fn a_prompt_landing_after_its_turns_stop_leaves_the_slot_idle() {
        let mut rec = slot();
        land(&mut rec, &[(PROMPT, 2_000), (STOP, 3_000)]);
        assert!(!rec.busy, "calibration: in order");
        let mut rec = slot();
        land(&mut rec, &[(STOP, 3_000), (PROMPT, 2_000)]);
        assert!(!rec.busy);
        assert_eq!(rec.last_stop_ms, Some(3_000));
        assert_eq!(rec.last_activity_ms, 3_000);
        assert_eq!(rec.first_prompt.as_deref(), Some("go"));
    }

    #[test]
    fn a_stop_landing_after_the_next_prompt_leaves_it_busy() {
        let mut rec = slot();
        land(&mut rec, &[(PROMPT, 2_000), (PROMPT, 5_000), (STOP, 3_000)]);
        assert!(rec.busy);
        assert_eq!(rec.last_stop_ms, None);
        land(&mut rec, &[(STOP, 6_000)]);
        assert!(!rec.busy, "calibration: the newer turn's own Stop ends it");
    }

    #[test]
    fn a_prompt_typed_mid_turn_landing_after_both_stops_leaves_it_idle() {
        // The second prompt fires while the first turn runs, and its own turn fires no prompt.
        let mut rec = slot();
        land(
            &mut rec,
            &[
                (PROMPT, 1_000),
                (STOP, 3_000),
                (STOP, 4_000),
                (PROMPT, 2_000),
            ],
        );
        assert!(!rec.busy);
        assert_eq!(rec.last_stop_ms, Some(4_000));
    }

    #[test]
    fn a_start_landing_after_the_first_prompt_leaves_it_busy() {
        let start = r#"{"hook_event_name":"SessionStart","source":"startup"}"#;
        let mut rec = slot();
        land(&mut rec, &[(PROMPT, 2_000), (start, 1_000)]);
        assert!(rec.busy);
        let mut rec = slot();
        land(&mut rec, &[(PROMPT, 2_000), (start, 2_500)]);
        assert!(!rec.busy, "calibration: a newer start is at its prompt");
    }

    #[test]
    fn the_previous_conversations_late_events_are_dropped() {
        let mut rec = slot();
        let clear = r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"second",
            "transcript_path":"/t/second.jsonl"}"#;
        let late =
            r#"{"hook_event_name":"Stop","session_id":"first","transcript_path":"/t/first.jsonl"}"#;
        let out = land(&mut rec, &[(clear, 5_000), (late, 3_000)]);
        assert!(matches!(out[1], Outcome::Ignored(_)));
        assert_eq!(rec.session_id.as_deref(), Some("second"));
        assert_eq!(rec.transcript_path.as_deref(), Some("/t/second.jsonl"));
        assert_eq!(rec.last_stop_ms, None);
        // Fired in the tick the new one started in: still the old conversation's.
        let mut tied = slot();
        assert!(matches!(
            land(&mut tied, &[(clear, 5_000), (late, 5_000)])[1],
            Outcome::Ignored(_)
        ));
        assert_eq!(tied.session_id.as_deref(), Some("second"));
        let mut fresh = SlotRecord::new("claude-1", Moment::ms(5_000));
        land(&mut fresh, &[(clear, 5_000)]);
        assert_eq!(
            fresh.session_id.as_deref(),
            Some("second"),
            "calibration: a new record adopts"
        );
        // Calibration: the same Stop from the current conversation is taken.
        let own_stop = late.replace(r#""session_id":"first""#, r#""session_id":"second""#);
        assert_eq!(land(&mut rec, &[(&own_stop, 6_000)])[0], Outcome::Changed);
        assert_eq!(rec.last_stop_ms, Some(6_000));
    }

    #[test]
    fn a_newer_event_of_an_unknown_conversation_adopts_it() {
        // Its SessionStart was lost: the slot follows the conversation it hears.
        let mut rec = slot();
        rec.title = Some("old".into());
        let other = r#"{"hook_event_name":"UserPromptSubmit","prompt":"new","session_id":"third"}"#;
        land(&mut rec, &[(other, 2_000)]);
        assert_eq!(rec.session_id.as_deref(), Some("third"));
        assert_eq!(rec.title, None);
        assert_eq!(rec.first_prompt.as_deref(), Some("new"));
        assert!(rec.busy);
    }

    #[test]
    fn a_permission_prompt_and_the_prompt_before_it_land_in_either_order() {
        let ask = r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#;
        let mut rec = slot();
        land(&mut rec, &[(ask, 2_000)]);
        assert!(rec.needs_you, "calibration");
        let mut rec = slot();
        land(&mut rec, &[(PROMPT, 3_000), (ask, 2_000)]);
        assert!(!rec.needs_you, "answered by the next prompt");
        let mut rec = slot();
        land(&mut rec, &[(ask, 3_000), (PROMPT, 2_000)]);
        assert!(rec.needs_you && rec.busy, "asked within its turn");
    }

    #[test]
    fn a_prompt_landing_after_the_end_leaves_it_idle() {
        let end = r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#;
        for order in [
            [(PROMPT, 2_000), (end, 3_000)],
            [(end, 3_000), (PROMPT, 2_000)],
        ] {
            let mut rec = slot();
            land(&mut rec, &order);
            assert!(!rec.busy, "{order:?}");
            assert_eq!(rec.state, State::Closed);
        }
    }

    #[test]
    fn only_a_start_that_binds_the_slot_waits_out_a_stalled_writer() {
        let wait = |body: &str, binding| lock_wait(&ev(body), binding);
        let start =
            |source: &str| format!(r#"{{"hook_event_name":"SessionStart","source":"{source}"}}"#);
        assert_eq!(
            wait(r#"{"hook_event_name":"SessionEnd"}"#, Binding::Own),
            lockfile::SESSION_END_WAIT
        );
        for source in ["startup", "resume", "clear", "fork"] {
            assert_eq!(
                wait(&start(source), Binding::Own),
                lockfile::HOOK_WAIT,
                "{source}"
            );
            assert_eq!(
                wait(&start(source), Binding::Nested),
                lockfile::INTERACTIVE_WAIT
            );
        }
        assert_eq!(
            wait(&start("compact"), Binding::Own),
            lockfile::INTERACTIVE_WAIT
        );
        assert_eq!(
            wait(r#"{"hook_event_name":"SessionStart"}"#, Binding::Own),
            lockfile::INTERACTIVE_WAIT
        );
        assert_eq!(wait(PROMPT, Binding::Nested), lockfile::HOOK_WAIT);
        assert_eq!(wait(STOP, Binding::Own), lockfile::HOOK_WAIT);
    }

    #[test]
    fn an_end_landing_after_a_newer_start_leaves_it_live() {
        let start = r#"{"hook_event_name":"SessionStart","source":"startup"}"#;
        let end = r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#;
        let mut rec = slot();
        land(&mut rec, &[(start, 5_000), (end, 4_000)]);
        assert_eq!(rec.state, State::Live);
        land(&mut rec, &[(end, 6_000)]);
        assert_eq!(rec.state, State::Closed, "calibration");
    }

    #[test]
    fn events_fired_in_the_same_tick_both_apply() {
        let mut rec = slot();
        land(&mut rec, &[(PROMPT, 2_000), (STOP, 2_000)]);
        assert!(!rec.busy);
        assert_eq!(rec.last_stop_ms, Some(2_000));
    }

    #[test]
    fn a_killed_process_late_events_do_not_reach_its_resumed_slot() {
        let resume = r#"{"hook_event_name":"SessionStart","source":"resume","session_id":"first"}"#;
        let stop = r#"{"hook_event_name":"Stop","session_id":"first"}"#;
        let ask = r#"{"hook_event_name":"Notification","notification_type":"permission_prompt",
            "session_id":"first"}"#;
        let mut rec = slot();
        apply(
            &mut rec,
            &ev(resume),
            Moment::ms(9_000),
            Binding::Own,
            Some(200),
            Some(8),
        );
        land(&mut rec, &[(stop, 3_000), (ask, 4_000)]);
        assert_eq!(rec.last_stop_ms, None);
        assert!(!rec.needs_you && !rec.busy);
        assert_eq!(rec.state, State::Live);
        // Read /proc after the process had gone: no claude above, so nested.
        for body in [stop, ask] {
            apply(
                &mut rec,
                &ev(body),
                Moment::ms(5_000),
                Binding::Nested,
                None,
                None,
            );
        }
        assert_eq!(rec.last_activity_ms, 9_000, "idle since the resume, still");
        // A late start of the old process binds nothing.
        let compact =
            r#"{"hook_event_name":"SessionStart","source":"compact","session_id":"first"}"#;
        apply(
            &mut rec,
            &ev(compact),
            Moment::ms(5_000),
            Binding::Own,
            Some(100),
            Some(7),
        );
        assert_eq!((rec.pid, rec.proc_start), (Some(200), Some(8)));
        apply(
            &mut rec,
            &ev(compact),
            Moment::ms(9_500),
            Binding::Own,
            Some(300),
            Some(9),
        );
        assert_eq!(rec.pid, Some(300), "calibration: a newer start binds");
        land(&mut rec, &[(stop, 10_000), (ask, 11_000)]);
        assert!(
            rec.last_stop_ms == Some(10_000) && rec.needs_you,
            "calibration: newer ones are taken"
        );
    }

    #[test]
    fn the_first_prompt_is_the_earliest_fired() {
        let said =
            |text: &str| format!(r#"{{"hook_event_name":"UserPromptSubmit","prompt":"{text}"}}"#);
        let (one, two) = (said("one"), said("two"));
        let mut rec = slot();
        land(&mut rec, &[(&one, 2_000), (&two, 3_000)]);
        assert_eq!(rec.first_prompt.as_deref(), Some("one"), "calibration");
        let mut rec = slot();
        land(&mut rec, &[(&two, 3_000), (&one, 2_000)]);
        assert_eq!(rec.first_prompt.as_deref(), Some("one"));
    }

    #[test]
    fn a_new_record_refuses_what_fired_before_it() {
        // The menu reuses a closed slot's name with a fresh record; the old claude's last hook
        // can land after it.
        let mut rec = SlotRecord::new("claude-1", Moment::ms(5_000));
        let old = r#"{"hook_event_name":"Notification","notification_type":"permission_prompt",
            "session_id":"before"}"#;
        assert!(matches!(
            land(&mut rec, &[(old, 3_000)])[0],
            Outcome::Ignored(_)
        ));
        assert_eq!(rec.session_id, None);
        assert!(!rec.needs_you);
        let start = r#"{"hook_event_name":"SessionStart","source":"startup","session_id":"new"}"#;
        land(&mut rec, &[(start, 6_000)]);
        assert_eq!(rec.session_id.as_deref(), Some("new"), "calibration");
    }

    #[test]
    fn an_event_fired_inside_an_agent_says_so() {
        let inside = ev(r#"{"hook_event_name":"Notification","agent_id":"a1"}"#);
        assert!(inside.fired_in_subagent());
        assert!(!ev(STOP).fired_in_subagent(), "calibration");
    }

    #[test]
    fn clear_does_not_close_the_slot_and_the_new_conversation_binds() {
        let mut rec = slot();
        let out = own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionEnd","reason":"clear"}"#),
            2_000,
        );
        assert!(matches!(out, Outcome::Ignored(_)), "clear is not an end");
        assert_eq!(rec.state, State::Live);
        assert_eq!(rec.session_id.as_deref(), Some("first"));

        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"second"}"#),
            2_100,
        );
        assert_eq!(rec.state, State::Live);
        assert_eq!(
            rec.session_id.as_deref(),
            Some("second"),
            "same slot, new conversation"
        );
    }

    #[test]
    fn a_new_conversation_starts_its_event_times_afresh() {
        // Infra, 2026-10-03: a `UserPromptSubmit` from the conversation before read as this
        // one having been prompted.
        let mut rec = slot();
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"x"}"#),
            1_500,
        );
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"second"}"#),
            2_000,
        );
        assert_eq!(rec.last_event_ms.get("UserPromptSubmit"), None);
        assert_eq!(rec.last_event_ms.get("SessionStart"), Some(&2_000));
        assert_eq!(rec.last_event_ms.len(), 1);
        // Calibration: a start of the same conversation (compaction) keeps them.
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"y"}"#),
            2_500,
        );
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"compact","session_id":"second"}"#),
            3_000,
        );
        assert_eq!(rec.last_event_ms.get("UserPromptSubmit"), Some(&2_500));
    }

    #[test]
    fn resume_does_not_close_the_slot_either() {
        // The one that bit the design before it was written down: SessionEnd carries `resume`
        // as well as `clear`, and treating it as an end marks the session you just resumed
        // closed.
        let mut rec = slot();
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionEnd","reason":"resume"}"#),
            2_000,
        );
        assert_eq!(rec.state, State::Live);
    }

    #[test]
    fn a_keepalive_is_the_process_that_asked_for_it() {
        let start = |source: &str| {
            ev(&format!(
                r#"{{"hook_event_name":"SessionStart","source":"{source}","session_id":"first"}}"#
            ))
        };
        let mut rec = slot();
        rec.keep_until_ms = Some(60_000);
        own(&mut rec, &start("clear"), 2_000);
        assert_eq!(
            rec.keep_until_ms,
            Some(60_000),
            "calibration: the same process"
        );
        apply(
            &mut rec,
            &start("resume"),
            Moment::ms(3_000),
            Binding::Own,
            Some(200),
            Some(8),
        );
        assert_eq!(rec.keep_until_ms, None, "a new process asked for nothing");
    }

    #[test]
    fn a_start_at_the_prompt_is_idle_and_a_compaction_changes_nothing_about_it() {
        for source in ["startup", "resume", "clear", "fork"] {
            let mut rec = slot();
            rec.busy = true;
            let body = format!(r#"{{"hook_event_name":"SessionStart","source":"{source}"}}"#);
            own(&mut rec, &ev(&body), 2_000);
            assert!(!rec.busy, "{source}: nothing is running yet");
        }
        // A compaction mid-turn: the claude is still working, and must keep reading so.
        let mut rec = slot();
        rec.busy = true;
        rec.needs_you = false;
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"compact"}"#),
            3_000,
        );
        assert!(rec.busy, "a compaction does not end a turn");
        assert_eq!(rec.last_activity_ms, 3_000, "it is activity all the same");
        // No source at all: no evidence of idleness either.
        let mut rec = slot();
        rec.busy = true;
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart"}"#),
            4_000,
        );
        assert!(rec.busy);
    }

    #[test]
    fn a_real_end_closes_the_slot() {
        let mut rec = slot();
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#),
            2_000,
        );
        assert_eq!(rec.state, State::Closed);
    }

    #[test]
    fn an_end_while_offloading_means_offloaded_not_closed() {
        // This is why `offloading` is written before the signal rather than after it: the
        // SessionEnd the kill provokes is indistinguishable from a person typing /exit.
        let mut rec = slot();
        rec.state = State::Offloading;
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionEnd","reason":"other"}"#),
            2_000,
        );
        assert_eq!(rec.state, State::Offloaded);
    }

    #[test]
    fn a_start_arriving_mid_offload_wins() {
        let mut rec = slot();
        rec.state = State::Offloading;
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"resume"}"#),
            2_000,
        );
        assert_eq!(
            rec.state,
            State::Live,
            "the process in front of the owner is the truth"
        );
    }

    // ── a nested claude is work, not an identity ─────────────────────────────

    #[test]
    fn a_nested_claude_never_rebinds_the_slot() {
        let mut rec = slot();
        let before = rec.session_id.clone();
        let out = apply(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","session_id":"subagent","agent_id":"a1"}"#),
            Moment::ms(5_000),
            Binding::Nested,
            None,
            None,
        );
        assert_eq!(out, Outcome::Changed, "it still counts as activity");
        assert_eq!(
            rec.session_id, before,
            "the slot's conversation is untouched"
        );
        assert_eq!(rec.last_activity_ms, 5_000);
    }

    #[test]
    fn a_nested_claude_exiting_does_not_close_the_slot() {
        // The failure this prevents: a `claude -p` from a Bash call ends, and the slot the
        // owner is sitting in disappears from the menu.
        let mut rec = slot();
        apply(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionEnd","reason":"other"}"#),
            Moment::ms(5_000),
            Binding::Nested,
            None,
            None,
        );
        assert_eq!(rec.state, State::Live);
    }

    // ── attention ────────────────────────────────────────────────────────────

    #[test]
    fn an_idle_prompt_notification_changes_nothing() {
        // It fires about a minute after every Stop nobody answers. Treating it as "needs you"
        // would put every unanswered session under Needs you.
        let mut rec = slot();
        let out = own(
            &mut rec,
            &ev(r#"{"hook_event_name":"Notification","notification_type":"idle_prompt"}"#),
            9_000,
        );
        assert!(matches!(out, Outcome::Ignored(_)));
        assert!(!rec.needs_you);
        assert_eq!(rec.last_activity_ms, 1_000, "not even activity");
    }

    #[test]
    fn the_three_notifications_that_mean_a_person_is_needed() {
        for t in [
            "permission_prompt",
            "elicitation_dialog",
            "agent_needs_input",
        ] {
            let mut rec = slot();
            own(
                &mut rec,
                &ev(&format!(
                    r#"{{"hook_event_name":"Notification","notification_type":"{t}"}}"#
                )),
                9_000,
            );
            assert!(rec.needs_you, "{t} should mean needs you");
        }
        for t in ["auth_success", "agent_completed", "quota_auto_resume_fired"] {
            let mut rec = slot();
            own(
                &mut rec,
                &ev(&format!(
                    r#"{{"hook_event_name":"Notification","notification_type":"{t}"}}"#
                )),
                9_000,
            );
            assert!(!rec.needs_you, "{t} should not");
        }
    }

    #[test]
    fn answering_the_prompt_clears_it_and_a_stop_makes_the_slot_unread() {
        let mut rec = slot();
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#),
            2_000,
        );
        assert!(rec.needs_you);
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit"}"#),
            3_000,
        );
        assert!(!rec.needs_you);
        assert!(rec.busy);
        own(&mut rec, &ev(r#"{"hook_event_name":"Stop"}"#), 4_000);
        assert!(!rec.busy);
        assert_eq!(rec.last_stop_ms, Some(4_000));
        assert!(rec.unread(), "it finished while you were away");
    }

    #[test]
    fn an_event_this_version_has_never_heard_of_is_ignored_but_recorded() {
        let mut rec = slot();
        let out = own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SomethingNew2027"}"#),
            4_000,
        );
        assert!(matches!(out, Outcome::Ignored(_)));
        assert_eq!(
            rec.last_event_ms.get("SomethingNew2027"),
            Some(&4_000),
            "doctor can still say it arrived"
        );
    }

    #[test]
    fn every_event_records_when_it_was_last_seen() {
        // This is what makes a hook that quietly stops being delivered visible: `doctor`
        // prints the age of each event, and a Stop last seen in August is the symptom.
        let mut rec = slot();
        own(&mut rec, &ev(r#"{"hook_event_name":"Stop"}"#), 7_000);
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit"}"#),
            8_000,
        );
        assert_eq!(rec.last_event_ms.get("Stop"), Some(&7_000));
        assert_eq!(rec.last_event_ms.get("UserPromptSubmit"), Some(&8_000));
        own(&mut rec, &ev(r#"{"hook_event_name":"Stop"}"#), 6_000);
        assert_eq!(
            rec.last_event_ms.get("Stop"),
            Some(&7_000),
            "a late one moves nothing back"
        );
    }

    // ── titles: the first prompt, and a new conversation forgetting the old one ─

    #[test]
    fn the_first_prompt_is_kept_on_one_line_and_later_ones_are_not() {
        let mut rec = slot();
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"  fix the\n\ttunnel  "}"#),
            2_000,
        );
        assert_eq!(rec.first_prompt.as_deref(), Some("fix the tunnel"));
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"and then the dns"}"#),
            3_000,
        );
        assert_eq!(
            rec.first_prompt.as_deref(),
            Some("fix the tunnel"),
            "it is the FIRST prompt"
        );
    }

    #[test]
    fn a_long_or_empty_prompt_is_cut_or_skipped() {
        let mut rec = slot();
        let long = "é".repeat(FIRST_PROMPT_CHARS + 50);
        let body = format!(r#"{{"hook_event_name":"UserPromptSubmit","prompt":"{long}"}}"#);
        own(&mut rec, &ev(&body), 2_000);
        assert_eq!(
            rec.first_prompt.as_ref().map(|p| p.chars().count()),
            Some(FIRST_PROMPT_CHARS),
            "cut on a character boundary, multibyte included"
        );
        let mut rec = slot();
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":" \n "}"#),
            2_000,
        );
        assert_eq!(rec.first_prompt, None, "nothing printable is no title");
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"now a real one"}"#),
            3_000,
        );
        assert_eq!(rec.first_prompt.as_deref(), Some("now a real one"));
    }

    #[test]
    fn a_new_conversation_does_not_inherit_the_old_ones_title() {
        let mut rec = slot();
        rec.title = Some("old task".into());
        rec.first_prompt = Some("old prompt".into());
        // Calibration: a SessionStart for the SAME conversation (compact) keeps both.
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"compact","session_id":"first"}"#),
            2_000,
        );
        assert_eq!(rec.title.as_deref(), Some("old task"));
        assert_eq!(rec.first_prompt.as_deref(), Some("old prompt"));
        // /clear: a new conversation in the same slot.
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"second"}"#),
            3_000,
        );
        assert_eq!(rec.title, None);
        assert_eq!(rec.first_prompt, None);
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"new task"}"#),
            4_000,
        );
        assert_eq!(rec.first_prompt.as_deref(), Some("new task"));
    }

    #[test]
    fn titles_from_the_transcript_land_and_a_new_conversation_drops_them() {
        use crate::transcript::Titles;
        let mut rec = slot();
        apply_titles(
            &mut rec,
            &Titles {
                custom: None,
                ai: Some("Retire the old tunnel".into()),
            },
        );
        assert_eq!(rec.ai_title.as_deref(), Some("Retire the old tunnel"));
        // A tail that found no title leaves the recorded one alone.
        apply_titles(&mut rec, &Titles::default());
        assert_eq!(rec.ai_title.as_deref(), Some("Retire the old tunnel"));
        // /clear: a new conversation must not wear the old one's generated title.
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"second"}"#),
            2_000,
        );
        assert_eq!(rec.ai_title, None);
    }

    #[test]
    fn a_nested_claudes_prompt_is_not_the_slots_first_prompt() {
        let mut rec = slot();
        apply(
            &mut rec,
            &ev(r#"{"hook_event_name":"UserPromptSubmit","prompt":"subagent work"}"#),
            Moment::ms(2_000),
            Binding::Nested,
            None,
            None,
        );
        assert_eq!(rec.first_prompt, None);
    }
}
