# Handoff — 2026-10-01, the repo exists and nothing is written yet

## First orders

**Read this, then [`../design.md`](../design.md), then [`../../CLAUDE.md`](../../CLAUDE.md),
and start at step 1 below.** There is nothing to review and nothing deployed, so there is no
reason to stop and report before working — but **do not skip step 1**, because it decides
where the hooks live and everything after it depends on that answer.

Two things you are not to do, both inherited and both meant literally:

- **Do not deploy, start, stop or offload anything on any box.** Bring-up is the owner's.
  Build and test here; that is the whole of your remit.
- **Do not change the approved screens.** [`../mockups.md`](../mockups.md) was approved on
  2026-10-01, colour included. If you believe one is wrong, say so and leave it alone.

## Where this came from

The design was worked out in the `infra` repo (private, same owner) as
`plans/claude-sessions.md`, which still holds the phase list and will be deleted when the last
phase lands. **Everything you need is in this repo** — `docs/design.md` is self-contained on
purpose, because that plan is temporary.

Phase 1 of that plan is already done and is the reason this tool is wanted now: Claude Code's
agent view is turned off fleet-wide by baked managed settings, which is what stops an idle
session costing a gigabyte. That is also the file these hooks want to live in, hence step 1.

## The order of work

1. **Settle the managed-settings consent question.** `docs/design.md` § *Open question* has
   the detail: the binary has a consent dialog for managed settings in certain categories, and
   whether `hooks` is one of them is unknown. Put a hook in
   `/etc/claude-code/managed-settings.json` in a scratch container, run `claude -p`, and see
   whether it blocks. **Write the answer into `docs/design.md`** — replacing that section, not
   appending to it — and say how you established it.
2. **`cargo init`.** One binary, subcommands as in the README. Pick the argument parser you
   like; there is no precedent in this repo to match yet, so set one.
3. **The registry and its state machine**, tested over the event table before anything reads
   it: `/clear`, resume, offload-then-`SessionEnd`, a nested claude, an idle-prompt
   notification, a container restart with reused pids, two writers at once. Per-slot `flock`
   with a timeout well under the 1.5 s `SessionEnd` budget.
4. **`claude-sessions hook`**, with the always-exit-0 test naming `UserPromptSubmit` and
   `Stop` — the two blocking events.
5. **`claude-sessions reconcile`** and **`doctor`**. `reconcile` is what makes the registry
   agree with reality after a container restart, including sweeping sockets whose server is
   gone with the attached bit still set.
6. **CI and the first release.** Cross-compiled static musl for `aarch64` and `x86_64`,
   per-arch SHA-256 in the release notes, and a smoke step that runs `--version` on the built
   binary. Then tell the owner the tag and the hashes: `infra`'s `Dockerfile.base` pins them,
   and that edit is a separate change in that repo, made by whoever is working there.
7. **Then the TUI**, to the approved screens. Its first commit should record, in whatever this
   repo's equivalent of a decisions file turns out to be, that it renders to
   `docs/mockups.md` as approved on 2026-10-01.

Steps 2–5 are one sitting's work if the tests are written alongside. Step 6 is where it stops
being a toy.

## Decided already — do not re-litigate

- The name is `claude-sessions`; the launcher is the default subcommand.
- One binary, not several, so the hook path and the menu share one registry implementation.
- Offload threshold: **10 minutes** after `Stop`.
- **A slot with a pending timer is never offloaded**, whoever set it. The menu marks it, so a
  forgotten `/loop` is visible and closable.
- Ages tick at most once a minute; an idle menu emits zero bytes.
- `Enter` on an offloaded row resumes immediately, no confirmation.
- `u` keeps its column, for sessions the menu did not start.
- Row titles come from Claude Code's own session title, else the first prompt truncated.
- `s` is a shell, `Esc` quits, `q` quits and is listed nowhere.
- The screens are ASCII plus the box-drawing set and `·`. **`Enter` is spelled out** because
  `↵` is in no monospace font on Google Fonts and silently misaligns every line it sits on —
  measured, see `CLAUDE.md`.
- Licence is AGPL-3.0-or-later. Public repo, because the consuming image's build has no token.

## Open, besides step 1

- **A long-interval wake tool**, deferred and possibly unnecessary. `ScheduleWakeup` clamps at
  an hour, and since a pending timer pins a slot, a loop waiting longer holds its memory the
  whole time. A tool that said *wake me in six hours with this prompt* would let `offload`
  stop the slot and resume it when due. Only worth building if long waits turn out to be
  common, and it needs a resumed session to accept a replayed prompt, which nobody has tested.
- **Where a local checkout of this repo should live** if it is ever worked on from inside a dev
  container rather than a cloud session. `/workspace` there is the `infra` clone and `infra`
  is config-only; the obvious spare path, `~/.local/share/claude-sessions`, is **the registry's
  own directory** and must not also be a source tree. The clean answer is a second bind mount
  in that container's compose file, which is the owner's to add.

## Delete this file

Once step 1 is answered in `docs/design.md` and the first release is cut, nothing here is the
only record of anything. Move whatever is still true into `docs/`, then delete it. A handoff
that outlives its items becomes a second, stale source of truth.
