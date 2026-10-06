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

use crate::events::Binding;
use crate::procinfo;

/// The slot name from the environment, if this process was started under one.
pub fn slot_from_env() -> Option<String> {
    std::env::var("CLAUDE_SESSIONS_SLOT")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Decide the binding for the claude that fired this hook.
///
/// `hook_pid` is the hook process itself; its claude is an ancestor. We look for the nearest
/// `claude` above us, then ask whether ITS parent is a `zmx` daemon. One generation is the
/// whole test: the daemon forks the session's command directly (seen on zmx 0.8.1), so the
/// slot's own claude has zmx as its parent and a nested one has another claude (or a shell)
/// in between.
///
/// A hook fired in a subagent's own context is a cheaper answer when the payload says so
/// (`Event::fired_in_subagent`), so the caller passes it; a `claude -p` from a shell carries
/// no such field and still needs this.
pub fn binding_for(hook_pid: u32, fired_in_subagent: bool) -> Binding {
    if fired_in_subagent {
        return Binding::Nested;
    }
    // At most a few generations: hook -> sh -> claude is the usual shape, and a deep walk
    // would start finding unrelated claudes in a container that runs several.
    let Some(claude) = procinfo::ancestor_named(hook_pid, "claude", 8) else {
        // No claude above us at all. Something ran the hook by hand; treat it as nested
        // rather than letting it rebind a slot it may know nothing about.
        return Binding::Nested;
    };
    match procinfo::parent(claude).and_then(procinfo::comm) {
        Some(parent) if parent == "zmx" => Binding::Own,
        _ => Binding::Nested,
    }
}
