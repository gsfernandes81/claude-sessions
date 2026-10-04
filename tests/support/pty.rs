//! A pseudo-terminal to run things on and watch every byte they write.
//!
//! What the menu's tests need it for is the claim that **an idle menu emits zero bytes**: the
//! link is metered, so the claim is tested by running the menu on a pty and counting what
//! comes out of the master while nothing happens. That makes this harness the instrument, and
//! an instrument that reads zero because it is deaf would pass that test for the wrong reason
//! — `tests/pty_harness.rs` calibrates it both ways before anything trusts it.
//!
//! Included by path (`#[path = "support/pty.rs"] mod pty;`) from integration tests, and from
//! `src/term.rs`'s own tests, so it is std plus a few C functions declared here — this crate
//! has no dependencies; see Cargo.toml.
//!
//! The pair is opened from `/dev/ptmx` with std's `File` rather than with `openpty(3)`,
//! because std opens with `O_CLOEXEC` and musl's `openpty` does not: tests run in parallel
//! threads, and a child spawned by one test between `openpty` and a later `FD_CLOEXEC` would
//! inherit another test's terminal and hold it open.
#![allow(dead_code)]

use std::ffi::{c_char, c_int};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

// asm-generic ioctl numbers, which x86_64 shares with aarch64.
const TIOCSWINSZ: c_int = 0x5414;
const TIOCSCTTY: c_int = 0x540e;
// O_NOCTTY is 0o400 in the generic and the x86 fcntl.h alike: opening a terminal must not
// make it the test process's controlling terminal.
const O_NOCTTY: i32 = 0o400;

/// `struct winsize`: four `unsigned short`s everywhere.
#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

unsafe extern "C" {
    // The same declaration as src/term.rs's, which this file is compiled beside.
    fn ioctl(fd: c_int, request: c_int, ...) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname_r(fd: c_int, buf: *mut c_char, len: usize) -> c_int;
    fn setsid() -> c_int;
    fn signal(sig: c_int, handler: usize) -> usize;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

// The same on x86_64 and aarch64: SIGHUP and SIG_IGN are 1, F_SETFL is 4 and O_NONBLOCK is
// 0o4000 in the generic and the x86 headers alike.
const SIGHUP: c_int = 1;
const SIG_IGN: usize = 1;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;

/// A pty pair sized `cols`×`rows`: the master, and the slave opened once.
fn pair(cols: u16, rows: u16) -> std::io::Result<(File, OwnedFd)> {
    let master = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open("/dev/ptmx")?;
    // SAFETY: a valid master fd.
    if unsafe { unlockpt(master.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut name = [0u8; 64];
    // SAFETY: the buffer and its length; ptsname_r returns an errno rather than setting it.
    let rc = unsafe { ptsname_r(master.as_raw_fd(), name.as_mut_ptr().cast(), name.len()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let path = std::str::from_utf8(&name[..end]).map_err(std::io::Error::other)?;
    let slave: OwnedFd = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(path)?
        .into();
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: a valid fd and a pointer to a live winsize.
    unsafe { ioctl(master.as_raw_fd(), TIOCSWINSZ, &ws as *const Winsize) };
    Ok((master, slave))
}

/// `cmd` on a fresh pty whose master is the only handle on it — no reader thread, no held
/// slave — so dropping the returned master hangs the terminal up, as a dropped ssh link
/// does. The child ignores SIGHUP, so the hangup reaches it as its terminal failing, which is
/// how issue #6's menu met it. The master is non-blocking: read it in a loop.
pub fn spawn_bare(mut cmd: Command, cols: u16, rows: u16) -> std::io::Result<(File, Child)> {
    let (master, slave) = pair(cols, rows)?;
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: setsid, ioctl and signal are async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(|| {
            if setsid() < 0 || ioctl(0, TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            signal(SIGHUP, SIG_IGN);
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(slave);
    // SAFETY: a valid fd.
    unsafe { fcntl(master.as_raw_fd(), F_SETFL, O_NONBLOCK) };
    Ok((master, child))
}

pub struct Pty {
    /// Writes (keys) and the resize ioctl. Reads happen on a clone, in a thread.
    master: File,
    /// Held for the pty's whole life, so the master never reads EIO between a child exiting
    /// and the test looking, its termios can be read after the child is gone, and the
    /// terminal's settings are not reset by the last close.
    slave: OwnedFd,
    rx: Receiver<Vec<u8>>,
    child: Option<Child>,
}

impl Pty {
    /// A pty pair sized `cols`×`rows`, with nothing running on it.
    pub fn open(cols: u16, rows: u16) -> std::io::Result<Pty> {
        let (master, slave) = pair(cols, rows)?;

        // Everything the child writes, gathered by a thread so a read never blocks a test.
        // The thread ends on EIO, once the slave's last holder — this struct — closes it.
        let (tx, rx) = channel();
        let mut reader = master.try_clone()?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        Ok(Pty {
            master,
            slave,
            rx,
            child: None,
        })
    }

    /// `cmd` on a fresh pty, with the slave as its stdin, stdout, stderr and controlling
    /// terminal — so it gets SIGWINCH on a resize, and `stty` and `/dev/tty` work in it.
    pub fn spawn(mut cmd: Command, cols: u16, rows: u16) -> std::io::Result<Pty> {
        let mut pty = Pty::open(cols, rows)?;
        cmd.stdin(Stdio::from(pty.slave.try_clone()?))
            .stdout(Stdio::from(pty.slave.try_clone()?))
            .stderr(Stdio::from(pty.slave.try_clone()?));
        // SAFETY: setsid and ioctl are async-signal-safe syscalls, all a pre_exec may make.
        unsafe {
            cmd.pre_exec(|| {
                // A new session, so the pty can become its controlling terminal, and then
                // that terminal: stdin is the slave by now.
                if setsid() < 0 || ioctl(0, TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        pty.child = Some(cmd.spawn()?);
        Ok(pty)
    }

    pub fn slave_fd(&self) -> RawFd {
        self.slave.as_raw_fd()
    }

    /// Everything written over the next `window`, chunk by chunk, each with when it arrived —
    /// for measuring how evenly something animates.
    pub fn read_timed(&mut self, window: Duration) -> Vec<(Instant, Vec<u8>)> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return out;
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => out.push((Instant::now(), chunk)),
                Err(RecvTimeoutError::Timeout) => return out,
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(left);
                    return out;
                }
            }
        }
    }

    /// Everything written to the terminal over the next `window` — the whole window, even
    /// once bytes have arrived, because the question is usually whether *more* do.
    pub fn read_for(&mut self, window: Duration) -> Vec<u8> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return out;
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => out.extend(chunk),
                Err(RecvTimeoutError::Timeout) => return out,
                // The reader is gone, so nothing more can arrive; wait out the window anyway
                // so a caller timing it is not surprised.
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(left);
                    return out;
                }
            }
        }
    }

    /// Read until `needle` has been seen or `within` has passed, and return everything read
    /// — which may run past the needle, to the end of the chunk it came in.
    pub fn read_until(&mut self, needle: &[u8], within: Duration) -> Vec<u8> {
        let deadline = Instant::now() + within;
        let mut out = Vec::new();
        while !out.windows(needle.len()).any(|w| w == needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => out.extend(chunk),
                Err(_) => break,
            }
        }
        out
    }

    /// Type `bytes`, as a terminal emulator would send them.
    pub fn write(&mut self, bytes: &[u8]) {
        self.master
            .write_all(bytes)
            .expect("write to the pty master");
    }

    /// Resize the window. The kernel sends SIGWINCH to the terminal's foreground process
    /// group — the spawned child, if there is one.
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a struct winsize.
        let rc = unsafe { ioctl(self.master.as_raw_fd(), TIOCSWINSZ, &ws as *const Winsize) };
        assert_eq!(rc, 0, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
    }

    /// The spawned child, for `try_wait` and `id`.
    pub fn child(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("nothing was spawned on this pty")
    }

    /// Wait up to `within` for the child to exit.
    pub fn wait(&mut self, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Ok(Some(status)) = self.child().try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
