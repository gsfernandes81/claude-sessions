//! The two number formats every screen shares, so the list, the header, `doctor` and the
//! dialogs can never disagree about what `14m` or `1.0G` means.

/// An age as the list shows it: `now` under a minute, then whole minutes, hours, days.
/// Coarse on purpose — the menu redraws an age at most once a minute, and a finer unit would
/// either lie or cost bytes on a metered link.
pub fn age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Bytes as the header shows them: whole megabytes below a gigabyte, one decimal above —
/// `812M`, `1.0G`.
pub fn human(bytes: u64) -> String {
    let mb = bytes / (1024 * 1024);
    if mb >= 1024 {
        format!("{:.1}G", mb as f64 / 1024.0)
    } else {
        format!("{mb}M")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_as_the_mockups_draw_them() {
        assert_eq!(age(59_999), "now");
        assert_eq!(age(2 * 60_000), "2m");
        assert_eq!(age(5 * 3_600_000), "5h");
        assert_eq!(age(2 * 86_400_000), "2d");
    }

    #[test]
    fn memory_reads_as_the_header_draws_it() {
        assert_eq!(human(812 * 1024 * 1024), "812M");
        assert_eq!(human(1024 * 1024 * 1024), "1.0G");
        assert_eq!(human(892 * 1024 * 1024), "892M");
    }
}
