//! How much room is left, asked of the cgroup rather than of the machine.
//!
//! **`/proc/meminfo` inside a container describes the host.** It reports all of a 4 GB Pi
//! while the container's own ceiling is `mem_limit` — 1 GB for one of these, 2.5 GB for
//! another. A low-memory check built on `MemAvailable` would read "plenty" right up to the
//! OOM kill. The cgroup v2 files are readable unprivileged, verified 2026-10-01.

use std::fs;

pub struct Memory {
    /// The container's ceiling, or `None` where the cgroup says `max` — then there is no
    /// container limit and the host's figure is the real one.
    pub limit: Option<u64>,
    pub current: Option<u64>,
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
    Memory {
        limit,
        current: read_u64("/sys/fs/cgroup/memory.current").or_else(mem_available),
    }
}

/// `MemAvailable`, in bytes — the fallback, and only correct when there is no cgroup limit.
fn mem_available() -> Option<u64> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

impl Memory {
    /// Bytes free within the ceiling, where there is one.
    pub fn headroom(&self) -> Option<u64> {
        match (self.limit, self.current) {
            (Some(limit), Some(current)) => Some(limit.saturating_sub(current)),
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

/// What one idle session costs, with the agent view off.
pub const SESSION_COST: u64 = 250 * 1024 * 1024;
