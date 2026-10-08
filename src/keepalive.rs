//! `claude-sessions keepalive <duration>`: keep the slot this runs in from being offloaded for
//! a while.
//!
//! The offloader acts on measured activity alone (`activity.rs`), so work that waits without
//! reading, writing or computing — a sleep before a check, a remote job, claude's own wake-up —
//! reads as quiet and is stopped after ten minutes. Claude runs this before such a wait (the
//! keep-alive skill, `skills/keepalive/SKILL.md`, says when). The duration is mandatory and
//! capped, so a keep-alive nobody ends still ends.

use crate::clock::{self, Millis};
use crate::{bind, lockfile, registry};
use std::io;

/// The longest keep-alive one call may ask for.
pub const MAX_MS: Millis = 12 * 60 * 60 * 1000;

/// `90s`, `25m`, `2h`, or a bare number of seconds; `0` ends a keep-alive. Over [`MAX_MS`]
/// is refused rather than cut, so a caller that asked for more knows it did not get it.
pub fn parse(s: &str) -> Result<Millis, String> {
    let s = s.trim();
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => s.split_at(at),
        None => (s, "s"),
    };
    let n: Millis = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration: use 90s, 25m or 2h"))?;
    let unit_ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(format!("{s:?} is not a duration: use 90s, 25m or 2h")),
    };
    let ms = n.saturating_mul(unit_ms);
    if ms > MAX_MS {
        return Err(format!(
            "{s} is longer than the {}h a keep-alive may last; ask again when it runs out",
            MAX_MS / 3_600_000
        ));
    }
    Ok(ms)
}

/// Set the slot's keep-alive to `ms` from now, or end it for 0, under the slot's lock. The
/// slot is the one this process runs in, by `CLAUDE_SESSIONS_SLOT`. `Ok` is the line to say.
pub fn run(ms: Millis) -> io::Result<String> {
    let Some(slot) = bind::slot_from_env() else {
        return Err(io::Error::other(
            "not inside a claude-sessions slot (no CLAUDE_SESSIONS_SLOT), so nothing to keep alive",
        ));
    };
    let _lock =
        lockfile::SlotLock::acquire(&registry::lock_path(&slot), lockfile::INTERACTIVE_WAIT)?;
    let Some(mut rec) = registry::load(&slot)? else {
        return Err(io::Error::other(format!("{slot} has no record")));
    };
    let now = clock::now();
    rec.keep_until_ms = (ms > 0).then(|| now + ms);
    rec.updated_ms = now;
    registry::store(&rec)?;
    Ok(match ms {
        0 => format!("{slot}: keep-alive ended; offloaded once quiet for 10 minutes"),
        _ => format!(
            "{slot}: kept alive for {}, then offloaded once quiet for 10 minutes",
            spelled(ms)
        ),
    })
}

/// A duration as [`parse`] reads it, in the largest unit that says it exactly.
fn spelled(ms: Millis) -> String {
    match ms {
        _ if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        _ if ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        _ => format!("{}s", ms / 1_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_is_seconds_minutes_or_hours() {
        assert_eq!(parse("90s"), Ok(90_000));
        assert_eq!(parse("25m"), Ok(25 * 60_000));
        assert_eq!(parse("2h"), Ok(2 * 3_600_000));
        assert_eq!(parse("45"), Ok(45_000), "a bare number is seconds");
        assert_eq!(parse("0"), Ok(0), "ends a keep-alive");
        for said in ["90s", "25m", "2h"] {
            assert_eq!(spelled(parse(said).unwrap()), said);
        }
    }

    #[test]
    fn a_duration_past_the_cap_or_unreadable_is_refused() {
        assert_eq!(parse("12h"), Ok(MAX_MS), "calibration: the cap itself");
        assert!(parse("13h").is_err());
        assert!(parse("99999999999999999999h").is_err());
        for bad in ["", "m", "5d", "1.5h", "-3m", "ten"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
