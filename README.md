# claude-sessions

**Several Claude Code sessions in several dev containers, cheap when idle, and findable again
after a disconnect.** One `ssh` into the hub lands on a list of every container's open
sessions, says which of them is waiting for you, and opens the one you pick — reattached if it
is still running, resumed from disk if it was stopped to save memory. `Enter` on a container
shows that one alone, its closed conversations too.

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
registry implementation and one set of tests. The menu and the offload timer run in a **hub**
that reaches every dev container over ssh; everything that reads or changes a container's
sessions runs in that container, from the same binary, which the hub installs into a volume
they all mount ([`docs/design.md`](docs/design.md) § *The hub*):

| | |
|---|---|
| `claude-sessions` | the menu over every container — the default, and what the hub's ssh login lands on |
| `claude-sessions offload-all` | one offload pass in every container, over ssh; the hub's timer runs it every 3 minutes |
| `claude-sessions install DIR` | this binary, its hooks drop-in and its skill, into the volume every container mounts |
| `claude-sessions agent` | a container's half of the hub, run over ssh |
| `claude-sessions hook` | fed by Claude Code's hooks; the only writer of session state |
| `claude-sessions offload` | in a container: stops a session nobody is using, keeping its conversation — closing it if it never had one |
| `claude-sessions keepalive` | asked by claude before a quiet wait, so the offloader leaves its session alone for that long |
| `claude-sessions skill` | the skill that tells claude when to ask for that |
| `claude-sessions list` | every slot, one line each |
| `claude-sessions hooks-config` | the Claude Code settings that install the hooks and the status line |
| `claude-sessions reconcile` | makes the registry agree with reality after a restart |
| `claude-sessions close` | ends a session without opening it |
| `claude-sessions doctor` | says what it can and cannot see, per slot |
| `claude-sessions statusline` | RAM and load across every container, and the host, for Claude Code's status line |

## Status

Released as GitHub releases, which infra's hub image pins; 0.5.0 runs on the fleet from
2026-10-27. What is still open is [`docs/handoff/2026-10-02-menu.md`](docs/handoff/2026-10-02-menu.md).

- [`docs/design.md`](docs/design.md) — what it does and why, in enough detail to build from
- [`docs/mockups.md`](docs/mockups.md) — the screens, approved and binding
- [`CLAUDE.md`](CLAUDE.md) — how work is done in this repo

## How it gets to a box

A GitHub release carries static musl binaries, cross-compiled. The consumer is the hub's
Dockerfile in the (public) `infra` config repo, which downloads a **pinned** tag and checks a
per-architecture SHA-256 — so this repo must stay public (that build runs on a public runner
with no token) and a published release must never be re-cut under the same tag.

## Licence

AGPL-3.0-or-later. See [`LICENSE`](LICENSE).
