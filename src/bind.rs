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

use crate::clock::{self, Millis};
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
    pub fired_at: Option<Millis>,
}

/// Where the hook `hook_pid` came from.
///
/// Its claude is the slot's own when that claude's parent is a `zmx` daemon: the daemon forks
/// the session's command directly (seen on zmx 0.8.1), so a nested claude has another claude
/// or a shell in between. A hook fired in a subagent's own context is nested whatever the
/// tree says (`Event::fired_in_subagent`). No claude above at all is something running the
/// hook by hand, which must not rebind a slot it may know nothing about.
pub fn origin(hook_pid: u32, fired_in_subagent: bool) -> Origin {
    // hook -> sh -> claude -> zmx is the usual shape; a deeper walk would start finding
    // unrelated claudes in a container that runs several.
    let line = procinfo::lineage(hook_pid, 9);
    let Some(at) = line.iter().skip(1).position(|(_, comm)| comm == "claude") else {
        return Origin {
            binding: Binding::Nested,
            claude: None,
            fired_at: None,
        };
    };
    let (forked, claude) = (line[at].0, line[at + 1].0);
    let own = !fired_in_subagent && line.get(at + 2).is_some_and(|(_, comm)| comm == "zmx");
    Origin {
        binding: if own { Binding::Own } else { Binding::Nested },
        claude: Some(claude),
        fired_at: procinfo::start_time(forked).and_then(clock::at_tick),
    }
}
