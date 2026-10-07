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
///
/// **Only for a process, never for a thread.** `/proc/<tid>/stat` opens for any thread id even
/// though threads are not listed in `/proc`, so a stored pid that has since been handed out as
/// some other program's thread id would otherwise read as alive — found on infra-dev, where a
/// dead session's pid 161 was a thread of cloudflared (issue #4). A thread's `Tgid` is its
/// process's pid, not its own.
pub fn start_time(pid: u32) -> Option<u64> {
    if !is_process(pid) {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    // After `comm` the fields are: state(3) ppid(4) ... starttime(22). `tail` begins at
    // field 3, so starttime is the 20th whitespace-separated token in it.
    tail.split_whitespace().nth(19)?.parse().ok()
}

/// Whether `pid` names a process (its own thread-group leader) rather than a thread.
fn is_process(pid: u32) -> bool {
    let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("Tgid:"))
        .and_then(|t| t.trim().parse::<u32>().ok())
        == Some(pid)
}

/// Is this the same process we recorded, rather than a reuse of its pid?
///
/// **A zombie is not alive.** It keeps its pid and its start time until its parent reaps it,
/// so a start-time match alone calls a dead claude alive for as long as a stuck zmx daemon
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

/// `pid` and up to `limit` of its ancestors, nearest first, with each one's `comm`.
pub fn lineage(pid: u32, limit: usize) -> Vec<(u32, String)> {
    let mut line = Vec::new();
    let mut cur = pid;
    while line.len() <= limit && cur > 1 {
        let Some(name) = comm(cur) else { break };
        line.push((cur, name));
        match parent(cur) {
            Some(p) => cur = p,
            None => break,
        }
    }
    line
}

/// The nearest ancestor of `pid` whose `comm` is `name`, at most `limit` steps up.
pub fn ancestor_named(pid: u32, name: &str, limit: usize) -> Option<u32> {
    lineage(pid, limit)
        .into_iter()
        .skip(1)
        .find(|(_, comm)| comm == name)
        .map(|(p, _)| p)
}

/// How long ago a process started, from its start time in clock ticks.
pub fn age(start_ticks: u64) -> Option<std::time::Duration> {
    let ns = crate::clock::since_tick_ns(start_ticks)?.max(0);
    Some(std::time::Duration::from_nanos(u64::try_from(ns).ok()?))
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
    fn a_thread_id_is_not_a_process() {
        // A thread of this very test process, kept alive while we look at it.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (tid_tx, tid_rx) = std::sync::mpsc::channel::<u32>();
        let worker = std::thread::spawn(move || {
            let tid = std::fs::read_link("/proc/thread-self")
                .ok()
                .and_then(|p| p.file_name()?.to_str()?.parse().ok())
                .expect("thread-self names the thread");
            tid_tx.send(tid).unwrap();
            rx.recv().ok();
        });
        let tid = tid_rx.recv().unwrap();
        let me = std::process::id();
        assert_ne!(tid, me, "calibration: a thread has its own id");
        assert!(
            std::fs::read_to_string(format!("/proc/{tid}/stat")).is_ok(),
            "calibration: its stat opens, which is exactly the trap"
        );
        assert_eq!(start_time(tid), None, "a thread id is not a process");
        assert!(
            start_time(me).is_some(),
            "calibration: the process itself is"
        );
        tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn our_own_age_is_small_and_known() {
        let start = start_time(std::process::id()).unwrap();
        let age = age(start).expect("uptime is readable");
        // The test binary started moments ago; a wrong tick rate would put this hours out.
        assert!(age < std::time::Duration::from_secs(600), "age {age:?}");
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

    #[test]
    fn a_lineage_runs_up_through_our_child_and_its_start_is_one_moment() {
        let before = crate::clock::now();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .spawn()
            .expect("spawn");
        let me = std::process::id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let grandchild = loop {
            let t = table().expect("/proc lists");
            if let Some(p) = descendants(&t, child.id()).first() {
                break p.pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the sleep never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let line: Vec<u32> = lineage(grandchild, 2).into_iter().map(|(p, _)| p).collect();
        assert_eq!(line, [grandchild, child.id(), me]);
        assert_eq!(ancestor_named(grandchild, "sh", 4), Some(child.id()));
        assert_eq!(
            ancestor_named(grandchild, "sleep", 4),
            None,
            "calibration: not itself"
        );
        let tick = start_time(child.id()).unwrap();
        let at = crate::clock::at_tick(tick).unwrap();
        assert!(
            at + 10 >= before && at <= crate::clock::now(),
            "{before} {at}"
        );
        std::thread::sleep(std::time::Duration::from_millis(7));
        assert_eq!(
            crate::clock::at_tick(tick),
            Some(at),
            "one tick reads the same later"
        );
        assert_eq!(
            crate::clock::at_tick(tick + 1),
            Some(at + 10),
            "calibration"
        );
        let _ = std::process::Command::new("kill")
            .args(["-9", &grandchild.to_string()])
            .status();
        child.kill().ok();
        child.wait().ok();
    }
}
