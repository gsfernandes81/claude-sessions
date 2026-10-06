//! The line Claude Code draws under its prompt, so memory is in view while you work rather
//! than only in the menu: `RAM: 1.0G / 3.0G used, Load: 2.3, or3-dev` (owner, 2026-10-06).
//!
//! RAM is the menu's figure — the container's working set against its limit, page cache the
//! kernel would reclaim first set aside (`mem.rs`). Where the container has no limit, the
//! host's own total and available memory are the honest figures, and are used instead. Load
//! is the one-minute load average, and the last part is the hostname, which names the dev
//! container. Anything that cannot be read is a `?`, never a guess. ASCII only: this lands in
//! whatever font the terminal has (CLAUDE.md, on glyphs).
//!
//! Claude Code runs it often, so it reads a few small files and nothing else — no registry,
//! no `zmx list`.

use std::io::IsTerminal as _;

/// The line, from what was read.
pub fn line(
    used: Option<u64>,
    total: Option<u64>,
    load: Option<f64>,
    host: Option<&str>,
) -> String {
    let ram = match (used, total) {
        (Some(u), Some(t)) => format!("{} / {} used", gib(u), gib(t)),
        _ => "?".to_string(),
    };
    let load = load.map_or("?".to_string(), |l| format!("{l:.1}"));
    let host = host.filter(|h| !h.is_empty()).unwrap_or("?");
    format!("RAM: {ram}, Load: {load}, {host}")
}

fn gib(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
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

/// Read, and say the line. Claude Code writes its session as JSON on stdin; it is drained so
/// the writer never meets a closed pipe, but not read when a person runs this at a terminal.
pub fn run() {
    if !std::io::stdin().is_terminal() {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
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
    say!(
        "{}",
        line(used, total, load, host.as_deref().map(str::trim))
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1024 * 1024 * 1024;

    #[test]
    fn it_reads_as_the_owner_asked() {
        assert_eq!(
            line(Some(G), Some(3 * G), Some(2.31), Some("or3-dev")),
            "RAM: 1.0G / 3.0G used, Load: 2.3, or3-dev"
        );
    }

    #[test]
    fn what_cannot_be_read_is_a_question_mark() {
        assert_eq!(line(None, Some(G), None, None), "RAM: ?, Load: ?, ?");
        assert_eq!(
            line(Some(G / 2), Some(G), Some(0.0), Some("")),
            "RAM: 0.5G / 1.0G used, Load: 0.0, ?"
        );
    }

    #[test]
    fn the_hosts_figures_come_from_meminfo() {
        let mi = "MemTotal:        4045112 kB\nMemFree:          100000 kB\nMemAvailable:    1022556 kB\n";
        assert_eq!(host_memory(mi), Some((4_045_112 * 1024, 1_022_556 * 1024)));
        assert_eq!(host_memory("MemTotal: 1 kB\n"), None);
    }
}
