//! The line Claude Code draws under its prompt, so memory is in view while you work rather
//! than only in the menu: `RAM: 1.0G / 3.0G, Load: 2.3, or3-dev` (owner, 2026-10-06; 36
//! columns, inside the 40 a phone shows).
//!
//! RAM is the menu's figure — the container's working set against its limit, page cache the
//! kernel would reclaim first set aside (`mem.rs`). Where the container has no limit, the
//! host's own total and available memory are the honest figures, and are used instead. Load
//! is the one-minute load average, and the last part is the hostname, which names the dev
//! container. Anything that cannot be read is a `?`, never a guess. ASCII only: this lands in
//! whatever font the terminal has (CLAUDE.md, on glyphs).
//!
//! **Colour** (owner, 2026-10-06): yellow, then red, as RAM passes 70% and 85% of its total
//! and as the load passes 0.7 and 1.0 per logical core of the machine — the machine's, because
//! the load average counts the whole machine too. The numbers carry the meaning;
//! colour is emphasis on top, and `NO_COLOR` turns it off. Yellow here is the owner's choice
//! for this line; the menu's amber stays reserved for *waiting for you*.
//!
//! Claude Code runs it on its own events and every 60 s (`hooks_config`), so it reads a few
//! small files and nothing else — no registry, no `zmx list`.

use std::io::IsTerminal as _;

const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Yellow from the first threshold, red from the second.
fn shade(value: f64, warn: f64, high: f64) -> Option<&'static str> {
    if value >= high {
        Some(RED)
    } else if value >= warn {
        Some(YELLOW)
    } else {
        None
    }
}

fn paint(text: String, colour: Option<&str>) -> String {
    match colour {
        Some(c) => format!("{c}{text}{RESET}"),
        None => text,
    }
}

/// The line, from what was read. `colour` false gives plain text.
pub fn line(
    used: Option<u64>,
    total: Option<u64>,
    load: Option<f64>,
    cores: Option<usize>,
    host: Option<&str>,
    colour: bool,
) -> String {
    let ram = match (used, total) {
        (Some(u), Some(t)) if t > 0 => paint(
            format!("RAM: {} / {}", gib(u), gib(t)),
            shade(u as f64 / t as f64, 0.70, 0.85).filter(|_| colour),
        ),
        _ => "RAM: ?".to_string(),
    };
    let load = match load {
        Some(l) => paint(
            format!("Load: {l:.1}"),
            cores
                .filter(|&c| c > 0)
                .and_then(|c| shade(l / c as f64, 0.7, 1.0))
                .filter(|_| colour),
        ),
        None => "Load: ?".to_string(),
    };
    let host = host.filter(|h| !h.is_empty()).unwrap_or("?");
    format!("{ram}, {load}, {host}")
}

fn gib(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
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
    let m = crate::mem::read();
    let (used, total) = match m.limit {
        Some(limit) => (m.used(), Some(limit)),
        None => match std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| host_memory(&s))
        {
            Some((total, available)) => (Some(total.saturating_sub(available)), Some(total)),
            None => (None, None),
        },
    };
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    let cores = std::fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .and_then(|s| cpu_count(&s));
    let colour = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    say!(
        "{}",
        line(
            used,
            total,
            load,
            cores,
            host.as_deref().map(str::trim),
            colour
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1024 * 1024 * 1024;

    #[test]
    fn it_reads_as_the_owner_asked() {
        assert_eq!(
            line(
                Some(G),
                Some(3 * G),
                Some(2.31),
                Some(4),
                Some("or3-dev"),
                true
            ),
            "RAM: 1.0G / 3.0G, Load: 2.3, or3-dev"
        );
    }

    #[test]
    fn ram_and_load_turn_yellow_then_red() {
        let ram = |u| line(Some(u), Some(100 * G), Some(0.1), Some(4), Some("h"), true);
        assert!(!ram(69 * G).contains('\x1b'), "under 70%: plain");
        assert!(
            ram(70 * G).starts_with(&format!("{YELLOW}RAM: ")),
            "{:?}",
            ram(70 * G)
        );
        assert!(ram(85 * G).starts_with(&format!("{RED}RAM: ")));
        // Load against cores: 2.7 on 4 is under 0.7 a core; 2.8 is at it; 4.0 is one a core.
        let load = |l| line(Some(G), Some(100 * G), Some(l), Some(4), Some("h"), true);
        assert!(!load(2.7).contains('\x1b'));
        assert!(
            load(2.8).contains(&format!("{YELLOW}Load: 2.8{RESET}")),
            "{:?}",
            load(2.8)
        );
        assert!(load(4.0).contains(&format!("{RED}Load: 4.0{RESET}")));
        // The same load on 8 cores is calm: it is the per-core figure that counts.
        assert!(
            !line(Some(G), Some(100 * G), Some(4.0), Some(8), Some("h"), true).contains('\x1b')
        );
        // No colour asked for: plain whatever the figures.
        assert_eq!(
            line(
                Some(99 * G),
                Some(100 * G),
                Some(9.0),
                Some(1),
                Some("h"),
                false
            ),
            "RAM: 99.0G / 100.0G, Load: 9.0, h"
        );
    }

    #[test]
    fn what_cannot_be_read_is_a_question_mark() {
        assert_eq!(
            line(None, Some(G), None, None, None, true),
            "RAM: ?, Load: ?, ?"
        );
        assert_eq!(
            line(Some(G / 2), Some(G), Some(0.0), None, Some(""), true),
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
}
