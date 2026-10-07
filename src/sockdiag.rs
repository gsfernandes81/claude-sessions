//! Bytes through every TCP socket in this network namespace, by inode, asked of the kernel's
//! socket-diagnostics netlink (`NETLINK_SOCK_DIAG`, the interface `ss -ti` reads).
//!
//! **Why.** `/proc/<pid>/io` counts `read`/`write` and not `send`/`recv`, so a claude's own
//! network — the API, a websocket, an HTTP MCP server — is invisible to the activity
//! measurement's byte counts (design.md § *Activity, measured*). The kernel keeps a per-socket
//! count whichever call moved the bytes: `tcpi_bytes_received` and `tcpi_bytes_acked` in
//! `struct tcp_info`. A dump needs no privilege — measured on 2026-10-07 as `nobody` with every
//! capability dropped, a megabyte moved by `send`/`recv` over loopback read back exactly — and
//! sees the sockets of the caller's network namespace, which is the container's, where every
//! slot's claude lives.
//!
//! **Only TCP.** UDP and Unix sockets carry no byte counts in this interface.
//!
//! The calls are declared here rather than taken from `libc`, as everywhere in this crate (see
//! Cargo.toml). `socket`, `send` and `recv` are musl's own functions; the constants are the
//! generic ones, the same on x86_64 and aarch64, and the message layouts are the kernel's
//! stable netlink ABI, in native byte order on both.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_void};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const AF_NETLINK: c_int = 16;
const SOCK_DGRAM: c_int = 2;
const SOCK_CLOEXEC: c_int = 0o2_000_000;
const NETLINK_SOCK_DIAG: c_int = 4;
const MSG_DONTWAIT: c_int = 0x40;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const IPPROTO_TCP: u8 = 6;

const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const INET_DIAG_INFO: u16 = 2;

/// `struct nlmsghdr`.
const NLMSG_HDR: usize = 16;
/// `struct inet_diag_msg`: family, state, timer, retrans, a 48-byte `inet_diag_sockid`, then
/// expires, rqueue, wqueue, uid and inode, four bytes each.
const DIAG_MSG: usize = 72;
const DIAG_INODE: usize = 68;
/// `tcpi_bytes_acked` and `tcpi_bytes_received` in `struct tcp_info`, both since Linux 4.1/4.2.
const TCPI_BYTES_ACKED: usize = 120;
const TCPI_BYTES_RECEIVED: usize = 128;

unsafe extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
    fn recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
}

/// Every TCP socket's bytes, both ways, by inode: IPv4 and IPv6, in every state. An error means
/// none are known — a kernel without the diag module, or a refusal — never that there are none.
pub fn tcp() -> io::Result<BTreeMap<u64, u64>> {
    // SAFETY: socket(2) returns a new descriptor or -1 with errno set.
    let fd = unsafe { socket(AF_NETLINK, SOCK_DGRAM | SOCK_CLOEXEC, NETLINK_SOCK_DIAG) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nobody else owns, closed when this drops.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut out = BTreeMap::new();
    for (seq, family) in [(1u32, AF_INET), (2, AF_INET6)] {
        dump(&fd, seq, family, &mut out)?;
    }
    Ok(out)
}

fn dump(fd: &OwnedFd, seq: u32, family: u8, out: &mut BTreeMap<u64, u64>) -> io::Result<()> {
    // `struct inet_diag_req_v2`: family, protocol, the extensions wanted, a pad byte, the state
    // mask (all of them), and an `inet_diag_sockid` left zero, which a dump ignores.
    let mut req = Vec::with_capacity(NLMSG_HDR + 56);
    req.extend_from_slice(&((NLMSG_HDR + 56) as u32).to_ne_bytes());
    req.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    req.extend_from_slice(&seq.to_ne_bytes());
    req.extend_from_slice(&0u32.to_ne_bytes());
    req.extend_from_slice(&[family, IPPROTO_TCP, 1 << (INET_DIAG_INFO - 1), 0]);
    req.extend_from_slice(&u32::MAX.to_ne_bytes());
    req.extend_from_slice(&[0u8; 48]);
    // SAFETY: the buffer is valid for its length for the duration of the call.
    let sent = unsafe { send(fd.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        // Never blocks: the kernel queues a dump's first part while handling the request and
        // each next one while handing over the last, so the queue is empty only once the dump
        // has ended — or broken, which is an error rather than a wait that could hold a pass.
        // SAFETY: the buffer is valid and writable for its length.
        let n = unsafe {
            recv(
                fd.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                MSG_DONTWAIT,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the dump ended without its last message",
            ));
        }
        let mut off = 0;
        while off + NLMSG_HDR <= n {
            let len = u32_at(&buf, off) as usize;
            let kind = u16::from_ne_bytes([buf[off + 4], buf[off + 5]]);
            if len < NLMSG_HDR || off + len > n {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a malformed netlink message",
                ));
            }
            match kind {
                NLMSG_DONE => return Ok(()),
                NLMSG_ERROR => {
                    let errno = i32::from_ne_bytes(
                        buf[off + NLMSG_HDR..off + NLMSG_HDR + 4]
                            .try_into()
                            .unwrap_or([0; 4]),
                    );
                    return Err(io::Error::from_raw_os_error(-errno));
                }
                _ => {
                    if let Some((inode, bytes)) = socket_bytes(&buf[off + NLMSG_HDR..off + len]) {
                        out.insert(inode, bytes);
                    }
                }
            }
            off += align(len);
        }
    }
}

/// One `inet_diag_msg` and its attributes: the socket's inode and its bytes both ways, or
/// `None` without a `tcp_info` long enough to hold them.
fn socket_bytes(msg: &[u8]) -> Option<(u64, u64)> {
    if msg.len() < DIAG_MSG {
        return None;
    }
    let inode = u64::from(u32_at(msg, DIAG_INODE));
    let mut off = DIAG_MSG;
    while off + 4 <= msg.len() {
        let len = u16::from_ne_bytes([msg[off], msg[off + 1]]) as usize;
        let kind = u16::from_ne_bytes([msg[off + 2], msg[off + 3]]);
        if len < 4 || off + len > msg.len() {
            return None;
        }
        if kind == INET_DIAG_INFO {
            let info = &msg[off + 4..off + len];
            let at = |o: usize| -> Option<u64> {
                Some(u64::from_ne_bytes(info.get(o..o + 8)?.try_into().ok()?))
            };
            return Some((
                inode,
                at(TCPI_BYTES_ACKED)?.saturating_add(at(TCPI_BYTES_RECEIVED)?),
            ));
        }
        off += align(len);
    }
    None
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Netlink messages and attributes are each padded to four bytes.
fn align(len: usize) -> usize {
    (len + 3) & !3
}

/// The inodes of the sockets `pid` holds open, or `None` if its descriptors cannot be listed.
pub fn held(pid: u32) -> Option<Vec<u64>> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut out: Vec<u64> = dir
        .flatten()
        .filter_map(|e| {
            let target = std::fs::read_link(e.path()).ok()?;
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;

    /// Calibrated against a known count: a loopback pair moves 100,000 bytes, and the
    /// receiving socket, found by the inode this process holds it under, reads at least that
    /// — and the sender as much, acked. Std's TCP uses `send`/`recv`, the calls `/proc/<pid>/io`
    /// does not count, so this is the gap itself, closed.
    #[test]
    fn a_sockets_bytes_are_read_by_its_inode() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut tx = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut rx, _) = listener.accept().unwrap();
        let inode = |s: &std::net::TcpStream| {
            std::fs::metadata(format!("/proc/self/fd/{}", s.as_raw_fd()))
                .unwrap()
                .ino()
        };
        let before = tcp().expect("the dump");
        assert!(
            before.get(&inode(&rx)).is_some_and(|&b| b < 100_000),
            "calibration: the socket is in the dump before it has moved anything: {:?}",
            before.get(&inode(&rx))
        );
        tx.write_all(&[7u8; 100_000]).unwrap();
        let mut got = vec![0u8; 100_000];
        rx.read_exact(&mut got).unwrap();
        let after = tcp().expect("the dump");
        assert!(
            after[&inode(&rx)] >= 100_000,
            "{:?}",
            after.get(&inode(&rx))
        );
        assert!(
            after[&inode(&tx)] >= 100_000,
            "{:?}",
            after.get(&inode(&tx))
        );
        let mine = held(std::process::id()).unwrap();
        assert!(
            mine.contains(&inode(&rx)) && mine.contains(&inode(&tx)),
            "{mine:?}"
        );
    }

    #[test]
    fn a_short_or_infoless_message_reads_as_nothing() {
        assert_eq!(socket_bytes(&[0u8; 10]), None);
        let mut msg = vec![0u8; DIAG_MSG];
        msg[DIAG_INODE..DIAG_INODE + 4].copy_from_slice(&42u32.to_ne_bytes());
        assert_eq!(socket_bytes(&msg), None, "no attribute");
        // A tcp_info too short for the byte counts: an old kernel's.
        let mut short = msg.clone();
        short.extend_from_slice(&(4u16 + 100).to_ne_bytes());
        short.extend_from_slice(&INET_DIAG_INFO.to_ne_bytes());
        short.extend_from_slice(&[0u8; 100]);
        assert_eq!(socket_bytes(&short), None);
        // Calibration: one long enough reads both counts, summed.
        let mut info = vec![0u8; 136];
        info[TCPI_BYTES_ACKED..TCPI_BYTES_ACKED + 8].copy_from_slice(&5u64.to_ne_bytes());
        info[TCPI_BYTES_RECEIVED..TCPI_BYTES_RECEIVED + 8].copy_from_slice(&7u64.to_ne_bytes());
        msg.extend_from_slice(&(4u16 + 136).to_ne_bytes());
        msg.extend_from_slice(&INET_DIAG_INFO.to_ne_bytes());
        msg.extend_from_slice(&info);
        assert_eq!(socket_bytes(&msg), Some((42, 12)));
    }
}
