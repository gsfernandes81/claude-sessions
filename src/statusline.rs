//! The load line: `RAM: 1.9G / 3.0G, Load: 2.3, or3-dev` (owner, 2026-10-06; 36 columns,
//! inside the 40 a phone shows). Claude Code draws it under its prompt, so memory is in view
//! while you work, and the menu draws the same line at the foot of the door (owner,
//! 2026-10-27), roughly where Claude Code puts it.
//!
//! **The figures are every container's, wherever the line is read** (owner, 2026-10-27:
//! "total ram and total cpu in all sessions"). RAM is the dev containers' shared pool where it
//! is mounted (`CLAUDE_SESSIONS_POOL`, a cgroup v2 directory, read as the menu reads a
//! container: the working set, page cache the kernel reclaims first set aside — `mem.rs`).
//! Where it is not, a session's line falls back to its own container's limit, or the host's
//! total and available memory where there is no limit, and the door to the sum of what each
//! container's agent said. Load is the one-minute load average, **counted once per machine**:
//! `/proc/loadavg` is not namespaced, so every container on one host reads the same host-wide
//! figure, and adding them would count that host once per container. The kernel's `boot_id`
//! says which machine a reading is from ([`total_load`]). The last part names where the line is
//! read: the container's hostname in a session, the hub's at the door. Anything that cannot
//! be read is a `?`, never a guess. ASCII only: this lands in whatever font the terminal has
//! (CLAUDE.md, on glyphs).
//!
//! **Colour** (owner, 2026-10-06): yellow, then red, as RAM passes 70% and 85% of its total
//! and as the load passes 0.7 and 1.0 per logical core — the machines', because the load
//! average counts the whole machine too. The numbers carry the meaning; colour is emphasis on
//! top, and `NO_COLOR` turns it off. The menu draws the same levels in its own vocabulary
//! (`render.rs`), since its amber is reserved for *waiting for you*.
//!
//! Claude Code runs it on its own events and every 60 s (`hooks_config`), so it reads a few
//! small files and nothing else — no registry, no `zmx list`.

use crate::ui::HostLoad;
use std::io::IsTerminal as _;

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
            "{}, {}, {}",
            paint(self.ram()),
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

/// The door's figures: the pool's memory where the hub can read it, else every container's
/// working set and ceiling added up — those that said; and the load of every machine, the
/// hub's own reading first, since it is the freshest (a container's arrives with its rows).
pub fn door<'a>(
    own: Option<HostLoad>,
    pool: Option<(u64, u64)>,
    containers: impl IntoIterator<Item = &'a crate::ui::Header>,
    place: &str,
) -> Figures {
    let containers: Vec<&crate::ui::Header> = containers.into_iter().collect();
    let (used, total) = match pool {
        Some((used, total)) => (Some(used), Some(total)),
        None => {
            let said: Vec<(u64, u64)> = containers.iter().filter_map(|h| h.memory).collect();
            if said.is_empty() {
                (None, None)
            } else {
                (
                    Some(said.iter().map(|m| m.0).sum()),
                    Some(said.iter().map(|m| m.1).sum()),
                )
            }
        }
    };
    let total_load = total_load(
        own.iter()
            .chain(containers.iter().filter_map(|h| h.load.as_ref())),
    );
    Figures {
        used,
        total,
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
    (n > 0).then_some(n)
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

/// The one-minute load average from `/proc/loadavg`, in hundredths.
fn loadavg_centi(text: &str) -> Option<u64> {
    let l: f64 = text.split_whitespace().next()?.parse().ok()?;
    (l.is_finite() && l >= 0.0).then(|| (l * 100.0).round() as u64)
}

/// This machine's load, as this container reads it. `None` without a load average or a
/// `boot_id`: a reading that cannot say which machine it is could only be double-counted.
pub fn host_load() -> Option<HostLoad> {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    Some(HostLoad {
        centi: loadavg_centi(&read("/proc/loadavg")?)?,
        cores: read("/sys/devices/system/cpu/online").and_then(|s| cpu_count(&s)),
        boot: read("/proc/sys/kernel/random/boot_id")?.trim().to_string(),
    })
    .filter(|h| !h.boot.is_empty())
}

/// The pool's working set and ceiling, where infra mounts it (`CLAUDE_SESSIONS_POOL`) and it
/// has a ceiling.
pub fn pool() -> Option<(u64, u64)> {
    let dir = std::env::var("CLAUDE_SESSIONS_POOL")
        .ok()
        .filter(|d| !d.is_empty())?;
    let m = crate::mem::read_at(&dir);
    m.limit.zip(m.used()).map(|(limit, used)| (used, limit))
}

/// What a session's line shows for memory: the pool, else this container against its limit,
/// else the host's total and what it has available.
fn memory() -> (Option<u64>, Option<u64>) {
    if let Some((used, total)) = pool() {
        return (Some(used), Some(total));
    }
    let m = crate::mem::read();
    match m.limit {
        Some(limit) => (m.used(), Some(limit)),
        None => match std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| host_memory(&s))
        {
            Some((total, available)) => (Some(total.saturating_sub(available)), Some(total)),
            None => (None, None),
        },
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
    let (used, total) = memory();
    let load = host_load();
    let figures = Figures {
        used,
        total,
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
            load: load.map(|l| (l * 100.0).round() as u64),
            cores,
            place: place.to_string(),
        }
    }

    #[test]
    fn it_reads_as_the_owner_asked() {
        assert_eq!(
            figures(Some(G), Some(3 * G), Some(2.31), Some(4), "or3-dev").line(true),
            "RAM: 1.0G / 3.0G, Load: 2.3, or3-dev"
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
            "RAM: 99.0G / 100.0G, Load: 9.0, h"
        );
    }

    #[test]
    fn what_cannot_be_read_is_a_question_mark() {
        assert_eq!(
            figures(None, Some(G), None, None, "").line(true),
            "RAM: ?, Load: ?, ?"
        );
        assert_eq!(
            figures(Some(G / 2), Some(G), Some(0.0), None, "").line(true),
            "RAM: 0.5G / 1.0G, Load: 0.0, ?"
        );
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
    }

    fn on(boot: &str, centi: u64, cores: Option<usize>) -> HostLoad {
        HostLoad {
            centi,
            cores,
            boot: boot.to_string(),
        }
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
            [&a, &b, &old],
            "dev",
        );
        assert_eq!(f.line(false), "RAM: 2.0G / 3.0G, Load: 2.5, dev");
        // No pool: what each container said, added up; a container too old to say is left out.
        let f = door(None, None, [&a, &b, &old], "dev");
        assert_eq!(f.line(false), "RAM: 1.5G / 3.0G, Load: 2.0, dev");
        // Nothing said and nothing read: question marks, not zeros.
        assert_eq!(
            door(None, None, [&old], "dev").line(false),
            "RAM: ?, Load: ?, dev"
        );
    }
}
