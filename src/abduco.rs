//! abduco, read from the filesystem rather than by running it.
//!
//! `abduco`'s own listing wants a terminal and prints a header, so a readout built on it
//! cannot tell "no sessions" from "the listing did not work". The socket directory answers
//! the same question with a `stat`.
//!
//! **Attached is the owner-execute bit** on `~/.abduco/<name>@<hostname>`: `srwx------` while
//! a client is attached, `srw-------` when detached. Verified both ways round on 2026-10-01.
//!
//! **And that bit lies about a dead session.** `kill -9` on the server leaves the socket
//! behind with the attached bit still set — abduco's own listing drops it immediately, the
//! file does not. So the mode only ever answers *attached?* for a session already known to be
//! alive by its pid; on its own it reports a corpse as busy and the menu would refuse to
//! offer it. That asymmetry is why this module returns what it sees and judges nothing.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Socket {
    /// The session name, i.e. everything before the `@`.
    pub name: String,
    pub path: PathBuf,
    /// The owner-execute bit: a client is attached. **Only meaningful for a live session** —
    /// see the module note.
    pub attached_bit: bool,
}

fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("ABDUCO_SOCKET_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".abduco")
}

/// Every socket in the abduco directory.
///
/// The name carries the hostname — `claude-1@infra-dev` — which these containers derive from
/// their compose alias, so renaming a container orphans every session in it. We split on the
/// last `@` and keep the left half.
pub fn sockets() -> Vec<Socket> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let raw = e.file_name().to_string_lossy().to_string();
        let name = match raw.rfind('@') {
            Some(i) => raw[..i].to_string(),
            None => raw.clone(),
        };
        out.push(Socket {
            name,
            path: e.path(),
            attached_bit: meta.permissions().mode() & 0o100 != 0,
        });
    }
    out
}

pub fn socket_for(name: &str) -> Option<Socket> {
    sockets().into_iter().find(|s| s.name == name)
}

/// Is this one of ours? The menu names its slots `claude-<n>`; anything else in the directory
/// is somebody's own session and gets marked rather than adopted.
pub fn is_slot_name(name: &str) -> bool {
    name.strip_prefix("claude-")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}
