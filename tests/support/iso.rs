//! Transcript timestamps, as the tests that write transcripts need them.

/// `ms` as Claude Code writes a transcript's `timestamp`: `2026-10-01T00:00:00.000Z`. The
/// stored conversations are dated from now, not from a fixed day, because 30 days unused
/// archives one: a fixed date would move a listed row into the archive a month later and
/// fail this test for no change at all. Days to civil date as in Howard Hinnant's
/// `civil_from_days`.
pub fn iso(ms: u64) -> String {
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
    let (h, mi, s, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1_000 % 60,
        rem % 1_000,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}
