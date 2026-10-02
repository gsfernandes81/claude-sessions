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

/// The session name an abduco command line names, read as abduco itself reads it.
///
/// abduco takes its options getopt-style: they cluster (`-fA` is `-f -A`), only `-e` takes a
/// value (the detach key, attached or as the next argument), and the session name is the first
/// argument that is not an option — `abduco [-a|-A|-c|-n] [-e key] [-flpqr] name command…`.
/// Reading only a separate `-c`/`-A`/`-n` followed by the name, as this once did, named no
/// session for `abduco -fA work fish`, and `reconcile` then swept a live session's socket
/// (issue #3). A command line that is not a session's — `abduco` alone lists sessions — names
/// none.
pub fn session_name(argv: &[String]) -> Option<String> {
    let mut args = argv.iter().skip(1);
    let mut mode = false;
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args.next().filter(|_| mode).cloned();
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) else {
            return mode.then(|| arg.clone());
        };
        for (i, flag) in cluster.char_indices() {
            match flag {
                'a' | 'A' | 'c' | 'n' => mode = true,
                // The detach key: the rest of this argument, or all of the next one.
                'e' => {
                    if cluster[i + 1..].is_empty() {
                        args.next();
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    None
}

/// Is this one of ours? The menu names its slots `claude-<n>`; anything else in the directory
/// is somebody's own session and gets marked rather than adopted.
pub fn is_slot_name(name: &str) -> bool {
    name.strip_prefix("claude-")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(cmd: &str) -> Option<String> {
        session_name(
            &cmd.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn the_fleets_own_spellings_are_read() {
        // Calibration: the forms that were always read still are.
        assert_eq!(name("abduco -A claude claude").as_deref(), Some("claude"));
        assert_eq!(
            name("abduco -c claude-3 env CLAUDE_SESSIONS_SLOT=claude-3 claude").as_deref(),
            Some("claude-3")
        );
        assert_eq!(name("abduco -n bg sleep 9").as_deref(), Some("bg"));
    }

    #[test]
    fn clustered_flags_are_read_as_abduco_reads_them() {
        // Issue #3: this named nothing, and reconcile swept the live session's socket.
        assert_eq!(name("abduco -fA work fish").as_deref(), Some("work"));
        assert_eq!(name("abduco -rA work fish").as_deref(), Some("work"));
        assert_eq!(name("abduco -f -A work fish").as_deref(), Some("work"));
        assert_eq!(name("abduco -a work").as_deref(), Some("work"));
    }

    #[test]
    fn the_detach_key_is_not_mistaken_for_the_name() {
        assert_eq!(name("abduco -e ^q -A work fish").as_deref(), Some("work"));
        assert_eq!(name("abduco -e^q -A work fish").as_deref(), Some("work"));
        assert_eq!(name("abduco -Ae ^q work fish").as_deref(), Some("work"));
        assert_eq!(name("abduco -A -- -odd fish").as_deref(), Some("-odd"));
    }

    #[test]
    fn a_command_line_that_is_not_a_session_names_none() {
        assert_eq!(name("abduco"), None, "abduco alone lists sessions");
        assert_eq!(name("abduco -l"), None);
        assert_eq!(
            name("abduco listing"),
            None,
            "no -a/-A/-c/-n, so not a session"
        );
    }
}
