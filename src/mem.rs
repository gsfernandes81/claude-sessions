//! How much room is left, asked of the cgroup rather than of the machine.
//!
//! **`/proc/meminfo` inside a container describes the host.** It reports all of a 4 GB Pi
//! while the container's own ceiling is `mem_limit` — 1 GB for one of these, 2.5 GB for
//! another. A low-memory check built on `MemAvailable` would read "plenty" right up to the
//! OOM kill. The cgroup v2 files are readable unprivileged, verified 2026-10-01.
//!
//! **And `memory.current` counts page cache** (claude-sessions#7, infra's reading of
//! 2026-10-04). File pages stay charged to the cgroup after the process that read them has
//! gone, until the kernel reclaims them: on infra-dev an interrupted Claude Code self-update
//! left about 650 MB of cache, the dry run's memory line read 950 → 297 MB free with nothing
//! running, and the menu would have offered to offload a session for room the kernel would
//! simply have taken back. So what is *used* is `memory.current` less `inactive_file` from
//! `memory.stat` — the working set, as kubelet counts it: cache the kernel reclaims first,
//! long before it would OOM-kill anything.
//!
//! **Only the inactive list, on purpose.** Cache read twice moves to `active_file` and still
//! counts as used, so a burst like the self-update's may be only partly set aside. Counting
//! active cache as free too would overstate room: it holds the running claudes' own code and
//! libraries, which the kernel keeps, and a room check that says yes too readily ends in an
//! OOM kill of a working session — the one mistake worse than an offer to offload. tmpfs pages
//! (`shmem`) are swap-backed and sit on the anon lists, so they are never counted as cache.

use std::fs;

pub struct Memory {
    /// The container's ceiling, or `None` where the cgroup says `max` — then there is no
    /// container limit and the host's figure is the real one.
    pub limit: Option<u64>,
    /// `memory.current`: everything charged to the cgroup, page cache included.
    pub current: Option<u64>,
    /// `inactive_file` from `memory.stat`: the part of `current` that is cache the kernel
    /// reclaims first. `None` where it cannot be read, and then nothing is set aside.
    pub reclaimable: Option<u64>,
}

fn read_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn read() -> Memory {
    let limit = match fs::read_to_string("/sys/fs/cgroup/memory.max") {
        Ok(s) if s.trim() == "max" => None,
        Ok(s) => s.trim().parse().ok(),
        Err(_) => None,
    };
    // No fallback for `current`. `MemAvailable` once stood in when `memory.current` could not
    // be read; it is the host's memory *available*, not the container's used, and against a
    // container limit it read a 4 GB host's free gigabytes as use: no room, every time. With no
    // limit nothing reads `current`, so an unreadable one is simply unknown — and unknown
    // headroom never refuses (`room_for`).
    Memory {
        limit,
        current: read_u64("/sys/fs/cgroup/memory.current"),
        reclaimable: fs::read_to_string("/sys/fs/cgroup/memory.stat")
            .ok()
            .and_then(|s| stat_field(&s, "inactive_file")),
    }
}

/// One `key value` line of a cgroup v2 stat file. The key must match whole: `memory.stat`
/// also has `inactive_file` beside `active_file`, and `memory.events` has `oom_group_kill`
/// beside `oom_kill`.
fn stat_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

impl Memory {
    /// What is in use, page cache the kernel would reclaim first set aside: the working set.
    pub fn used(&self) -> Option<u64> {
        self.current
            .map(|c| c.saturating_sub(self.reclaimable.unwrap_or(0)))
    }

    /// Bytes free within the ceiling, where there is one — counting reclaimable cache as free.
    pub fn headroom(&self) -> Option<u64> {
        match (self.limit, self.used()) {
            (Some(limit), Some(used)) => Some(limit.saturating_sub(used)),
            _ => None,
        }
    }

    /// Is there room for another session? `want` is what one costs — about 250 MB for an idle
    /// Claude Code with the agent view off, measured 2026-10-01.
    ///
    /// Unknown headroom answers **yes**: refusing to open a session because a cgroup file
    /// could not be read would make an unreadable file worse than a full container.
    pub fn room_for(&self, want: u64) -> bool {
        self.headroom().is_none_or(|free| free >= want)
    }
}

/// How many processes in this container the kernel has killed for memory, ever — `oom_kill`
/// in the cgroup's `memory.events`. Read before and after a start, it says whether a claude
/// that died at once was killed for memory: zmx does not report its program's exit status, so
/// the 137 that used to say so is gone (owner, 2026-10-06). It counts the whole container, so
/// an unrelated kill in the same moment would be misread as this one; that is rare enough for
/// a dialog that only ever adds a hint.
pub fn oom_kills() -> Option<u64> {
    oom_kill_in(&fs::read_to_string("/sys/fs/cgroup/memory.events").ok()?)
}

fn oom_kill_in(events: &str) -> Option<u64> {
    stat_field(events, "oom_kill")
}

/// What one idle session costs, with the agent view off.
pub const SESSION_COST: u64 = 250 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oom_kill_count_is_read_from_memory_events() {
        // As cgroup v2 writes it; `oom_group_kill` must not be mistaken for it.
        let events = "low 0\nhigh 0\nmax 12\noom 2\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(oom_kill_in(events), Some(2));
        assert_eq!(oom_kill_in("low 0\n"), None);
    }

    /// infra-dev on 2026-10-04 (claude-sessions#7): 892 MB charged against 1 GiB, 515 MB of
    /// it inactive page cache. As 0.4.2 read it there was no room for a 250 MB session.
    #[test]
    fn reclaimable_cache_is_room() {
        const MB: u64 = 1024 * 1024;
        // `file` is the two lists together; 515 MiB of it inactive, as infra read it.
        let stat = "anon 340000000\nfile 565001600\nkernel 30000000\nshmem 2000000\n\
                    active_file 24984960\ninactive_file 540016640\nslab_reclaimable 9000000\n";
        let mut m = Memory {
            limit: Some(1024 * MB),
            current: Some(892 * MB),
            reclaimable: None,
        };
        assert!(
            !m.room_for(SESSION_COST),
            "calibration: the cache counted as used"
        );
        m.reclaimable = stat_field(stat, "inactive_file");
        assert_eq!(
            m.reclaimable,
            Some(540_016_640),
            "not active_file, read first"
        );
        assert_eq!(m.used(), Some(892 * MB - 540_016_640));
        assert!(m.room_for(SESSION_COST));
        assert_eq!(m.headroom(), Some(1024 * MB - (892 * MB - 540_016_640)));
    }

    #[test]
    fn an_unreadable_figure_is_unknown_and_unknown_never_refuses() {
        const MB: u64 = 1024 * 1024;
        // A limit with no `memory.current`: once filled in from the host's MemAvailable, which
        // read a host's free gigabytes as the container's use.
        let m = Memory {
            limit: Some(1024 * MB),
            current: None,
            reclaimable: Some(500 * MB),
        };
        assert_eq!(m.used(), None);
        assert_eq!(m.headroom(), None);
        assert!(m.room_for(SESSION_COST));
        // No `memory.stat`: nothing is set aside, which is 0.4.2's reading — the safe one.
        let m = Memory {
            limit: Some(1024 * MB),
            current: Some(900 * MB),
            reclaimable: None,
        };
        assert_eq!(m.used(), Some(900 * MB));
        assert!(!m.room_for(SESSION_COST));
        // More cache than charge, as a racy read of two files could give: never below zero.
        let m = Memory {
            limit: Some(1024 * MB),
            current: Some(100 * MB),
            reclaimable: Some(200 * MB),
        };
        assert_eq!(m.used(), Some(0));
    }
}
