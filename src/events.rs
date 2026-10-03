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
//! *work running under* the slot — enough to keep the offloader off it — and nothing more.

use crate::clock::Millis;
use crate::json::{self, Value};
use crate::registry::{SlotRecord, State, Timer};

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
    pub fn tool_name(&self) -> Option<&str> {
        self.s("tool_name")
    }
    pub fn tool_input(&self) -> Option<&Value> {
        self.0.get("tool_input")
    }
    pub fn tool_response(&self) -> Option<&Value> {
        self.0.get("tool_response")
    }
    /// Present on subagents only, which is a cheaper nested test than the `/proc` walk — but
    /// not a complete one: a `claude -p` from a Bash call carries no such field.
    pub fn has_agent_id(&self) -> bool {
        self.0.get("agent_id").is_some()
    }
}

/// What `apply` did, so the caller knows whether to write and `doctor` can say why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Changed,
    /// Deliberately nothing, with the reason — `idle_prompt`, a `clear` end, an unknown tool.
    Ignored(&'static str),
}

/// The notification types that mean a person is needed.
///
/// `idle_prompt` is NOT one of them, and that is the single most important omission in this
/// file: it fires about a minute after every `Stop` nobody answers, so treating it as "needs
/// you" would pin every detached session open forever and the offloader would never run again.
fn needs_you_type(t: &str) -> bool {
    matches!(
        t,
        "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input"
    )
}

/// Apply an event to a record. `pid`/`proc_start` describe the slot's own claude, and are
/// `None` when this event did not come from it.
pub fn apply(
    rec: &mut SlotRecord,
    ev: &Event,
    now: Millis,
    binding: Binding,
    pid: Option<u32>,
    proc_start: Option<u64>,
) -> Outcome {
    rec.last_event_ms.insert(ev.name().to_string(), now);

    // A nested claude is work, not a new identity. Keeping the slot's activity fresh is the
    // whole point: a subagent grinding away for twenty minutes must not look idle to the
    // offloader.
    if binding == Binding::Nested {
        rec.last_activity_ms = now;
        rec.updated_ms = now;
        return Outcome::Changed;
    }

    // Every event from the slot's own claude names its current transcript; recorded so
    // "is there a conversation to resume" can be answered by the file existing (issue #5).
    if let Some(p) = ev.transcript_path() {
        rec.transcript_path = Some(p.to_string());
    }

    let out = match ev.name() {
        "SessionStart" => {
            if let Some(id) = ev.session_id() {
                // A different conversation in the same slot — after /clear, a resume, a fork.
                // What described the old one must not describe the new one: the title and the
                // first prompt start again, rather than the menu showing last task's name on
                // this task's row.
                if rec.session_id.as_deref() != Some(id) {
                    rec.title = None;
                    rec.ai_title = None;
                    rec.first_prompt = None;
                    // The new conversation's own path, if this event carried one, was set above.
                    if ev.transcript_path().is_none() {
                        rec.transcript_path = None;
                    }
                }
                rec.session_id = Some(id.to_string());
            }
            if let Some(cwd) = ev.cwd() {
                rec.cwd = Some(cwd.to_string());
            }
            if let Some(t) = ev.session_title() {
                rec.title = Some(t.to_string());
            }
            if pid.is_some() {
                rec.pid = pid;
                rec.proc_start = proc_start;
            }
            // A start always means live, including one arriving mid-offload: the owner opened
            // it faster than the offloader could stop it, and the process in front of them is
            // the truth.
            rec.state = State::Live;
            rec.last_activity_ms = now;
            // A start that opens a conversation leaves claude at its prompt, waiting: "you can
            // type right away" (vendor hook docs, read 2026-10-03). That is idle, as after a
            // Stop, so a slot opened and then left is offloadable like any other. `compact` is
            // NOT one of these: auto-compaction can happen in the middle of a turn, the docs
            // do not say otherwise, and a compaction must not make a working claude read as
            // done — so it leaves `busy`, `needs_you` and readiness exactly as they were. A
            // start with no source at all is treated the same way: no evidence of idleness.
            if matches!(
                ev.source(),
                Some("startup") | Some("resume") | Some("clear") | Some("fork")
            ) {
                rec.busy = false;
                rec.needs_you = false;
                rec.ready_ms = Some(now);
            }
            Outcome::Changed
        }
        "UserPromptSubmit" => {
            if rec.first_prompt.is_none() {
                rec.first_prompt = ev.prompt().and_then(one_line);
            }
            rec.busy = true;
            rec.needs_you = false;
            rec.last_activity_ms = now;
            Outcome::Changed
        }
        "Stop" => {
            rec.busy = false;
            rec.last_activity_ms = now;
            rec.last_stop_ms = Some(now);
            Outcome::Changed
        }
        "Notification" => match ev.notification_type() {
            Some(t) if needs_you_type(t) => {
                rec.needs_you = true;
                rec.last_activity_ms = now;
                Outcome::Changed
            }
            Some("idle_prompt") => {
                Outcome::Ignored("idle_prompt fires after every unanswered Stop")
            }
            _ => Outcome::Ignored("notification type does not mean a person is needed"),
        },
        "PostToolUse" => apply_timer(rec, ev, now),
        "SessionEnd" => match ev.reason() {
            // Both are followed by a SessionStart in the same process.
            Some("clear") | Some("resume") => {
                Outcome::Ignored("clear and resume continue in the same process")
            }
            _ => {
                rec.state = if rec.state == State::Offloading {
                    State::Offloaded
                } else {
                    State::Closed
                };
                rec.busy = false;
                Outcome::Changed
            }
        },
        _ => Outcome::Ignored("event not in the table"),
    };

    if out == Outcome::Changed {
        rec.updated_ms = now;
    }
    out
}

/// The tools whose `PostToolUse` sets or clears a timer. `hooks_config` installs the
/// `PostToolUse` hook for exactly these, and its tests check each one really is handled below.
pub const TIMER_TOOLS: [&str; 3] = ["ScheduleWakeup", "CronCreate", "CronDelete"];

/// Timers, from `PostToolUse` on the tools that set them.
///
/// `CronList` is not here and must not be: it reads timers, it does not create one. Nor is
/// `TaskStop`, which stops a background task rather than a timer. A slot pinned open by a
/// listing would never be offloadable again.
fn apply_timer(rec: &mut SlotRecord, ev: &Event, now: Millis) -> Outcome {
    let input = ev.tool_input();
    match ev.tool_name().unwrap_or("") {
        "ScheduleWakeup" => {
            let stop = input
                .and_then(|v| v.get("stop"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if stop {
                let before = rec.timers.len();
                rec.timers.retain(|t| t.id != "wakeup");
                return if rec.timers.len() == before {
                    Outcome::Ignored("no wake-up to stop")
                } else {
                    Outcome::Changed
                };
            }
            let delay = input
                .and_then(|v| v.get("delaySeconds"))
                .and_then(Value::as_f64);
            // No delay means a payload shape this version does not know. Record the timer with
            // no due time rather than guessing one: an unknown due date counts as pending,
            // which errs towards keeping the session alive.
            let due = delay.map(|d| now + (d.max(0.0) * 1000.0) as Millis);
            upsert(
                rec,
                Timer {
                    id: "wakeup".into(),
                    due_ms: due,
                    recurring: false,
                },
            );
            Outcome::Changed
        }
        "CronCreate" => {
            let id = ev
                .tool_response()
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
                .or_else(|| input.and_then(|v| v.get("name")).and_then(Value::as_str))
                .unwrap_or("cron")
                .to_string();
            upsert(
                rec,
                Timer {
                    id: format!("cron:{id}"),
                    due_ms: None,
                    recurring: true,
                },
            );
            Outcome::Changed
        }
        "CronDelete" => {
            let id = input
                .and_then(|v| v.get("id").or_else(|| v.get("name")))
                .and_then(Value::as_str);
            let before = rec.timers.len();
            match id {
                Some(id) => {
                    let key = format!("cron:{id}");
                    rec.timers.retain(|t| t.id != key);
                }
                // A delete whose target we cannot read is still a delete, and dropping every
                // cron is the safe direction: the alternative is a slot pinned open forever by
                // a timer that no longer exists.
                None => rec.timers.retain(|t| !t.recurring),
            }
            if rec.timers.len() == before {
                Outcome::Ignored("no such timer")
            } else {
                Outcome::Changed
            }
        }
        _ => Outcome::Ignored("tool does not set a timer"),
    }
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

fn upsert(rec: &mut SlotRecord, t: Timer) {
    if let Some(existing) = rec.timers.iter_mut().find(|e| e.id == t.id) {
        *existing = t;
    } else {
        rec.timers.push(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(body: &str) -> Event {
        Event::parse(body).expect("the test payload is valid json")
    }
    /// A slot mid-session: bound, idle, nothing waiting.
    fn slot() -> SlotRecord {
        let mut r = SlotRecord::new("claude-1", 1_000);
        r.session_id = Some("first".into());
        r.pid = Some(100);
        r.proc_start = Some(7);
        r
    }
    fn own(rec: &mut SlotRecord, e: &Event, now: Millis) -> Outcome {
        apply(rec, e, now, Binding::Own, Some(100), Some(7))
    }

    // ── /clear and resume: the two ends that are not ends ────────────────────

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
    fn a_start_at_the_prompt_is_ready_and_a_compaction_changes_nothing_about_it() {
        for source in ["startup", "resume", "clear", "fork"] {
            let mut rec = slot();
            rec.busy = true;
            let body = format!(r#"{{"hook_event_name":"SessionStart","source":"{source}"}}"#);
            own(&mut rec, &ev(&body), 2_000);
            assert_eq!(rec.ready_ms, Some(2_000), "{source} opens at the prompt");
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
        assert_eq!(rec.ready_ms, None, "nor does it leave claude at its prompt");
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
        assert_eq!(rec.ready_ms, None);
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
            5_000,
            Binding::Nested,
            None,
            None,
        );
        assert_eq!(out, Outcome::Changed, "it still counts as activity");
        assert_eq!(
            rec.session_id, before,
            "the slot's conversation is untouched"
        );
        assert_eq!(
            rec.last_activity_ms, 5_000,
            "so the offloader leaves it alone"
        );
    }

    #[test]
    fn a_nested_claude_exiting_does_not_close_the_slot() {
        // The failure this prevents: a `claude -p` from a Bash call ends, and the slot the
        // owner is sitting in disappears from the menu.
        let mut rec = slot();
        apply(
            &mut rec,
            &ev(r#"{"hook_event_name":"SessionEnd","reason":"other"}"#),
            5_000,
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
        // would pin every detached session open and the offloader would never run again.
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

    // ── timers ───────────────────────────────────────────────────────────────

    #[test]
    fn a_wake_up_becomes_a_pending_timer_and_stopping_it_removes_it() {
        let mut rec = slot();
        own(
            &mut rec,
            &ev(
                r#"{"hook_event_name":"PostToolUse","tool_name":"ScheduleWakeup","tool_input":{"delaySeconds":1800}}"#,
            ),
            10_000,
        );
        assert_eq!(rec.timers.len(), 1);
        assert_eq!(rec.timers[0].due_ms, Some(10_000 + 1_800_000));
        assert!(rec.has_pending_timer(20_000));

        own(
            &mut rec,
            &ev(
                r#"{"hook_event_name":"PostToolUse","tool_name":"ScheduleWakeup","tool_input":{"stop":true}}"#,
            ),
            11_000,
        );
        assert!(
            rec.timers.is_empty(),
            "a stopped loop no longer pins the slot"
        );
    }

    #[test]
    fn a_second_wake_up_replaces_the_first_rather_than_stacking() {
        let mut rec = slot();
        for (now, delay) in [(1_000u64, 60.0), (2_000, 120.0)] {
            own(
                &mut rec,
                &ev(&format!(
                    r#"{{"hook_event_name":"PostToolUse","tool_name":"ScheduleWakeup","tool_input":{{"delaySeconds":{delay}}}}}"#
                )),
                now,
            );
        }
        assert_eq!(rec.timers.len(), 1, "one loop, one timer");
        assert_eq!(rec.timers[0].due_ms, Some(2_000 + 120_000));
    }

    #[test]
    fn a_wake_up_with_an_unreadable_shape_still_pins_the_slot() {
        let mut rec = slot();
        own(
            &mut rec,
            &ev(
                r#"{"hook_event_name":"PostToolUse","tool_name":"ScheduleWakeup","tool_input":{"somethingNew":1}}"#,
            ),
            10_000,
        );
        assert_eq!(rec.timers[0].due_ms, None);
        assert!(
            rec.has_pending_timer(u64::MAX),
            "an unknown due date must keep the session alive, not expose it to the offloader"
        );
    }

    #[test]
    fn a_cron_is_recurring_and_is_removed_by_id() {
        let mut rec = slot();
        own(
            &mut rec,
            &ev(
                r#"{"hook_event_name":"PostToolUse","tool_name":"CronCreate","tool_input":{"name":"nightly"},"tool_response":{"id":"cr_7"}}"#,
            ),
            1_000,
        );
        assert_eq!(
            rec.timers[0].id, "cron:cr_7",
            "the response's id wins over the name"
        );
        assert!(
            rec.has_pending_timer(u64::MAX),
            "a recurring timer never expires"
        );

        own(
            &mut rec,
            &ev(
                r#"{"hook_event_name":"PostToolUse","tool_name":"CronDelete","tool_input":{"id":"cr_7"}}"#,
            ),
            2_000,
        );
        assert!(rec.timers.is_empty());
    }

    #[test]
    fn a_delete_we_cannot_read_drops_every_cron() {
        // The safe direction: a slot pinned open forever by a timer that no longer exists is
        // worse than losing the mark on one that does.
        let mut rec = slot();
        rec.timers.push(Timer {
            id: "cron:a".into(),
            due_ms: None,
            recurring: true,
        });
        rec.timers.push(Timer {
            id: "wakeup".into(),
            due_ms: Some(9_000),
            recurring: false,
        });
        own(
            &mut rec,
            &ev(r#"{"hook_event_name":"PostToolUse","tool_name":"CronDelete","tool_input":{}}"#),
            2_000,
        );
        assert_eq!(rec.timers.len(), 1, "the crons went, the wake-up stayed");
        assert_eq!(rec.timers[0].id, "wakeup");
    }

    #[test]
    fn reading_or_stopping_something_is_not_a_timer() {
        // CronList reads timers and TaskStop stops a background task. A slot pinned open by a
        // listing would never be offloadable again.
        for tool in ["CronList", "TaskStop", "TaskOutput", "Bash"] {
            let mut rec = slot();
            let out = own(
                &mut rec,
                &ev(&format!(
                    r#"{{"hook_event_name":"PostToolUse","tool_name":"{tool}"}}"#
                )),
                1_000,
            );
            assert!(
                matches!(out, Outcome::Ignored(_)),
                "{tool} must not set a timer"
            );
            assert!(rec.timers.is_empty());
        }
    }

    // ── tolerance ────────────────────────────────────────────────────────────

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
            2_000,
            Binding::Nested,
            None,
            None,
        );
        assert_eq!(rec.first_prompt, None);
    }
}
