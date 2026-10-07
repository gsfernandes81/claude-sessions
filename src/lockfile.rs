//! A per-slot advisory lock, with a timeout that is not optional.
//!
//! Two reasons the timeout is the whole point of this module.
//!
//! **The menu is the ssh door.** If a lock can be held forever, a stuck process holds the
//! door shut and the only way in is the break-glass shell. Every wait here is bounded and a
//! timeout is an ordinary, reportable outcome rather than a panic.
//!
//! **`SessionEnd` hooks share a 1.5 second budget across all of them.** The write that marks
//! a slot closed happens on that path, so its wait has to be well inside that — see
//! `SESSION_END_WAIT`. Being cut off halfway is worse than giving up: `reconcile` can repair
//! a slot that was never marked closed, but it cannot repair a half-written file.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

// Declared here rather than taken from the `libc` crate, because this crate has no
// dependencies — see Cargo.toml. These two values are part of Linux's ABI and cannot change:
// `flock(2)` has carried them since 4.2BSD.
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// What a `SessionEnd` hook may spend waiting. `SessionEnd` hooks share 1.5 s across all of
/// them, so a quarter of that is the most one write may risk.
pub const SESSION_END_WAIT: Duration = Duration::from_millis(400);

/// What every other hook may spend: long enough to outlast a write stalled on a busy disk.
/// Nothing waits on an async hook, and `SessionStart`'s timeout is set above this.
pub const HOOK_WAIT: Duration = Duration::from_secs(15);

/// What an interactive command may spend. Long enough to outlast a competing write, short
/// enough that a person does not think the menu has hung.
pub const INTERACTIVE_WAIT: Duration = Duration::from_millis(2000);

#[derive(Debug)]
pub struct SlotLock {
    file: File,
}

impl SlotLock {
    /// Take the lock for `path`, waiting at most `wait`.
    ///
    /// Polls rather than blocking in `flock(2)`: a blocking call cannot be given a deadline
    /// without arming a signal to interrupt it, which is a great deal more machinery than
    /// asking again in five milliseconds. Contention here is a handful of processes, not hundreds.
    pub fn acquire(path: &Path, wait: Duration) -> io::Result<SlotLock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: a valid fd from the File above, which outlives the call.
            let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
            if rc == 0 {
                return Ok(SlotLock { file });
            }
            let err = io::Error::last_os_error();
            let busy = matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            );
            if !busy {
                return Err(err);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("lock {} busy for {:?}", path.display(), wait),
                ));
            }
            sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for SlotLock {
    fn drop(&mut self) {
        // Closing the fd releases the lock, so this is belt and braces — but an explicit
        // unlock makes the lifetime of the lock the lifetime of this value, which is what
        // every caller assumes when it holds one across a decision and an action.
        unsafe { flock(self.file.as_raw_fd(), LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_waiter_times_out_rather_than_hanging() {
        let dir = std::env::temp_dir().join(format!("cs-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slot.lock");
        let held = SlotLock::acquire(&path, Duration::from_millis(50)).expect("first take");
        let start = Instant::now();
        let err = SlotLock::acquire(&path, Duration::from_millis(60))
            .expect_err("a second exclusive take must not succeed");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // The guarantee that matters for the door: it gave up, and roughly when it said it
        // would rather than at some unbounded later point.
        assert!(
            start.elapsed() < Duration::from_millis(600),
            "waited {:?}",
            start.elapsed()
        );
        drop(held);
        SlotLock::acquire(&path, Duration::from_millis(50)).expect("free once dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
