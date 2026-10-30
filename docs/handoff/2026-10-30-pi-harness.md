# Handoff 2026-10-30: pi as a second harness — design sent to infra, nothing built yet

**First orders for the next session: read this, check `/workspace/.mail/in/infra/` for the
answer to the design (stamp `20261030T021528Z-pi-harness-design`), and keep a watcher on the
inbox while you work** (the owner's standing instruction for this work: take instructions
from infra's agent on it, push back where something looks wrong for this repo).

## State

- **The brief** is infra's message `20261030T020644.628Z` (2026-10-30): nine requirements
  from the owner for pi (pi.dev, `@earendil-works/pi-coding-agent` 1.4.0) as a slot —
  `pi-<n>`, started from the menu, offloaded by the same rule, resumed by `Enter`, listed under
  Archived, mail address `<repo>/pi-<n>`. The approved plan is infra's `plans/pi-harness.md`,
  section *In claude-sessions*.
- **The design went back the same day** (outbox copy below, in full). Its decisions, in
  short: a `harness` field on record and wire (absent = claude); `CLAUDE_SESSIONS_PI` beside
  `CLAUDE_SESSIONS_CLAUDE` in one table; launch `pi --session-id <uuid> --tui-mode regular` with
  id and cwd in the record before zmx runs; **events from a pi extension** (plain JS, shipped by
  `install` as `extensions/claude-sessions.js`, linked by infra's entrypoint) that feeds the
  same `claude-sessions hook` with `{"pi_event":…}` bodies, mapped onto the existing event
  table; a pi JSONL reader for titles, exchange and Archived; `p` key for a new pi session;
  a dim ASCII `pi: ` title prefix proposed, both needing the owner.
- **Measured from the package, not a run:** pi's `tuiMode` defaults to fullscreen (alternate
  screen plus mouse tracking, the issue #8 combination); `--tui-mode regular` is the twin of
  the three `CLAUDE_CODE_DISABLE_*` variables. The session's permission classifier refused to
  run the downloaded pi binary here, so comm, idle CPU and RSS are infra's to measure (asked).
- **Waiting on:** the owner's word on `p` and the `pi: ` prefix (via infra); infra's
  measurements; infra linking the extension at container start.

## Built (2026-10-30, this session), all gates green

Points 1-7 and 9 of the design: `src/harness.rs`; the harness on record, row key, dialog and
wire; `src/pisession.rs`; the launcher's program table, `pi-<n>` allocation with a fresh id,
`--session-id … --tui-mode regular`; `events::from_pi` with `PromptAnswered` and
`TitleChanged`; `bind.rs` admitting `pi`; `extensions/claude-sessions.js` shipped by `install`
and printed by `claude-sessions pi-extension`; Archived listing pi's sessions; the agent ops
`new_pi` and `offload_then_open_pi`; `Action::New(host, harness)` in the menu. Tests: the
state machine over pi payloads, the pi start line and allocation, a resume with pi's line,
the hook contract, and a stand-in pi bound end to end under the real hook (`tests/launch.rs`).
`docs/design.md` § *Harnesses* has the reasoning.

The owner's answers came the same day (`20261030T030016.063Z`): `p` for a pi session and the
dim `pi: ` row mark, both built — hint line, keys screens, the mark, the mockups redrawn at
both widths from the renderer (`print_the_mockups`, then `write_the_styled_mockups`).

## Still open

1. **Infra's measurements** (comm, idle CPU, RSS, what survives TERM) — may change
   `mem::SESSION_COST` per harness or add a sweep rule; otherwise nothing.
2. **Release 0.10.0**: the bump to 0.10.0 in `Cargo.toml` and `Cargo.lock` was prepared on
   2026-10-30 but the session's permission layer refused the commit that publishes it; the
   owner cuts it (`git commit` of the bump on main, pushed; CI tags `v0.10.0`), then mail infra
   the tag (`20261030T030016.063Z` thread). Infra runs its two smoke checks against the tag.
3. Delete this note once the release is pinned and a pi slot has been seen on the fleet.

## The design as sent

The hub holds the message (outbox stamp `20261030T021539.802Z`); the decisions above are
its content, and go into `docs/design.md` § *Harnesses* as they are built. Infra agreed on
2026-10-30 (`20261030T021656.261Z`): one program table, harness on record and wire, the
extension linked by its entrypoint, `tuiMode: regular` baked, measurements from its smoke run.

