//! Time, injectable — because the state machine is tested and `SystemTime::now()` inside it
//! would make every test a race. Milliseconds since the epoch everywhere; the registry is
//! JSON and a u64 of milliseconds reads the same in every language that might ever look at
//! it.

use std::time::{SystemTime, UNIX_EPOCH};

pub type Millis = u64;

pub fn now() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        // A clock before 1970 is not a case worth a Result all the way up the call stack.
        .unwrap_or(0)
}

// `clock_gettime(2)` is declared here rather than taken from `libc` (see Cargo.toml). Both
// targets are 64-bit musl, where `timespec` is two `i64`s, and the clock ids are Linux ABI.
#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

const CLOCK_REALTIME: i32 = 0;
/// The clock `/proc` counts process start times on, suspend included.
const CLOCK_BOOTTIME: i32 = 7;

/// `USER_HZ`, the unit of `/proc` start times: kernel ABI, 100 on every Linux shipped for.
const NS_PER_TICK: i128 = 10_000_000;

unsafe extern "C" {
    fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
}

fn read_ns(clock: i32) -> Option<i128> {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: a valid pointer to a `timespec` this frame owns.
    let ok = unsafe { clock_gettime(clock, &mut ts) } == 0;
    ok.then(|| i128::from(ts.sec) * 1_000_000_000 + i128::from(ts.nsec))
}

fn since_boot_ns() -> Option<i128> {
    read_ns(CLOCK_BOOTTIME)
}

/// Nanoseconds from `ticks` after boot until now.
pub fn since_tick_ns(ticks: u64) -> Option<i128> {
    Some(since_boot_ns()? - i128::from(ticks) * NS_PER_TICK)
}

/// The wall-clock time of a moment `ticks` after boot, to within a millisecond.
fn at_tick(ticks: u64) -> Option<Millis> {
    let boot = read_ns(CLOCK_REALTIME)? - since_boot_ns()?;
    Millis::try_from((boot + i128::from(ticks) * NS_PER_TICK) / 1_000_000).ok()
}

/// A moment twice over: the clock tick since boot the kernel stamps a process's start with,
/// exact and so what events are ordered by, and its wall-clock time, for everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    pub tick: u64,
    pub at: Millis,
}

impl Moment {
    pub fn now() -> Moment {
        let tick = since_boot_ns().map_or(0, |ns| (ns / NS_PER_TICK) as u64);
        Moment { tick, at: now() }
    }

    pub fn of_tick(tick: u64) -> Option<Moment> {
        Some(Moment {
            tick,
            at: at_tick(tick)?,
        })
    }

    /// A moment at `at` milliseconds, ticking with it, for tests.
    #[cfg(test)]
    pub fn ms(at: Millis) -> Moment {
        Moment { tick: at / 10, at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_reads_as_one_wall_clock_moment() {
        let now = Moment::now();
        let then = Moment::of_tick(now.tick).unwrap();
        assert!(then.at.abs_diff(now.at) <= 11, "{then:?} {now:?}");
        let later = Moment::of_tick(now.tick + 100).unwrap();
        assert!(
            later.at.abs_diff(then.at + 1_000) <= 1,
            "calibration: 100 ticks is 1 s"
        );
    }
}
