//! The two number formats every screen shares, so the list, the header, `doctor` and the
//! dialogs can never disagree about what `14m` or `1.0G` means — and the two UTC date formats
//! mail is stamped with.

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

/// Milliseconds since the epoch as a civil UTC date: `(year, month, day, hour, minute,
/// second, millisecond)`. Days to civil date as in Howard Hinnant's `civil_from_days`.
fn civil(ms: u64) -> (i64, i64, i64, u64, u64, u64, u64) {
    let (days, rem) = ((ms / 86_400_000) as i64, ms % 86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1_000 % 60,
        rem % 1_000,
    )
}

/// `ms` as a `Date:` header reads: `2026-10-28T03:15:00Z`, whole seconds.
pub fn utc(ms: u64) -> String {
    let (y, m, d, h, mi, s, _) = civil(ms);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `ms` as a file name that sorts by time: `20261028T031500.123Z` — no colons, which a
/// Windows checkout and a shell both mind, and milliseconds, since two messages in one
/// second are ordinary.
pub fn stamp(ms: u64) -> String {
    let (y, m, d, h, mi, s, milli) = civil(ms);
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}.{milli:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_and_the_stamp_sorts_by_time() {
        // 2026-10-28T03:15:00.123Z, checked against `date -u -d @1793157300`.
        let ms = 1_793_157_300_123;
        assert_eq!(utc(ms), "2026-10-28T03:15:00Z");
        assert_eq!(stamp(ms), "20261028T031500.123Z");
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        // A leap day, and the year turning.
        assert_eq!(utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        assert_eq!(utc(1_767_225_599_000), "2025-12-31T23:59:59Z");
        assert!(stamp(ms) < stamp(ms + 1));
    }

    #[test]
    fn every_day_to_2500_follows_the_one_before() {
        // Day by day from 1970, each date is the day after the last: across month ends, leap
        // days, 2100 (no leap day) and 2400 (a leap day, the last day of an era).
        let leap = |y: i64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let days_in = |y: i64, m: i64| match m {
            2 if leap(y) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        let mut was = (1970, 1, 1);
        let mut seen_2400_leap = false;
        for day in 1..(531 * 366) {
            let (y, m, d, ..) = civil(day * 86_400_000);
            let (py, pm, pd) = was;
            let next = if pd < days_in(py, pm) {
                (py, pm, pd + 1)
            } else if pm < 12 {
                (py, pm + 1, 1)
            } else {
                (py + 1, 1, 1)
            };
            assert_eq!((y, m, d), next, "day {day}");
            seen_2400_leap |= (y, m, d) == (2400, 2, 29);
            was = (y, m, d);
        }
        assert!(seen_2400_leap && was.0 >= 2500, "{was:?}");
    }

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
