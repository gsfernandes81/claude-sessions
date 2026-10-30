//! The harnesses a slot can run: Claude Code, and pi (pi.dev) since 0.10.0 (the owner's brief
//! of 2026-10-30, relayed by infra; `docs/design.md` § *Harnesses*).
//!
//! A slot's harness is in its name — `claude-<n>`, `pi-<n>` — and on its record and its row,
//! so a record or row written before 0.10.0, which has no such field, reads as claude: what
//! the last release wrote must still read, and the only harness it could have run is claude.
//! Everything that differs between the two — the program, its start and resume arguments,
//! the process name its hook is bound by, where its conversations are and how they are read —
//! is asked of this type, so no second table of "if pi" grows elsewhere.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Harness {
    #[default]
    Claude,
    Pi,
}

impl Harness {
    pub const ALL: [Harness; 2] = [Harness::Claude, Harness::Pi];

    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Pi => "pi",
        }
    }

    /// Tolerant: a harness this version does not know reads as claude, the one every version
    /// before this knew. A record from a newer version naming a third harness is then drawn
    /// and never resumed with the wrong program: `resume_args` is asked of the record's own
    /// reading, and the menu's row says what it is.
    pub fn parse(s: &str) -> Harness {
        match s {
            "pi" => Harness::Pi,
            _ => Harness::Claude,
        }
    }

    /// The slot name's prefix: `claude-` or `pi-`.
    pub fn prefix(self) -> &'static str {
        match self {
            Harness::Claude => "claude-",
            Harness::Pi => "pi-",
        }
    }

    /// Which harness a slot name says it runs: `claude-3` is claude's, `pi-1` is pi's, and
    /// `work` — a session somebody typed — is nobody's.
    pub fn of_slot(name: &str) -> Option<Harness> {
        Harness::ALL.into_iter().find(|h| {
            name.strip_prefix(h.prefix())
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
    }

    /// The process name a running harness has, as `/proc/<pid>/comm` reads it: `claude` is
    /// the npm wrapper's name for Claude Code; pi sets its `process.title` to `pi` (read from
    /// the 1.4.0 bundle, 2026-10-30; `pi-rpc` in rpc mode, which no slot runs).
    pub fn comm(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Pi => "pi",
        }
    }

    /// Whether `comm` is some harness's process: the one a hook's binding looks for above it.
    pub fn of_comm(comm: &str) -> Option<Harness> {
        Harness::ALL.into_iter().find(|h| h.comm() == comm)
    }

    /// The environment variable naming the program to run for this harness, and its default:
    /// one table, so tests can stand a program in for either (`launch::Deps`).
    pub fn program_var(self) -> (&'static str, &'static str) {
        match self {
            Harness::Claude => ("CLAUDE_SESSIONS_CLAUDE", "claude"),
            Harness::Pi => ("CLAUDE_SESSIONS_PI", "pi"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_name_says_its_harness_and_anything_else_is_nobodys() {
        assert_eq!(Harness::of_slot("claude-1"), Some(Harness::Claude));
        assert_eq!(Harness::of_slot("claude-42"), Some(Harness::Claude));
        assert_eq!(Harness::of_slot("pi-1"), Some(Harness::Pi));
        for not in [
            "claude",
            "claude-",
            "claude-1a",
            "pi",
            "pi-",
            "pi-x",
            "work",
            "-1",
        ] {
            assert_eq!(Harness::of_slot(not), None, "{not:?}");
        }
    }

    #[test]
    fn every_harness_reads_back_and_an_unknown_one_reads_as_claude() {
        for h in Harness::ALL {
            assert_eq!(Harness::parse(h.as_str()), h);
            assert_eq!(Harness::of_comm(h.comm()), Some(h));
            assert_eq!(Harness::of_slot(&format!("{}7", h.prefix())), Some(h));
        }
        assert_eq!(Harness::parse("codex"), Harness::Claude);
        assert_eq!(Harness::parse(""), Harness::Claude);
        assert_eq!(Harness::of_comm("bash"), None);
        assert_eq!(Harness::default(), Harness::Claude, "absent is claude");
    }
}
