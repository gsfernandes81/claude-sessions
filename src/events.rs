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
    pub fn session_title(&self) -> Option<&str> {
        self.s("session_title")
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

    let out = match ev.name() {
        "SessionStart" => {
            if let Some(id) = ev.session_id() {
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
            rec.busy = false;
            rec.needs_you = false;
            rec.last_activity_ms = now;
            Outcome::Changed
        }
        "UserPromptSubmit" => {
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
}
