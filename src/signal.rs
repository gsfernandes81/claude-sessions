//! Signalling a process we recorded, and never one that merely inherited its pid.
//!
//! The offloader's whole risk is here. A stored pid will eventually name somebody else's
//! process, and `kill(pid)` after checking the start time still leaves a window in which the
//! pid is reused between the check and the signal. A **pidfd** closes it: the descriptor is
//! opened first and refers to whatever held the pid at that instant, the start time is
//! checked second, and a match then proves the descriptor is the recorded process — the
//! recorded one already held that pid before the descriptor was opened, and a pid is never
//! held by two processes at once. From then on a signal sent through it reaches that
//! process or nobody.
//!
//! The syscalls are declared here rather than taken from the `libc` crate, because this crate
//! has no dependencies — see Cargo.toml. `pidfd_open` (434) and `pidfd_send_signal` (424) are
//! in the generic syscall table, so the numbers are the same on x86_64 and aarch64, the two
//! targets this ships for. Both arrived in Linux 5.3; on an older kernel the `ENOSYS` falls
//! back to `kill(2)` after the same start-time check, which is the best that kernel offers.

use crate::procinfo;
use std::ffi::c_long;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;

const SYS_PIDFD_SEND_SIGNAL: c_long = 424;
const SYS_PIDFD_OPEN: c_long = 434;
const ENOSYS: i32 = 38;
const ESRCH: i32 = 3;

unsafe extern "C" {
    fn syscall(num: c_long, ...) -> c_long;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// What a signal attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// Delivered to the recorded process.
    Delivered,
    /// The recorded process is not there — it has exited, or its pid now names something
    /// else. Nothing was signalled.
    Gone,
}

/// Send `sig` to `pid`, but only if it is still the process that started at `start`.
pub fn send(pid: u32, start: u64, sig: i32) -> io::Result<Sent> {
    let fd = match pidfd_open(pid) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(ENOSYS) => return send_by_kill(pid, start, sig),
        Err(e) => return gone_or(e),
    };
    if !procinfo::is_alive(pid, start) {
        return Ok(Sent::Gone);
    }
    use std::os::fd::AsRawFd;
    // SAFETY: a valid pidfd owned above; the null siginfo means "as kill(2) would".
    let rc = unsafe {
        syscall(
            SYS_PIDFD_SEND_SIGNAL,
            fd.as_raw_fd() as c_long,
            sig as c_long,
            std::ptr::null::<u8>(),
            0 as c_long,
        )
    };
    if rc == 0 {
        Ok(Sent::Delivered)
    } else {
        gone_or(io::Error::last_os_error())
    }
}

/// A kernel before 5.3, which has no pidfds: the same start-time check, then `kill(2)`, which
/// leaves the window a pidfd closes but is the best that kernel offers.
fn send_by_kill(pid: u32, start: u64, sig: i32) -> io::Result<Sent> {
    if !procinfo::is_alive(pid, start) {
        return Ok(Sent::Gone);
    }
    // SAFETY: plain kill(2); pid fits, it came from /proc.
    if unsafe { kill(pid as i32, sig) } == 0 {
        Ok(Sent::Delivered)
    } else {
        gone_or(io::Error::last_os_error())
    }
}

/// `ESRCH` is the process having gone between being looked at and being signalled, which is
/// what `Gone` says; anything else is an error.
fn gone_or(e: io::Error) -> io::Result<Sent> {
    if e.raw_os_error() == Some(ESRCH) {
        Ok(Sent::Gone)
    } else {
        Err(e)
    }
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open(pid, flags=0) returns a new fd or -1 with errno set.
    let fd = unsafe { syscall(SYS_PIDFD_OPEN, pid as c_long, 0 as c_long) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nobody else owns, closed when the OwnedFd drops.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_start_time_is_never_signalled() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start = procinfo::start_time(pid).expect("readable");
        // Calibration first: the right start time does reach it, using signal 0, which checks
        // deliverability and sends nothing.
        assert_eq!(send(pid, start, 0).unwrap(), Sent::Delivered);
        // A pid whose start time does not match is somebody else's process.
        assert_eq!(send(pid, start + 1, SIGKILL).unwrap(), Sent::Gone);
        assert!(
            procinfo::is_alive(pid, start),
            "it must have survived the mismatched KILL"
        );
        assert_eq!(send(pid, start, SIGKILL).unwrap(), Sent::Delivered);
        child.wait().expect("reap");
        assert_eq!(send(pid, start, SIGTERM).unwrap(), Sent::Gone);
    }

    /// The fallback a pre-5.3 kernel takes, driven here directly since this one never takes it.
    #[test]
    fn the_kill_fallback_checks_the_start_time_the_same_way() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start = procinfo::start_time(pid).expect("readable");
        assert_eq!(send_by_kill(pid, start, 0).unwrap(), Sent::Delivered);
        assert_eq!(send_by_kill(pid, start + 1, SIGKILL).unwrap(), Sent::Gone);
        assert!(
            procinfo::is_alive(pid, start),
            "survived the mismatched KILL"
        );
        assert_eq!(send_by_kill(pid, start, SIGKILL).unwrap(), Sent::Delivered);
        child.wait().expect("reap");
        assert_eq!(send_by_kill(pid, start, SIGTERM).unwrap(), Sent::Gone);
    }

    #[test]
    fn only_esrch_means_gone() {
        let gone = gone_or(io::Error::from_raw_os_error(ESRCH)).unwrap();
        assert_eq!(gone, Sent::Gone);
        let e = gone_or(io::Error::from_raw_os_error(1)).unwrap_err();
        assert_eq!(
            e.raw_os_error(),
            Some(1),
            "EPERM is an error, not a gone process"
        );
        // And through `send`: pid 0 is no process, which pidfd_open refuses with EINVAL — an
        // error, not a process that has gone, nor a reason to fall back to kill(2), which
        // would signal the whole process group.
        let e = send(0, 0, 0).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(22), "{e}");
    }
}
