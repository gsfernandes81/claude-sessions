# claude-sessions — Project Rules

**This repo is source.** One Rust binary, no deployment config, no secrets, no host state.
The fleet's configuration lives in a separate private `infra` repo, which consumes this one
as a **pinned, checksummed GitHub release** — never as a git dependency and never built in
place.

**Nothing here has ever run on a box.** There are no users, no installed copies and no
compatibility to preserve. Say so rather than inventing caution about breaking things that do
not exist yet.

## What it is, in one paragraph

A **slot** is one `abduco` session, i.e. one running `claude` process. A slot holds a
sequence of **conversations** (Claude session ids): `/clear` starts a new one in the same
process, `--resume` re-enters an old one. A **registry** keyed by slot records what each one
is doing, written only by `claude-sessions hook` from Claude Code's own hook events. The menu
reads the registry and `abduco`, never guesses, and opens what the owner picks.
[`docs/design.md`](docs/design.md) is the specification; read it before writing code.

## The rules that have teeth

- **The approved mockups are binding.** [`docs/mockups.md`](docs/mockups.md) was approved by
  the owner on 2026-10-01. Phase 4 renders to those screens; a change to them needs the
  owner, not a judgement call. The colour assignment in that file is part of the approval.
- **`claude-sessions hook` always exits 0.** `UserPromptSubmit` and `Stop` are *blocking*
  hooks: a non-zero exit on the first blocks the prompt, and on the second makes Claude carry
  on as though it had more to do. A registry bug must never wedge a session. It logs its own
  failures to its own log and returns 0. A test pins this, naming those two events.
- **Every lock has a timeout.** The menu is what an ssh login lands on, so a stuck lock would
  hold the door shut. `SessionEnd` hooks additionally share a **1.5-second** budget across
  all of them — a lock wait on that path must be well inside it, or the write that marks a
  slot closed is cut off halfway.
- **An idle menu emits zero bytes.** Ages tick at most once a minute; nothing else redraws
  without a keypress or a registry change. The link is metered. Test it under a pty.
- **Never widen a glyph's job without measuring the font.** `↵` (U+21B5) is in **no**
  monospace face on Google Fonts — JetBrains Mono, Noto Sans Mono, Source Code Pro, IBM Plex
  Mono, Fira Mono and Space Mono were each downloaded and their cmaps read on 2026-10-01.
  It fell back to a face with a different advance and silently dragged every line it sat on
  out of alignment. The key is spelled `Enter`. Anything outside ASCII and the box-drawing
  set used in the mockups gets the same treatment: check coverage, do not assume it.
- **Nothing is colour-only.** Every mark is a glyph first, so a pipe, a monochrome terminal
  and a screen reader all lose nothing. Amber means *this one is waiting for you* and is used
  for nothing else.
- **Never resume a conversation that is already running.** Two processes on one conversation
  fork it. Checked under the slot's lock, not before taking it.
- **Never start, stop or offload anything on the fleet on your own initiative.** Bring-up is
  the owner's. This applies to a cloud session too: you can build and test, you cannot deploy.

## Testing

The state machine is the product, so it is tested as one. Over the event table in
`docs/design.md`, at minimum: `/clear`, a resume, offload-then-`SessionEnd`, a nested
`claude` under a slot, an idle-prompt notification, a container restart with reused pids, and
two writers at once. Plus the always-exit-0 test above.

Rendering is tested against a test backend at **40×24 and 80×24** — the two widths the
mockups are drawn at — and the zero-idle-bytes property under a pty.

**Calibrate a check against known-good state before trusting its verdict.** A check that can
pass for the wrong reason is worse than no check, because it stops you looking. If a new test
does not read healthy against a working case first, the test is wrong.

## Release, and the contract with `infra`

- **Static musl binaries, cross-compiled on the runner** — `aarch64-unknown-linux-musl` and
  `x86_64-unknown-linux-musl`. Never compiled in the consuming image: that build's arm64 leg
  runs under QEMU and a Rust compile there is unusable.
- **A release is immutable.** `infra` pins a tag and checks a per-arch SHA-256. Re-cutting a
  published tag silently changes what a pinned, checksummed consumer gets, which is the one
  thing the checksum exists to prevent. Cut a new tag instead.
- `claude-sessions --version` must work on a bare static binary with no config and no network:
  the consuming image runs it as a **fatal** build check, and a static binary has no excuse.
- This repo stays **public**. The consuming build has no token.

## Git

- `main` is the only long-lived branch. Small, finished commits straight to it, pushed.
- **Commit messages are one lowercase sentence saying what changed and why**, with an area
  prefix where there is one (`registry: …`, `hook: …`, `ci: …`). The *why* is the point.
- Read `git diff --stat` before every commit and revert anything the change has no business
  touching.
- Never rewrite `main` history, never force-push.
- A commit that changes behaviour and leaves `docs/` describing the old behaviour is
  incomplete.

## Handoffs

`docs/handoff/YYYY-MM-DD-<topic>.md`, written at the end of a session for the next one.
**A handoff's opening instructions are the next session's first orders.** A note is deleted
once its open items are all closed *and* nothing in it is the only record of a decision —
move the durable reasoning into `docs/` first, then delete it. A handoff that outlives its
items becomes a second, stale source of truth.
