# Handoff 2026-10-28: agent mail — built and released as 0.8.0, infra's side next

**First orders for the next session: read this, say where things stand, and stop.** Nothing
here is started without the owner. This note goes once infra pins 0.8.0 and the first real
message has moved on the fleet; the durable reasoning is already in `docs/design.md` § *Agent
mail*.

## State

- **0.8.0 carries the claude-sessions half**: `mail-loop` in the hub, `mail-agent` in each
  container, `mail-hook` in the drop-in, the read cursor. `tests/mail.rs` proves it over a
  stand-in ssh: a message that moves, each refusal bounced, a receiver down then back, a full
  queue, the hook's one line or nothing. `docs/releases/v0.8.0.md` is the report infra asked
  for (env names, the volume layout, the hook's line, the issue number); CI appends it to the
  release notes.
- **The owner's view and the idle-session nudge are #17**, deferred, not built.
- **Infra's items** (its handoff `docs/handoff/2026-10-28-agent-mail.md`): the policy file and
  its one-shot service, the three skills, the `mail` volume and the `HUB_MAIL_*` lines in
  compose, the entrypoint starting `claude-sessions mail-loop` supervised, the
  `.git/info/exclude` line, the CI round trip, and the pin bump to 0.8.0.

## The first real run, when the owner brings it up

- A container's `.mail/` is in the workspace volume: the hook finds it at `/workspace/.mail`,
  and the loop reads nothing a container did not put in `out/`. If infra's entrypoint has not
  yet added the exclude line, `.mail/` shows in `git status` — harmless, ugly.
- `install` never removes a file it did not write, so the skills infra puts beside the
  keepalive skill in the volume survive it (the coordination point infra asked about).
- The known-bad case on the fleet: a file in `out/nowhere/` must come back as mail from `hub`
  in `in/hub/` within a tick, and `~/.hub/`'s mail log must say `refused, unknown receiver`.
  Only once that is seen to fail does a passing round trip mean anything.
