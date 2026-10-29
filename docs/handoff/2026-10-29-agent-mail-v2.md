# Handoff 2026-10-29: agent mail v2 — built on main, not released

**First orders for the next session: read this, say where things stand, and stop.** The
release is the owner's to cut. This note goes once 0.9.0 is on the fleet and a message has
moved through the server there; the durable reasoning is in `docs/design.md` § *Agent mail*,
the contract with infra in `docs/releases/v0.9.0.md`.

## State

- **Built to infra's brief of 2026-10-29** (message `20261029T021247.317Z` in the v1 inbox):
  `mail-server` in the hub (`src/mail.rs`, `src/mailbox.rs`, `src/http.rs`), the `mail`
  client, the hook on `SessionStart`/`UserPromptSubmit`/`Stop`, the nudge as an agent op
  (`agent::nudge`), the three mail skills in `skills/`. `mail-loop` and `mail-agent` are gone.
- **Not released.** `Cargo.toml` still says 0.8.1; the notes are written for 0.9.0. A bump is a
  publish, and the owner's.
- **The token path is settled** (infra, `20261029T084708.707Z`): the hub's `github-tokens`
  volume at `/home/dev/tokens`, the default now, and infra's compose sets `HUB_MAIL_TOKENS`
  beside `HUB_GITHUB_TOKENS`. Infra's cutover waits for 0.9.0 and its SHA256SUMS; nothing of
  v1 is torn down before then.

## The first run on the fleet

- **The nudge is reasoned, not seen.** It types the fixed line, waits 250 ms, sends `\r`. Watch
  one idle, detached session take it as a prompt (and the hook say the hub typed it). If
  Claude Code reads it as a paste and leaves the line unsubmitted, the gap is the knob.
- **The known-bad case first:** a send to an offloaded session must print `refused
  <addr> (offloaded)`, and one to a repo whose container is stopped `queued <repo> (its
  container is not answering)`. Only once those are seen does a delivered one mean anything.
- **A fresh slot may be refused for a second** (`not a live session the hub can see`) until
  its agent's next snapshot. The hook is silent about it and the next prompt picks the mail up.
