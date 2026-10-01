//! What `/proc` can answer about a process, and nothing else.
//!
//! **A pid alone is never enough.** Pids are reused, and this container is restarted often
//! enough that a stored pid will eventually name somebody else's process — the offloader
//! would then signal it. Every record therefore carries the pid AND its start time, read
//! from field 22 of `/proc/<pid>/stat`, which is monotonic within a boot and cannot be
//! reused: together they identify a process rather than a slot in a table.

use std::fs;

/// Field 22 of `/proc/<pid>/stat`, in clock ticks since boot.
///
/// The parse looks dim on purpose. `comm` — field 2 — is the executable name in
/// parentheses and may itself contain spaces and parentheses, so splitting the line on
/// whitespace from the left is wrong for any process whose name contains a space. Everything
/// after the LAST `)` is fixed-width, so that is where the count starts.
pub fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    // After `comm` the fields are: state(3) ppid(4) ... starttime(22). `tail` begins at
    // field 3, so starttime is the 20th whitespace-separated token in it.
    tail.split_whitespace().nth(19)?.parse().ok()
}

/// Is this the same process we recorded, rather than a reuse of its pid?
pub fn is_alive(pid: u32, recorded_start: u64) -> bool {
    start_time(pid) == Some(recorded_start)
}

/// The parent of a pid, from field 4 of `/proc/<pid>/stat`.
pub fn parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(1)?.parse().ok()
}

/// The first element of a process's argv, as `/proc` has it.
pub fn comm(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')? + 1;
    let close = stat.rfind(')')?;
    Some(stat[open..close].to_string())
}

/// Walk up from `pid` looking for an ancestor whose `comm` is `name`, at most `limit` steps.
///
/// Used for the one question the hook has to answer before it writes anything: is the claude
/// that fired this event the slot's own, or a nested one? A nested claude inherits
/// `CLAUDE_SESSIONS_SLOT` from its parent and would otherwise rebind the slot to its own
/// short-lived conversation.
pub fn ancestor_named(pid: u32, name: &str, limit: usize) -> Option<u32> {
    let mut cur = pid;
    for _ in 0..limit {
        let p = parent(cur)?;
        if p <= 1 {
            return None;
        }
        if comm(p).as_deref() == Some(name) {
            return Some(p);
        }
        cur = p;
    }
    None
}
