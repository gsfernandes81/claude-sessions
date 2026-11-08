//! The load line: `RAM: 1.9G / 3.0G, Swap: 0.2G / 4.0G, Load: 2.3, or3-dev` (owner,
//! 2026-10-06; swap split out of RAM on 2026-11-08). Claude Code draws it under its prompt, so
//! memory is in view while you work, and the menu draws the same figures at the foot of the
//! door (owner, 2026-10-27), roughly where Claude Code puts it.
//!
//! **The figures are every container's, wherever the line is read** (owner, 2026-10-27:
//! "total ram and total cpu in all sessions"). RAM is the dev containers' shared pool where it
//! is mounted (`CLAUDE_SESSIONS_POOL`, a cgroup v2 directory, read as the menu reads a
//! container: the working set, page cache the kernel reclaims first set aside — `mem.rs`).
//! Where it is not, a session's line falls back to its own container's limit, or the host's
//! total and available memory where there is no limit, and the door to what the machines
//! have — the host's used and total memory, which every container's `/proc/meminfo` reports
//! alike, **counted once per machine** as the load is ([`total_memory`]; owner, 2026-11-08:
//! eight containers' 2 GB ceilings on a 4 GB Pi are not 16 GB) — or, from agents too old to
//! say what their machine has, to the sum of what each said. Swap is read from the same place
//! as RAM — the cgroup's `memory.swap.current` against its `memory.swap.max`, capped at the
//! host's `SwapTotal`, or the host's `SwapTotal` less `SwapFree` — and `Swap: none` where
//! there is no swap. Load is the one-minute load average, counted once per machine too:
//! `/proc/loadavg` is not namespaced, so every container on one host reads the same host-wide
//! figure, and adding them would count that host once per container. The kernel's `boot_id`
//! says which machine a reading is from ([`total_load`]). The last part names where the line is
//! read: the container's hostname in a session, the hub's at the door. Anything that cannot
//! be read is a `?`, never a guess. ASCII only: this lands in whatever font the terminal has
//! (CLAUDE.md, on glyphs).
//!
//! **Colour** (owner, 2026-10-06): yellow, then red, as RAM or swap passes 70% and 85% of its
//! total and as the load passes 0.7 and 1.0 per logical core — the machines', because the load
//! average counts the whole machine too. The numbers carry the meaning; colour is emphasis on
//! top, and `NO_COLOR` turns it off. The menu draws the same levels in its own vocabulary
//! (`render.rs`), since its amber is reserved for *waiting for you*.
//!
//! Claude Code runs it on its own events and every 60 s (`hooks_config`), so it reads a few
//! small files and nothing else — no registry, no `zmx list`.

use crate::ui::HostLoad;
use std::io::IsTerminal as _;
use std::path::Path;

const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// How worrying a figure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Calm,
    Warn,
    High,
}

/// `Warn` from the first threshold, `High` from the second.
fn level(value: f64, warn: f64, high: f64) -> Level {
    if value >= high {
        Level::High
    } else if value >= warn {
        Level::Warn
    } else {
        Level::Calm
    }
}

/// What one line says, read or totalled; drawn by [`Figures::line`] for Claude Code and by
/// the menu in its own styles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Figures {
    pub used: Option<u64>,
    pub total: Option<u64>,
    /// Swap used and its total, read from where RAM is. `None` where it cannot be read.
    pub swap: Option<(u64, u64)>,
    /// The load average, in hundredths, as [`HostLoad`] carries it.
    pub load: Option<u64>,
    pub cores: Option<usize>,
    /// Where it is read: a container's hostname, or the hub's.
    pub place: String,
}

impl Figures {
    /// `RAM: 1.0G / 3.0G`, and how full that is: yellow at 70%, red at 85%.
    pub fn ram(&self) -> (String, Level) {
        match (self.used, self.total) {
            (Some(u), Some(t)) if t > 0 => (
                format!("RAM: {} / {}", gib(u), gib(t)),
                level(u as f64 / t as f64, 0.70, 0.85),
            ),
            _ => ("RAM: ?".to_string(), Level::Calm),
        }
    }

    /// `Swap: 0.2G / 4.0G`, at RAM's thresholds; `Swap: none` where there is none to use.
    pub fn swap(&self) -> (String, Level) {
        match self.swap {
            Some((u, t)) if t > 0 => (
                format!("Swap: {} / {}", gib(u), gib(t)),
                level(u as f64 / t as f64, 0.70, 0.85),
            ),
            Some((0, 0)) => ("Swap: none".to_string(), Level::Calm),
            _ => ("Swap: ?".to_string(), Level::Calm),
        }
    }

    /// `Load: 2.3`, and how busy that is: 0.7 and 1.0 a core. With no core count it is only
    /// a number, judged against nothing.
    pub fn load(&self) -> (String, Level) {
        match self.load.map(|c| c as f64 / 100.0) {
            Some(l) => (
                format!("Load: {l:.1}"),
                self.cores
                    .filter(|&c| c > 0)
                    .map_or(Level::Calm, |c| level(l / c as f64, 0.7, 1.0)),
            ),
            None => ("Load: ?".to_string(), Level::Calm),
        }
    }

    pub fn place(&self) -> &str {
        if self.place.is_empty() {
            "?"
        } else {
            &self.place
        }
    }

    /// The line as Claude Code shows it, in ANSI colour unless `colour` is false.
    pub fn line(&self, colour: bool) -> String {
        let paint = |(text, lvl): (String, Level)| match lvl {
            Level::High if colour => format!("{RED}{text}{RESET}"),
            Level::Warn if colour => format!("{YELLOW}{text}{RESET}"),
            _ => text,
        };
        format!(
            "{}, {}, {}, {}",
            paint(self.ram()),
            paint(self.swap()),
            paint(self.load()),
            self.place()
        )
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// The total load of every machine the readings come from, and their logical CPUs together:
/// each machine counted once, by its `boot_id`, the first reading of it winning — so a caller
/// puts its freshest first. The cores are `None` when no reading of any machine had them.
pub fn total_load<'a>(
    readings: impl IntoIterator<Item = &'a HostLoad>,
) -> Option<(u64, Option<usize>)> {
    let mut seen: Vec<&str> = Vec::new();
    let mut load = 0;
    let mut cores: Option<usize> = None;
    for r in readings {
        if seen.contains(&r.boot.as_str()) {
            continue;
        }
        seen.push(&r.boot);
        load += r.centi;
        if let Some(c) = r.cores {
            cores = Some(cores.unwrap_or(0) + c);
        }
    }
    (!seen.is_empty()).then_some((load, cores))
}

/// A figure of memory or swap: used and total, in bytes.
pub type UsedTotal = (u64, u64);

/// The memory and swap of every machine the readings come from, used and total, each machine
/// counted once as [`total_load`] counts it — by the first reading of it that carries the
/// figure, since an older agent's reading has none. A figure no reading had is `None`.
pub fn total_memory<'a>(
    readings: impl IntoIterator<Item = &'a HostLoad>,
) -> (Option<UsedTotal>, Option<UsedTotal>) {
    let mut memory: Option<UsedTotal> = None;
    let mut swap: Option<UsedTotal> = None;
    let mut seen = (Vec::new(), Vec::new());
    for r in readings {
        for (figure, said, seen) in [
            (&mut memory, r.memory, &mut seen.0),
            (&mut swap, r.swap, &mut seen.1),
        ] {
            let Some((used, total)) = said else { continue };
            if seen.contains(&r.boot.as_str()) {
                continue;
            }
            seen.push(&r.boot);
            let (u, t) = figure.unwrap_or((0, 0));
            *figure = Some((u + used, t + total));
        }
    }
    (memory, swap)
}

/// The door's figures: the pool's memory and swap where the hub can read it, else the
/// machines' own — each counted once, from the hub's reading and the containers' — since the
/// containers' ceilings together come to more than their host has (eight 2 GB containers on
/// a 4 GB Pi are not 16 GB; owner, 2026-11-08), else, from containers too old to say what
/// their machine has, their working sets and ceilings added up; and the load of every
/// machine, the hub's own reading first, since it is the freshest (a container's arrives
/// with its rows).
pub fn door<'a>(
    own: Option<HostLoad>,
    pool: Option<(u64, u64)>,
    pool_swap: Option<(u64, u64)>,
    containers: impl IntoIterator<Item = &'a crate::ui::Header>,
    place: &str,
) -> Figures {
    let containers: Vec<&crate::ui::Header> = containers.into_iter().collect();
    let readings: Vec<&HostLoad> = own
        .iter()
        .chain(containers.iter().filter_map(|h| h.load.as_ref()))
        .collect();
    let sum = |said: Vec<(u64, u64)>| {
        (!said.is_empty()).then(|| {
            (
                said.iter().map(|m| m.0).sum(),
                said.iter().map(|m| m.1).sum(),
            )
        })
    };
    let (machines, machines_swap) = total_memory(readings.iter().copied());
    let memory = match pool {
        Some(pool) => Some(pool),
        None => machines.or_else(|| sum(containers.iter().filter_map(|h| h.memory).collect())),
    };
    let swap = match pool {
        Some(_) => pool_swap,
        None => machines_swap.or_else(|| sum(containers.iter().filter_map(|h| h.swap).collect())),
    };
    let total_load = total_load(readings.iter().copied());
    Figures {
        used: memory.map(|m| m.0),
        total: memory.map(|m| m.1),
        swap,
        load: total_load.map(|t| t.0),
        cores: total_load.and_then(|t| t.1),
        place: place.to_string(),
    }
}

/// The machine's logical CPUs, from a list like `0-3,6`: the same scope as `/proc/loadavg`,
/// which counts the whole machine, whatever share of it this container may use.
fn cpu_count(list: &str) -> Option<usize> {
    let mut n = 0;
    for part in list.trim().split(',') {
        n += match part.split_once('-') {
            Some((a, b)) => {
                b.parse::<usize>()
                    .ok()?
                    .checked_sub(a.parse::<usize>().ok()?)?
                    + 1
            }
            None => {
                part.parse::<usize>().ok()?;
                1
            }
        };
    }
    // Never 0: every part counts at least one or the whole list is refused.
    Some(n)
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, in bytes.
fn host_memory(meminfo: &str) -> Option<(u64, u64)> {
    let kb = |key: &str| {
        meminfo.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix(':')?;
            v.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    Some((kb("MemTotal")? * 1024, kb("MemAvailable")? * 1024))
}

/// `SwapTotal` and `SwapFree` from `/proc/meminfo`, in bytes.
fn host_swap(meminfo: &str) -> Option<(u64, u64)> {
    let kb = |key: &str| {
        meminfo.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix(':')?;
            v.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    Some((kb("SwapTotal")? * 1024, kb("SwapFree")? * 1024))
}

/// A cgroup's swap, used and total, from [`crate::mem::swap_at`]'s reading: its ceiling, or the
/// host's `SwapTotal` where that is smaller or there is no ceiling, since a cgroup can swap no
/// more than the host has. `None` where the use or every total is unreadable.
fn cgroup_swap(
    (current, max): (Option<u64>, Option<u64>),
    meminfo: Option<&str>,
) -> Option<(u64, u64)> {
    let host = meminfo.and_then(host_swap).map(|h| h.0);
    let total = match (max, host) {
        (Some(m), Some(h)) => m.min(h),
        (m, h) => m.or(h)?,
    };
    Some((current?, total))
}

/// The one-minute load average from `/proc/loadavg`, in hundredths.
fn loadavg_centi(text: &str) -> Option<u64> {
    let l: f64 = text.split_whitespace().next()?.parse().ok()?;
    (l.is_finite() && l >= 0.0).then(|| (l * 100.0).round() as u64)
}

/// This machine's load and memory, as this container reads them. `None` without a load
/// average or a `boot_id`: a reading that cannot say which machine it is could only be
/// double-counted. The memory is from `/proc/meminfo`, which describes the host, used (total
/// less available) and total; the swap the same way.
pub fn host_load() -> Option<HostLoad> {
    host_load_at(Path::new("/"))
}

/// [`host_load`] with `/proc` and `/sys` under `root`.
fn host_load_at(root: &Path) -> Option<HostLoad> {
    let read = |p: &str| std::fs::read_to_string(root.join(p)).ok();
    let meminfo = read("proc/meminfo");
    let used_of = |(total, spare): (u64, u64)| (total.saturating_sub(spare), total);
    Some(HostLoad {
        centi: loadavg_centi(&read("proc/loadavg")?)?,
        cores: read("sys/devices/system/cpu/online").and_then(|s| cpu_count(&s)),
        boot: read("proc/sys/kernel/random/boot_id")?.trim().to_string(),
        memory: meminfo.as_deref().and_then(host_memory).map(used_of),
        swap: meminfo.as_deref().and_then(host_swap).map(used_of),
    })
    .filter(|h| !h.boot.is_empty())
}

/// The pool's working set and ceiling, where infra mounts it (`CLAUDE_SESSIONS_POOL`) and it
/// has a ceiling.
pub fn pool() -> Option<(u64, u64)> {
    pool_at(&std::env::var("CLAUDE_SESSIONS_POOL").ok()?)
}

/// The pool's swap, read as [`pool`] reads its memory, against the host's swap where the pool
/// has no swap ceiling of its own.
pub fn pool_swap() -> Option<(u64, u64)> {
    let dir = std::env::var("CLAUDE_SESSIONS_POOL").ok()?;
    if dir.is_empty() {
        return None;
    }
    cgroup_swap(
        crate::mem::swap_at(&dir),
        std::fs::read_to_string("/proc/meminfo").ok().as_deref(),
    )
}

/// This container's swap against its ceiling, said where its memory is (a container with a
/// memory limit), for the agent's header.
pub fn own_swap() -> Option<(u64, u64)> {
    cgroup_swap(
        crate::mem::swap(),
        std::fs::read_to_string("/proc/meminfo").ok().as_deref(),
    )
}

/// [`pool`] from the cgroup directory `dir`; an empty one is no pool.
fn pool_at(dir: &str) -> Option<(u64, u64)> {
    if dir.is_empty() {
        return None;
    }
    let m = crate::mem::read_at(dir);
    m.limit.zip(m.used()).map(|(limit, used)| (used, limit))
}

/// What a session's line shows for memory: the pool, else this container against its limit,
/// else the host's total and what it has available (`meminfo`, `/proc/meminfo`'s text).
fn memory(
    pool: Option<(u64, u64)>,
    own: &crate::mem::Memory,
    meminfo: Option<&str>,
) -> (Option<u64>, Option<u64>) {
    if let Some((used, total)) = pool {
        return (Some(used), Some(total));
    }
    match own.limit {
        Some(limit) => (own.used(), Some(limit)),
        None => match meminfo.and_then(host_memory) {
            Some((total, available)) => (Some(total.saturating_sub(available)), Some(total)),
            None => (None, None),
        },
    }
}

/// What a session's line shows for swap, from where its RAM comes ([`memory`]): the pool's
/// (`pool`, read only where the pool's memory is), else this container's where it has a
/// limit, else the host's.
fn swap(
    pool: Option<Option<(u64, u64)>>,
    own: &crate::mem::Memory,
    own_swap: (Option<u64>, Option<u64>),
    meminfo: Option<&str>,
) -> Option<(u64, u64)> {
    if let Some(pool) = pool {
        return pool;
    }
    match own.limit {
        Some(_) => cgroup_swap(own_swap, meminfo),
        None => meminfo
            .and_then(host_swap)
            .map(|(total, free)| (total.saturating_sub(free), total)),
    }
}

/// Read, and say the line. Claude Code writes its session as JSON on stdin, straight after
/// starting this; it is drained so the writer never meets a closed pipe, but never waited on —
/// not at a terminal, and not past a moment's quiet on a pipe whose writer stays open (a
/// person running this through `docker exec -i`).
pub fn run() {
    if !std::io::stdin().is_terminal() {
        let mut buf = [0u8; 8192];
        while crate::term::readable_within(0, std::time::Duration::from_millis(100)) {
            match std::io::Read::read(&mut std::io::stdin().lock(), &mut buf) {
                Ok(n) if n > 0 => {}
                _ => break,
            }
        }
    }
    let pool_memory = pool();
    let own = crate::mem::read();
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
    let (used, total) = memory(pool_memory, &own, meminfo.as_deref());
    let swap = swap(
        pool_memory.map(|_| pool_swap()),
        &own,
        crate::mem::swap(),
        meminfo.as_deref(),
    );
    let load = host_load();
    let figures = Figures {
        used,
        total,
        swap,
        load: load.as_ref().map(|l| l.centi),
        cores: load.and_then(|l| l.cores),
        place: std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_string())
            .unwrap_or_default(),
    };
    let colour = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    say!("{}", figures.line(colour));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Header;

    const G: u64 = 1024 * 1024 * 1024;

    fn figures(
        used: Option<u64>,
        total: Option<u64>,
        load: Option<f64>,
        cores: Option<usize>,
        place: &str,
    ) -> Figures {
        Figures {
            used,
            total,
            swap: Some((G / 2, 4 * G)),
            load: load.map(|l| (l * 100.0).round() as u64),
            cores,
            place: place.to_string(),
        }
    }

    #[test]
    fn it_reads_as_the_owner_asked() {
        assert_eq!(
            figures(Some(G), Some(3 * G), Some(2.31), Some(4), "or3-dev").line(true),
            "RAM: 1.0G / 3.0G, Swap: 0.5G / 4.0G, Load: 2.3, or3-dev"
        );
    }

    #[test]
    fn ram_and_load_turn_yellow_then_red() {
        let ram = |u| figures(Some(u), Some(100 * G), Some(0.1), Some(4), "h").line(true);
        assert!(!ram(69 * G).contains('\x1b'), "under 70%: plain");
        assert!(
            ram(70 * G).starts_with(&format!("{YELLOW}RAM: ")),
            "{:?}",
            ram(70 * G)
        );
        assert!(ram(85 * G).starts_with(&format!("{RED}RAM: ")));
        // Load against cores: 2.7 on 4 is under 0.7 a core; 2.8 is at it; 4.0 is one a core.
        let load = |l| figures(Some(G), Some(100 * G), Some(l), Some(4), "h").line(true);
        assert!(!load(2.7).contains('\x1b'));
        assert!(
            load(2.8).contains(&format!("{YELLOW}Load: 2.8{RESET}")),
            "{:?}",
            load(2.8)
        );
        assert!(load(4.0).contains(&format!("{RED}Load: 4.0{RESET}")));
        // The same load on 8 cores is calm: it is the per-core figure that counts.
        assert!(
            !figures(Some(G), Some(100 * G), Some(4.0), Some(8), "h")
                .line(true)
                .contains('\x1b')
        );
        // No colour asked for: plain whatever the figures.
        assert_eq!(
            figures(Some(99 * G), Some(100 * G), Some(9.0), Some(1), "h").line(false),
            "RAM: 99.0G / 100.0G, Swap: 0.5G / 4.0G, Load: 9.0, h"
        );
    }

    #[test]
    fn what_cannot_be_read_is_a_question_mark() {
        assert_eq!(
            figures(None, Some(G), None, None, "").line(true),
            "RAM: ?, Swap: 0.5G / 4.0G, Load: ?, ?"
        );
        assert_eq!(
            figures(Some(G / 2), Some(G), Some(0.0), None, "").line(true),
            "RAM: 0.5G / 1.0G, Swap: 0.5G / 4.0G, Load: 0.0, ?"
        );
    }

    #[test]
    fn swap_is_said_beside_ram_at_the_same_thresholds() {
        let with = |swap| Figures {
            swap,
            ..figures(Some(G), Some(100 * G), Some(0.1), Some(4), "h")
        };
        assert_eq!(
            with(Some((G / 4, 4 * G))).line(true),
            "RAM: 1.0G / 100.0G, Swap: 0.2G / 4.0G, Load: 0.1, h"
        );
        assert!(!with(Some((69, 100))).line(true).contains('\x1b'));
        assert!(
            with(Some((70, 100)))
                .line(true)
                .contains(&format!("{YELLOW}Swap: "))
        );
        assert!(
            with(Some((85, 100)))
                .line(true)
                .contains(&format!("{RED}Swap: "))
        );
        // No swap on the machine is said as such; unreadable is a question mark.
        assert_eq!(with(Some((0, 0))).swap().0, "Swap: none");
        assert_eq!(with(None).swap().0, "Swap: ?");
        assert_eq!(with(Some((5, 0))).swap().0, "Swap: ?");
    }

    #[test]
    fn a_cgroups_swap_is_capped_at_the_hosts() {
        let mi = "SwapTotal: 4 kB\nSwapFree: 3 kB\n";
        assert_eq!(host_swap(mi), Some((4096, 3072)));
        assert_eq!(host_swap("SwapTotal: 4 kB\n"), None);
        // A ceiling above the host's swap is the host's; below it, the ceiling.
        assert_eq!(
            cgroup_swap((Some(10), Some(1 << 20)), Some(mi)),
            Some((10, 4096))
        );
        assert_eq!(
            cgroup_swap((Some(10), Some(100)), Some(mi)),
            Some((10, 100))
        );
        // No ceiling: the host's; neither, or no use read: unknown.
        assert_eq!(cgroup_swap((Some(10), None), Some(mi)), Some((10, 4096)));
        assert_eq!(cgroup_swap((Some(10), Some(100)), None), Some((10, 100)));
        assert_eq!(cgroup_swap((Some(10), None), None), None);
        assert_eq!(cgroup_swap((None, Some(100)), Some(mi)), None);
    }

    #[test]
    fn a_cgroups_swap_files_are_read() {
        let t = Tree::new("swap");
        t.put("memory.swap.current", "4644864\n")
            .put("memory.swap.max", "max\n");
        assert_eq!(crate::mem::swap_at(&t.dir()), (Some(4644864), None));
        t.put("memory.swap.max", "4294967296\n");
        assert_eq!(
            crate::mem::swap_at(&t.dir()),
            (Some(4644864), Some(4294967296))
        );
        assert_eq!(crate::mem::swap_at("/nonexistent-cs"), (None, None));
    }

    #[test]
    fn swap_is_the_pools_else_the_containers_else_the_hosts() {
        let own = |limit| crate::mem::Memory {
            limit,
            current: Some(300),
            reclaimable: None,
        };
        let mi = "SwapTotal: 4 kB\nSwapFree: 3 kB\n";
        let cg = (Some(7), Some(100));
        // The pool's memory read: its swap, even unknown, never the container's.
        assert_eq!(
            swap(Some(Some((1, 2))), &own(Some(9)), cg, Some(mi)),
            Some((1, 2))
        );
        assert_eq!(swap(Some(None), &own(Some(9)), cg, Some(mi)), None);
        assert_eq!(swap(None, &own(Some(9)), cg, Some(mi)), Some((7, 100)));
        assert_eq!(swap(None, &own(None), cg, Some(mi)), Some((1024, 4096)));
        assert_eq!(swap(None, &own(None), cg, None), None);
    }

    #[test]
    fn the_cores_are_counted_from_the_online_list() {
        assert_eq!(cpu_count("0-3\n"), Some(4));
        assert_eq!(cpu_count("0-3,6,8-9"), Some(7));
        assert_eq!(cpu_count("0"), Some(1));
        assert_eq!(cpu_count(""), None);
        assert_eq!(cpu_count("x"), None);
    }

    #[test]
    fn the_hosts_figures_come_from_meminfo() {
        let mi = "MemTotal:        4045112 kB\nMemFree:          100000 kB\nMemAvailable:    1022556 kB\n";
        assert_eq!(host_memory(mi), Some((4_045_112 * 1024, 1_022_556 * 1024)));
        assert_eq!(host_memory("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn the_load_average_is_read_in_hundredths() {
        assert_eq!(loadavg_centi("2.31 1.80 1.20 3/512 4242\n"), Some(231));
        assert_eq!(loadavg_centi("0.00 0.00 0.00 1/1 1"), Some(0));
        assert_eq!(loadavg_centi(""), None);
        assert_eq!(loadavg_centi("nan 0 0"), None);
        assert_eq!(loadavg_centi("inf 0 0"), None);
        assert_eq!(loadavg_centi("-1.00 0 0"), None);
    }

    #[test]
    fn a_ceiling_of_nothing_and_no_cores_judge_nothing() {
        // A zero total is no figure, not a division; zero cores is no yardstick.
        assert_eq!(
            figures(Some(G), Some(0), Some(9.0), Some(0), "h").line(true),
            "RAM: ?, Swap: 0.5G / 4.0G, Load: 9.0, h"
        );
        // Without colour a warning is as plain as an alarm.
        assert_eq!(
            figures(Some(70 * G), Some(100 * G), Some(2.8), Some(4), "h").line(false),
            "RAM: 70.0G / 100.0G, Swap: 0.5G / 4.0G, Load: 2.8, h"
        );
    }

    /// A directory standing in for `/`, or for a cgroup, removed when dropped.
    struct Tree(std::path::PathBuf);
    impl Tree {
        fn new(tag: &str) -> Tree {
            let d =
                std::env::temp_dir().join(format!("cs-statusline-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tree(d)
        }
        fn put(&self, rel: &str, text: &str) -> &Tree {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
            self
        }
        fn dir(&self) -> String {
            self.0.display().to_string()
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_machine_is_read_from_proc_and_sys_and_named_by_its_boot_id() {
        let t = Tree::new("host");
        t.put("proc/loadavg", "1.25 1 1 1/1 1\n")
            .put("sys/devices/system/cpu/online", "0-3\n")
            .put("proc/sys/kernel/random/boot_id", "b-1\n")
            .put(
                "proc/meminfo",
                "MemTotal: 4000 kB\nMemFree: 100 kB\nMemAvailable: 2500 kB\nSwapTotal: 10000 kB\nSwapFree: 9000 kB\n",
            );
        assert_eq!(
            host_load_at(&t.0),
            Some(HostLoad {
                centi: 125,
                cores: Some(4),
                boot: "b-1".into(),
                memory: Some((1500 * 1024, 4000 * 1024)),
                swap: Some((1000 * 1024, 10000 * 1024)),
            })
        );
        // The memory is a nicety too: without meminfo the reading still counts the load.
        let _ = std::fs::remove_file(t.0.join("proc/meminfo"));
        assert_eq!(
            host_load_at(&t.0).map(|h| (h.centi, h.memory, h.swap)),
            Some((125, None, None))
        );
        // Cores are a nicety; the boot id is not, since without it the load cannot be told
        // from another container's reading of the same machine.
        t.put("sys/devices/system/cpu/online", "");
        assert_eq!(host_load_at(&t.0).map(|h| h.cores), Some(None));
        t.put("proc/sys/kernel/random/boot_id", "\n");
        assert_eq!(host_load_at(&t.0), None);
        let _ = std::fs::remove_file(t.0.join("proc/sys/kernel/random/boot_id"));
        assert_eq!(host_load_at(&t.0), None);
    }

    #[test]
    fn the_pool_is_its_working_set_against_its_ceiling() {
        let t = Tree::new("pool");
        t.put("memory.max", "4000\n")
            .put("memory.current", "3000\n")
            .put("memory.stat", "active_file 7\ninactive_file 500\n");
        assert_eq!(pool_at(&t.dir()), Some((2500, 4000)));
        // A pool without a ceiling is no pool to measure against, and no directory is none.
        t.put("memory.max", "max\n");
        assert_eq!(pool_at(&t.dir()), None);
        assert_eq!(pool_at(""), None);
    }

    #[test]
    fn memory_is_the_pool_else_the_container_else_the_host() {
        let own = |limit| crate::mem::Memory {
            limit,
            current: Some(300),
            reclaimable: Some(100),
        };
        let mi = "MemTotal: 4 kB\nMemAvailable: 1 kB\n";
        assert_eq!(
            memory(Some((5, 9)), &own(Some(1000)), Some(mi)),
            (Some(5), Some(9))
        );
        assert_eq!(
            memory(None, &own(Some(1000)), Some(mi)),
            (Some(200), Some(1000))
        );
        assert_eq!(memory(None, &own(None), Some(mi)), (Some(3072), Some(4096)));
        assert_eq!(memory(None, &own(None), None), (None, None));
    }

    fn on(boot: &str, centi: u64, cores: Option<usize>) -> HostLoad {
        HostLoad {
            centi,
            cores,
            boot: boot.to_string(),
            memory: None,
            swap: None,
        }
    }

    /// A reading from a machine with `total` of RAM, `used` of it, and ten times as much swap,
    /// a tenth used.
    fn machine(boot: &str, used: u64, total: u64) -> HostLoad {
        HostLoad {
            memory: Some((used, total)),
            swap: Some((total, 10 * total)),
            ..on(boot, 100, Some(4))
        }
    }

    #[test]
    fn a_machines_memory_is_counted_once_however_many_containers_read_it() {
        // Three containers on one 4 GB Pi each read its one figure: 4 GB, not 12.
        let pi = [
            machine("pi", G + G / 2, 4 * G),
            machine("pi", G, 4 * G),
            machine("pi", 2 * G, 4 * G),
        ];
        assert_eq!(
            total_memory(&pi),
            (Some((G + G / 2, 4 * G)), Some((4 * G, 40 * G)))
        );
        // Calibration: two machines add up, and a reading too old to carry the figure leaves
        // its machine to one that does, wherever it comes in the order.
        let two = [
            on("pi", 300, Some(4)),
            machine("nuc", 8 * G, 32 * G),
            machine("pi", G, 4 * G),
        ];
        assert_eq!(
            total_memory(&two),
            (Some((9 * G, 36 * G)), Some((36 * G, 360 * G)))
        );
        // No reading carries it: unknown, not zero.
        assert_eq!(total_memory(&[on("pi", 300, Some(4))]), (None, None));
        assert_eq!(total_memory(&[]), (None, None));
    }

    #[test]
    fn a_machine_is_counted_once_however_many_containers_read_it() {
        // Three containers on one Pi read its one figure: the total is that figure, not three
        // of it. The first reading of a machine wins, so the freshest is put first.
        let zero = [
            on("pi", 230, Some(4)),
            on("pi", 210, Some(4)),
            on("pi", 200, Some(4)),
        ];
        assert_eq!(total_load(&zero), Some((230, Some(4))));
        // Calibration: two machines do add up, load and cores.
        let two = [
            on("pi", 230, Some(4)),
            on("nuc", 150, Some(8)),
            on("pi", 0, Some(4)),
        ];
        assert_eq!(total_load(&two), Some((380, Some(12))));
        // Cores only from the machines that said; none at all is none.
        assert_eq!(
            total_load(&[on("a", 100, None), on("b", 100, Some(2))]),
            Some((200, Some(2)))
        );
        assert_eq!(total_load(&[on("a", 100, None)]), Some((100, None)));
        assert_eq!(total_load(&[]), None);
    }

    fn container(memory: Option<(u64, u64)>, load: Option<HostLoad>) -> Header {
        Header {
            host: "c".to_string(),
            memory,
            swap: memory.map(|_| (G / 4, 2 * G)),
            load,
        }
    }

    #[test]
    fn the_door_totals_the_pool_or_else_every_container() {
        let a = container(Some((G, 2 * G)), Some(on("pi", 200, Some(4))));
        let b = container(Some((G / 2, G)), Some(on("pi", 150, Some(4))));
        let old = container(None, None);
        // The pool, where the hub can read it, is the memory figure; the hub's own load is
        // the machine's, and the containers on it add nothing.
        let f = door(
            Some(on("pi", 250, Some(4))),
            Some((2 * G, 3 * G)),
            Some((G / 2, 4 * G)),
            [&a, &b, &old],
            "dev",
        );
        assert_eq!(
            f.line(false),
            "RAM: 2.0G / 3.0G, Swap: 0.5G / 4.0G, Load: 2.5, dev"
        );
        // No pool, and no reading says what the machine has (agents from before 2026-11-08): what
        // each container said, added up; a container too old to say anything is left out.
        let f = door(None, None, None, [&a, &b, &old], "dev");
        assert_eq!(
            f.line(false),
            "RAM: 1.5G / 3.0G, Swap: 0.5G / 4.0G, Load: 2.0, dev"
        );
        // Nothing said and nothing read: question marks, not zeros.
        assert_eq!(
            door(None, None, None, [&old], "dev").line(false),
            "RAM: ?, Swap: ?, Load: ?, dev"
        );
    }

    #[test]
    fn the_door_says_what_the_machines_have_not_their_containers_ceilings_added_up() {
        // Eight containers with 2 GB ceilings and 4 GB of swap each on one 4 GB Pi with 10 GB
        // of swap: not 16 GB and 32 GB (owner, 2026-11-08), but the Pi's own figures, read by
        // the hub and by every container alike, counted once.
        let pi = HostLoad {
            memory: Some((G + G / 2, 4 * G)),
            swap: Some((G, 10 * G)),
            ..on("pi", 200, Some(4))
        };
        let fleet: Vec<Header> = (0..8)
            .map(|_| Header {
                host: "c".to_string(),
                memory: Some((G / 2, 2 * G)),
                swap: Some((G / 2, 4 * G)),
                load: Some(pi.clone()),
            })
            .collect();
        let f = door(Some(pi.clone()), None, None, &fleet, "zero");
        assert_eq!(
            f.line(false),
            "RAM: 1.5G / 4.0G, Swap: 1.0G / 10.0G, Load: 2.0, zero"
        );
        // Calibration: the ceilings do add up to the wrong figure where no reading, the hub's
        // or a container's, says what the machine has.
        let old_fleet: Vec<Header> = fleet
            .iter()
            .map(|h| Header {
                load: Some(on("pi", 200, Some(4))),
                ..h.clone()
            })
            .collect();
        let f = door(Some(on("pi", 200, Some(4))), None, None, &old_fleet, "zero");
        assert_eq!(f.ram().0, "RAM: 4.0G / 16.0G");
        assert_eq!(f.swap().0, "Swap: 4.0G / 32.0G");
        // One container's agent saying what the Pi has is enough to put that right.
        let f = door(None, None, None, [&old_fleet[0], &fleet[0]], "zero");
        assert_eq!(f.ram().0, "RAM: 1.5G / 4.0G");
        // The hub's own reading alone says it, with no container answering; and a second
        // machine's containers add theirs.
        assert_eq!(
            door(Some(pi.clone()), None, None, [], "zero").ram().0,
            "RAM: 1.5G / 4.0G"
        );
        let nuc = Header {
            load: Some(HostLoad {
                memory: Some((8 * G, 32 * G)),
                swap: Some((0, 0)),
                ..on("nuc", 100, Some(8))
            }),
            ..fleet[0].clone()
        };
        let f = door(Some(pi), None, None, [&fleet[0], &nuc], "zero");
        assert_eq!(
            f.line(false),
            "RAM: 9.5G / 36.0G, Swap: 1.0G / 10.0G, Load: 3.0, zero"
        );
        // The pool, where the hub reads one, still comes first.
        let f = door(
            Some(machine("pi", G, 4 * G)),
            Some((2 * G, 3 * G)),
            Some((G / 2, 4 * G)),
            &fleet,
            "zero",
        );
        assert_eq!(f.ram().0, "RAM: 2.0G / 3.0G");
        assert_eq!(f.swap().0, "Swap: 0.5G / 4.0G");
    }
}
