//! The Claude Code settings that feed the registry, printed by the binary that consumes them —
//! the hooks, and the status line `statusline` draws.
//!
//! `infra` writes this to `/etc/claude-code/managed-settings.d/claude-sessions.json` at image
//! build — see `docs/design.md` § *Where the hooks live* for why a managed drop-in. It is
//! generated rather than kept as a file beside the code because the two halves have to agree:
//! an event installed here that the state machine ignores is noise, and an event the state
//! machine handles that is not installed here is a registry that silently never hears it.
//! The tests below hold that agreement in place.
//!
//! Shapes and limits from `code.claude.com/docs/en/hooks`, read 2026-10-02:
//!
//! - **A matcher of letters and `|` is an exact list**, not a regex, so the `PostToolUse`
//!   matcher names exactly the timer tools and nothing that merely contains their names.
//! - **Every event but `SessionStart` and `SessionEnd` is `async`**: Claude Code neither waits
//!   for the hook nor times it out, so a stalled disk delays the event, not the prompt. Async
//!   hooks of one slot can land out of order, which `events` handles.
//! - **`SessionStart` and `SessionEnd` are synchronous**: claude's first reply waits for the
//!   first anyway, and the second must finish before claude exits.
//! - **`timeout` is seconds, and only a synchronous hook has one.** `SessionEnd` hooks share a
//!   1.5 s budget that a longer timeout *raises*, slowing every `/exit` on the box, so ours is 1
//!   and its lock wait 400 ms. `SessionStart`'s is above the hook's own lock wait: a stalled
//!   disk then costs the first reply seconds rather than the slot its binding.
//! - **No matcher on the other events**: `SessionStart` must see every source, including
//!   `clear` and `fork`, and the notification types are told apart in `events.rs`, where the
//!   reason for each is written down.

use crate::events::TIMER_TOOLS;
use crate::json::Value;

/// Every event `events::apply` acts on, in the order of the table in `docs/design.md`.
pub const EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "Notification",
    "PostToolUse",
    "SessionEnd",
];

/// How Claude Code runs an event's hook; the module header says why.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    Async,
    Sync { timeout_secs: u32 },
}

fn run(event: &str) -> Run {
    match event {
        "SessionStart" => Run::Sync { timeout_secs: 30 },
        "SessionEnd" => Run::Sync { timeout_secs: 1 },
        _ => Run::Async,
    }
}

/// The settings document, with `exe` as the path to this binary.
pub fn settings(exe: &str) -> Value {
    let command = format!("{} hook", shell_quote(exe));
    let mut hooks = Value::obj();
    for event in EVENTS {
        let mut handler = Value::obj();
        handler.set("type", Value::string("command"));
        handler.set("command", Value::string(command.as_str()));
        match run(event) {
            Run::Async => handler.set("async", Value::Bool(true)),
            Run::Sync { timeout_secs } => handler.set("timeout", Value::num(timeout_secs)),
        }
        let mut group = Value::obj();
        if event == "PostToolUse" {
            group.set("matcher", Value::string(TIMER_TOOLS.join("|")));
        }
        group.set("hooks", Value::Arr(vec![handler]));
        hooks.set(event, Value::Arr(vec![group]));
    }
    let mut doc = Value::obj();
    doc.set("hooks", hooks);
    // The status line (owner, 2026-10-06): memory in view while working. Managed settings
    // outrank a user's own, so installing this replaces any status line set per user.
    let mut status = Value::obj();
    status.set("type", Value::string("command"));
    status.set(
        "command",
        Value::string(format!("{} statusline", shell_quote(exe))),
    );
    // Its RAM and load move without any Claude Code event, so it is re-run on a timer too
    // (seconds); Claude Code redraws only when the text changes.
    status.set("refreshInterval", Value::num(60));
    doc.set("statusLine", status);
    doc
}

/// `exe` as one shell word. Hook commands are run by a shell, so a path with a space in it
/// would otherwise be two words and the hook would silently never run.
fn shell_quote(exe: &str) -> String {
    let plain = exe
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c));
    if plain && !exe.is_empty() {
        exe.to_string()
    } else {
        format!("'{}'", exe.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{self, Binding, Event, Outcome};
    use crate::registry::SlotRecord;

    fn installed() -> Value {
        crate::json::parse(&crate::json::to_string_pretty(&settings(
            "/usr/local/bin/claude-sessions",
        )))
        .expect("what we print is json")
    }

    #[test]
    fn every_event_the_state_machine_handles_is_installed() {
        // Calibration in both directions: each installed event changes a record when given a
        // payload that should change it, and an event NOT installed is one apply ignores. One
        // record through the table, in order: a `SubagentStop` changes one that lists its agent.
        let doc = installed();
        let hooks = doc.get("hooks").expect("a hooks key");
        let payloads = [
            r#"{"hook_event_name":"SessionStart","session_id":"s"}"#,
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"p"}"#,
            r#"{"hook_event_name":"Stop"}"#,
            r#"{"hook_event_name":"SubagentStart","agent_id":"a1","agent_type":"general-purpose"}"#,
            r#"{"hook_event_name":"SubagentStop","agent_id":"a1","background_tasks":[]}"#,
            r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#,
            r#"{"hook_event_name":"PostToolUse","tool_name":"ScheduleWakeup","tool_input":{"delaySeconds":60}}"#,
            r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#,
        ];
        let mut rec = SlotRecord::new("claude-1", 0);
        for (event, body) in EVENTS.iter().zip(payloads) {
            assert!(hooks.get(event).is_some(), "{event} is not installed");
            let ev = Event::parse(body).unwrap();
            assert_eq!(ev.name(), *event, "payload table out of step");
            let out = events::apply(&mut rec, &ev, 1_000, Binding::Own, None, None);
            assert_eq!(
                out,
                Outcome::Changed,
                "{event} is installed but does nothing"
            );
        }
        let mut rec = SlotRecord::new("claude-1", 0);
        let ev = Event::parse(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#).unwrap();
        assert!(
            matches!(
                events::apply(&mut rec, &ev, 1_000, Binding::Own, None, None),
                Outcome::Ignored(_)
            ),
            "an event apply ignores is right to be left uninstalled"
        );
    }

    #[test]
    fn the_timer_matcher_names_exactly_the_tools_that_set_timers() {
        let doc = installed();
        let matcher = doc
            .get("hooks")
            .and_then(|h| h.get("PostToolUse"))
            .and_then(Value::as_arr)
            .and_then(|a| a.first())
            .and_then(|g| g.get("matcher"))
            .and_then(Value::as_str)
            .expect("a PostToolUse matcher");
        assert_eq!(matcher, "ScheduleWakeup|CronCreate|CronDelete");
        // Plain letters and `|` only, so Claude Code reads it as an exact list, not a regex.
        assert!(matcher.chars().all(|c| c.is_ascii_alphabetic() || c == '|'));
        for tool in matcher.split('|') {
            let mut rec = SlotRecord::new("claude-1", 0);
            // Seed a cron so a delete has something to delete.
            let seed = Event::parse(
                r#"{"hook_event_name":"PostToolUse","tool_name":"CronCreate","tool_response":{"id":"c1"}}"#,
            )
            .unwrap();
            events::apply(&mut rec, &seed, 1, Binding::Own, None, None);
            let body = format!(
                r#"{{"hook_event_name":"PostToolUse","tool_name":"{tool}","tool_input":{{"delaySeconds":60,"id":"c1"}},"tool_response":{{"id":"c2"}}}}"#
            );
            let out = events::apply(
                &mut rec,
                &Event::parse(&body).unwrap(),
                1_000,
                Binding::Own,
                None,
                None,
            );
            assert_eq!(out, Outcome::Changed, "{tool} is matched but sets no timer");
        }
    }

    fn handler<'a>(doc: &'a Value, event: &str) -> &'a Value {
        doc.get("hooks")
            .and_then(|h| h.get(event))
            .and_then(Value::as_arr)
            .and_then(|a| a.first())
            .and_then(|g| g.get("hooks"))
            .and_then(Value::as_arr)
            .and_then(|a| a.first())
            .expect("a handler")
    }

    #[test]
    fn only_session_start_and_end_are_waited_on_and_only_they_time_out() {
        let doc = installed();
        let hook_wait = crate::lockfile::HOOK_WAIT.as_secs_f64();
        for event in EVENTS {
            let h = handler(&doc, event);
            let is_async = h.get("async").and_then(Value::as_bool) == Some(true);
            let timeout = h.get("timeout").and_then(Value::as_f64);
            match event {
                "SessionEnd" => {
                    assert!(!is_async);
                    assert!(
                        timeout.is_some_and(|t| t <= 1.5),
                        "inside the shared budget"
                    );
                }
                "SessionStart" => {
                    assert!(!is_async);
                    assert!(
                        timeout.is_some_and(|t| t > hook_wait),
                        "outlasts the lock wait"
                    );
                }
                _ => {
                    assert!(is_async, "{event} would make claude wait on the disk");
                    assert_eq!(timeout, None, "{event}: an async hook is never timed out");
                }
            }
        }
    }

    #[test]
    fn a_path_with_a_space_is_one_word() {
        assert_eq!(
            shell_quote("/usr/local/bin/claude-sessions"),
            "/usr/local/bin/claude-sessions"
        );
        assert_eq!(shell_quote("/opt/my tools/cs"), "'/opt/my tools/cs'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }

    #[test]
    fn the_status_line_names_this_binary() {
        let doc = installed();
        let status = doc.get("statusLine").expect("a statusLine");
        assert_eq!(status.get("type").and_then(Value::as_str), Some("command"));
        assert_eq!(
            status.get("command").and_then(Value::as_str),
            Some("/usr/local/bin/claude-sessions statusline")
        );
        assert_eq!(
            status.get("refreshInterval").and_then(Value::as_u64),
            Some(60)
        );
    }
}
