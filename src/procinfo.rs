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
///
/// **A zombie is not alive.** It keeps its pid and its start time until its parent reaps it,
/// so a start-time match alone calls a dead claude alive for as long as a stuck abduco server
/// fails to collect it — and the offloader would then wait out its grace on a corpse and
/// report that `KILL` did not work.
pub fn is_alive(pid: u32, recorded_start: u64) -> bool {
    start_time(pid) == Some(recorded_start) && !matches!(state(pid), Some('Z' | 'X'))
}

/// Field 3 of `/proc/<pid>/stat`: `R`, `S`, `Z` and the rest.
pub fn state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .next()?
        .chars()
        .next()
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

/// One row of a `/proc` snapshot.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub start: u64,
    pub state: char,
    pub args: Vec<String>,
}

/// Every process visible in `/proc`, or `None` if `/proc` could not be listed at all.
///
/// `None` and an empty table are different answers and callers must keep them apart: "no
/// descendants" read from a failed listing would let the offloader stop a slot whose child it
/// simply could not see. A process that exits mid-walk is skipped, which is the truth.
pub fn table() -> Option<Vec<Proc>> {
    let entries = fs::read_dir("/proc").ok()?;
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let tail: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let (Some(state), Some(ppid), Some(start)) = (
            tail.first().and_then(|s| s.chars().next()),
            tail.get(1).and_then(|s| s.parse().ok()),
            tail.get(19).and_then(|s| s.parse().ok()),
        ) else {
            continue;
        };
        let args = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).to_string())
                    .collect()
            })
            .unwrap_or_default();
        out.push(Proc {
            pid,
            ppid,
            comm: stat[open + 1..close].to_string(),
            start,
            state,
            args,
        });
    }
    Some(out)
}

/// Every descendant of `root` in a snapshot, nearest first. `root` itself is not included.
pub fn descendants(table: &[Proc], root: u32) -> Vec<&Proc> {
    let mut out: Vec<&Proc> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for p in table.iter().filter(|p| p.ppid == parent && p.pid != root) {
            // A pid cannot be its own ancestor, but a snapshot taken across a pid's reuse can
            // make it look like one; the check keeps that from looping forever.
            if !out.iter().any(|seen| seen.pid == p.pid) {
                out.push(p);
                frontier.push(p.pid);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_reads_as_alive_and_a_wrong_start_time_does_not() {
        let me = std::process::id();
        let start = start_time(me).expect("our own stat is readable");
        assert!(is_alive(me, start), "calibration: we are alive");
        assert!(!is_alive(me, start + 1), "a reused pid is not the original");
    }

    #[test]
    fn a_zombie_is_not_alive() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        let start = start_time(pid).expect("readable while it exists");
        // Not reaped yet, so it lingers as a zombie with the same pid and start time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state(pid) != Some('Z') && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(state(pid), Some('Z'), "calibration: it is a zombie");
        assert_eq!(start_time(pid), Some(start), "with its start time intact");
        assert!(!is_alive(pid, start));
        child.wait().expect("reap");
    }

    #[test]
    fn the_table_sees_a_child_as_a_descendant() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn");
        let table = table().expect("/proc lists");
        let found = descendants(&table, std::process::id());
        assert!(
            found
                .iter()
                .any(|p| p.pid == child.id() && p.comm == "sleep"),
            "the sleep we started should be under us"
        );
        child.kill().ok();
        child.wait().ok();
    }
}
