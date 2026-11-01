//! Which slot a hook belongs to, and whether it is the slot's own claude.
//!
//! The slot name arrives in the environment, because the menu starts every slot as
//! `zmx attach claude-<n> env CLAUDE_SESSIONS_SLOT=claude-<n> … claude …` and a hook inherits
//! it. That is the easy half.
//!
//! The hard half is that **a nested claude inherits it too** — a `claude -p` from a Bash
//! tool call, or a subagent — and would otherwise rebind the slot to its own short-lived
//! conversation, so the menu would offer to resume a conversation that ended seconds later.
//! A hook's claude is the slot's own only when it is the direct child of that slot's zmx
//! daemon. Anything else is work running under the slot.

use crate::clock::Moment;
use crate::events::Binding;
use crate::harness::Harness;
use crate::procinfo;

/// The slot this process runs in, and whether we started it.
///
/// The environment is the fast answer: the menu sets `CLAUDE_SESSIONS_SLOT` when it starts a
/// slot. Failing that — a `zmx attach work claude` somebody typed, which nothing here started —
/// zmx names its own session in `ZMX_SESSION`, so the session is listed as unregistered rather
/// than ignored. Only with a zmx daemon above us: the variable is inherited, and a process that
/// merely carries it out of a session is not in one.
pub fn slot() -> Option<(String, bool)> {
    let var = |k| std::env::var(k).ok().filter(|s: &String| !s.is_empty());
    if let Some(slot) = var("CLAUDE_SESSIONS_SLOT") {
        return Some((slot, true));
    }
    let name = var("ZMX_SESSION")?;
    procinfo::ancestor_named(std::process::id(), "zmx", 10)?;
    Some((name, false))
}

/// How a hook sits on its line of parents.
#[derive(Debug, PartialEq, Eq)]
pub struct Place {
    pub binding: Binding,
    /// The slot's own claude, when it is the one that fired the hook.
    pub own_pid: Option<u32>,
    /// The process whose start is when the event fired: what claude forked to run the hook,
    /// or with no claude above, the hook itself: an async hook outlives its claude, and its own
    /// start is that fork when `sh` exec'd it, later only by the shell's start-up otherwise.
    forked: Option<u32>,
}

impl Place {
    /// When the event fired, which a stall after the fork does not move; when it landed only
    /// if /proc cannot be read.
    pub fn fired(&self) -> Moment {
        self.forked
            .and_then(procinfo::start_time)
            .and_then(Moment::of_tick)
            .unwrap_or_else(Moment::now)
    }
}

/// Where the hook `hook_pid` sits.
pub fn origin(hook_pid: u32, fired_in_subagent: bool) -> Place {
    // hook -> sh -> claude -> zmx is the usual shape; a deeper walk would start finding
    // unrelated claudes in a container that runs several.
    place(&procinfo::lineage(hook_pid, 9), fired_in_subagent)
}

/// How a hook's line of parents, nearest first, places it.
///
/// The harness — a `claude`, or a `pi` (`Harness::of_comm`) — is the slot's own when its
/// parent is a `zmx` daemon: the daemon forks the session's command directly (seen on zmx
/// 0.8.1), so a nested one has another harness or a shell in between. A hook fired in a
/// subagent's own context is nested whatever the tree says (`Event::fired_in_subagent`). No
/// harness at all is a hook whose harness has gone, or one run by hand, and neither may
/// rebind a slot.
///
/// A remote-view slot has one process between: zmx forks `claude-sessions holder`, which runs
/// claude headless on pipes (`holder.rs`), so `claude` → `claude-sessions` → `zmx` is also the
/// slot's own. Only claude, and only that holder directly under the daemon: a holder somebody
/// ran inside a slot's shell has the shell between it and zmx, and its claude is nested.
/// (`claude-sessions` is exactly the 15 characters `/proc/<pid>/comm` keeps.)
fn place(line: &[(u32, String)], fired_in_subagent: bool) -> Place {
    let Some(c) = (1..line.len()).find(|&c| Harness::of_comm(&line[c].1).is_some()) else {
        return Place {
            binding: Binding::Nested,
            own_pid: None,
            forked: line.first().map(|(pid, _)| *pid),
        };
    };
    let comm = |i: usize| line.get(i).map(|(_, comm)| comm.as_str());
    let under_zmx = comm(c + 1) == Some("zmx");
    let under_holder = comm(c) == Some(Harness::Claude.comm())
        && comm(c + 1) == Some("claude-sessions")
        && comm(c + 2) == Some("zmx");
    let own = !fired_in_subagent && (under_zmx || under_holder);
    Place {
        binding: if own { Binding::Own } else { Binding::Nested },
        own_pid: own.then_some(line[c].0),
        forked: Some(line[c - 1].0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(names: &[&str]) -> Vec<(u32, String)> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| (i as u32 + 10, n.to_string()))
            .collect()
    }

    fn placed(binding: Binding, own_pid: Option<u32>, forked: u32) -> Place {
        Place {
            binding,
            own_pid,
            forked: Some(forked),
        }
    }

    #[test]
    fn a_hook_under_the_slots_own_claude_is_its_own() {
        let own = line(&["claude-sessions", "sh", "claude", "zmx"]);
        assert_eq!(place(&own, false), placed(Binding::Own, Some(12), 11));
        assert_eq!(
            place(&own, true).binding,
            Binding::Nested,
            "fired inside a subagent"
        );
        // `sh` exec'd the hook: the hook is what claude forked.
        let exec = line(&["claude-sessions", "claude", "zmx"]);
        assert_eq!(place(&exec, false), placed(Binding::Own, Some(11), 10));
        // pi's extension spawns the hook itself, with nothing between.
        let pi = line(&["claude-sessions", "pi", "zmx"]);
        assert_eq!(place(&pi, false), placed(Binding::Own, Some(11), 10));
        // A pi under a claude's tool call, or a claude under pi's: nested either way.
        let mixed = line(&["claude-sessions", "pi", "bash", "claude", "zmx"]);
        assert_eq!(place(&mixed, false), placed(Binding::Nested, None, 10));
        let mixed = line(&["claude-sessions", "claude", "bash", "pi", "zmx"]);
        assert_eq!(place(&mixed, false), placed(Binding::Nested, None, 10));
    }

    #[test]
    fn a_hook_under_a_remote_slots_claude_is_its_own_and_one_beneath_it_is_nested() {
        let own = line(&["claude-sessions", "sh", "claude", "claude-sessions", "zmx"]);
        assert_eq!(place(&own, false), placed(Binding::Own, Some(12), 11));
        assert_eq!(place(&own, true).binding, Binding::Nested, "in a subagent");
        // A `claude -p` from a tool call of the remote slot's claude.
        let nested = line(&[
            "claude-sessions",
            "sh",
            "claude",
            "bash",
            "claude",
            "claude-sessions",
            "zmx",
        ]);
        assert_eq!(place(&nested, false), placed(Binding::Nested, None, 11));
        // A holder somebody ran from a normal slot's shell: the shell is between it and zmx.
        let typed = line(&[
            "claude-sessions",
            "claude",
            "claude-sessions",
            "fish",
            "zmx",
        ]);
        assert_eq!(place(&typed, false).binding, Binding::Nested);
        // pi is never under a holder: the holder runs claude.
        let pi = line(&["claude-sessions", "pi", "claude-sessions", "zmx"]);
        assert_eq!(place(&pi, false).binding, Binding::Nested);
    }

    #[test]
    fn a_hook_under_a_nested_claude_or_none_is_nested() {
        let nested = line(&["claude-sessions", "sh", "claude", "bash", "claude", "zmx"]);
        assert_eq!(place(&nested, false), placed(Binding::Nested, None, 11));
        let mine = line(&["claude-sessions", "sh", "claude"]);
        assert_eq!(
            place(&mine, false).binding,
            Binding::Nested,
            "a claude with no zmx above"
        );
        // Its claude gone, the hook was reparented: its own start is when it fired.
        let orphan = line(&["claude-sessions", "init"]);
        assert_eq!(place(&orphan, false), placed(Binding::Nested, None, 10));
    }
}
