# claude-sessions

**Several Claude Code sessions per box, cheap when idle, and findable again after a
disconnect.** One `ssh` into a dev container lands on a list of the open sessions, says which
of them is waiting for you, and opens the one you pick — reattached if it is still running,
resumed from disk if it was stopped to save memory.

It was built for three Raspberry Pis reached over a metered satellite link, and it is honest
about that: a Pi 5 with 4 GB holding four dev containers, driven from a phone in portrait at
40 columns. It is not a general-purpose multiplexer and makes no promises about layouts
nobody here runs. If it suits yours, good — but that is luck, not a feature.

## The two problems it exists for

**An idle Claude Code session can cost a gigabyte.** Pressing the agent-view key once starts
a daemon, a warm spare and its pty host, and they outlive the session that started them.
Measured 2026-10-01 against Claude Code 2.1.286: ~250 MB idle, ~1,260 MB after one keypress,
~690 MB still resident after the session exited. On a 4 GB box with four containers that is
the whole budget, spent by a key nobody meant to press.

**And there was no way back to a session.** `ssh <container>` was one fixed `abduco` session,
so after the old idle-offloader stopped it, the next login started a *fresh* Claude and the
command to resume the real one sat in a log. No list, no "which one wants me", no way to end
one deliberately.

## What it is

One Rust binary with subcommands, so the hook path, the offloader and the menu share one
registry implementation and one set of tests:

| | |
|---|---|
| `claude-sessions` | the menu — the default, and what an ssh login lands on |
| `claude-sessions hook` | fed by Claude Code's hooks; the only writer of session state |
| `claude-sessions offload` | stops a session nobody is using, keeping its conversation — closing it if it never had one |
| `claude-sessions reconcile` | makes the registry agree with reality after a restart |
| `claude-sessions close` | ends a session without opening it |
| `claude-sessions doctor` | says what it can and cannot see, per slot |
| `claude-sessions statusline` | RAM, load and host, for Claude Code's status line |

## Status

**Nothing is deployed and nothing is released.** The design is settled and the screens are
approved; the code is not written. Start at
[`docs/handoff/2026-10-01-bootstrap.md`](docs/handoff/2026-10-01-bootstrap.md).

- [`docs/design.md`](docs/design.md) — what it does and why, in enough detail to build from
- [`docs/mockups.md`](docs/mockups.md) — the eight screens, approved and binding
- [`CLAUDE.md`](CLAUDE.md) — how work is done in this repo

## How it gets to a box

A GitHub release carries static musl binaries, cross-compiled. The consumer is a
`Dockerfile.base` in a private config repo, which downloads a **pinned** tag and checks a
per-architecture SHA-256 — so this repo must stay public (that build runs on a public runner
with no token) and a published release must never be re-cut under the same tag.

## Licence

AGPL-3.0-or-later. See [`LICENSE`](LICENSE).
