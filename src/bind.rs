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
use crate::procinfo;

/// The slot name from the environment, if this process was started under one.
pub fn slot_from_env() -> Option<String> {
    std::env::var("CLAUDE_SESSIONS_SLOT")
        .ok()
        .filter(|s| !s.is_empty())
}

/// What a hook learns from its line of parents.
#[derive(Debug, PartialEq, Eq)]
pub struct Origin {
    pub binding: Binding,
    /// The nearest claude above the hook: the one that fired it.
    pub claude: Option<u32>,
    /// When it fired: the start of the process that claude forked to run the hook, which a
    /// stall after the fork does not move.
    pub fired: Option<Moment>,
}

/// Where the hook `hook_pid` came from.
pub fn origin(hook_pid: u32, fired_in_subagent: bool) -> Origin {
    // hook -> sh -> claude -> zmx is the usual shape; a deeper walk would start finding
    // unrelated claudes in a container that runs several.
    let place = place(&procinfo::lineage(hook_pid, 9), fired_in_subagent);
    Origin {
        binding: place.binding,
        claude: place.claude,
        fired: place
            .forked
            .and_then(procinfo::start_time)
            .and_then(Moment::of_tick),
    }
}

/// How a hook sits on its line of parents.
#[derive(Debug, PartialEq, Eq)]
struct Place {
    binding: Binding,
    claude: Option<u32>,
    /// The process whose start is when the event fired: what claude forked to run the hook,
    /// or with no claude above, the hook itself. An async hook outlives its claude, and then
    /// started no later than the fork it came from.
    forked: Option<u32>,
}

/// How a hook's line of parents, nearest first, places it.
///
/// The claude is the slot's own when its parent is a `zmx` daemon: the daemon forks the
/// session's command directly (seen on zmx 0.8.1), so a nested claude has another claude or a
/// shell in between. A hook fired in a subagent's own context is nested whatever the tree says
/// (`Event::fired_in_subagent`). No claude at all is a hook whose claude has gone, or one run
/// by hand, and neither may rebind a slot.
fn place(line: &[(u32, String)], fired_in_subagent: bool) -> Place {
    let Some(i) = line.iter().skip(1).position(|(_, comm)| comm == "claude") else {
        return Place {
            binding: Binding::Nested,
            claude: None,
            forked: line.first().map(|(pid, _)| *pid),
        };
    };
    let own = !fired_in_subagent && line.get(i + 2).is_some_and(|(_, comm)| comm == "zmx");
    Place {
        binding: if own { Binding::Own } else { Binding::Nested },
        claude: Some(line[i + 1].0),
        forked: Some(line[i].0),
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

    fn placed(binding: Binding, claude: Option<u32>, forked: u32) -> Place {
        Place {
            binding,
            claude,
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
    }

    #[test]
    fn a_hook_under_a_nested_claude_or_none_is_nested() {
        let nested = line(&["claude-sessions", "sh", "claude", "bash", "claude", "zmx"]);
        assert_eq!(place(&nested, false), placed(Binding::Nested, Some(12), 11));
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
