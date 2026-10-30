# Handoff 2026-10-30: pi as a second harness — released as 0.10.0, not yet seen on the fleet

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

1. **Infra's measurements came** (`20261030T051247.788Z` and its correction
   `20261030T051639.420Z`, amd64 runner): comm `pi`; the pi process alone about 1.2 s CPU
   over 180 s idle and 258 MB RSS, its one child (`ty`, the Python language server pi-lens
   starts) not summed in; the offloader's own per-slot readout in the same run counted the
   two processes at 290 ms for one window; nothing surviving TERM; both binding checks pass
   (pid under zmx, empty hook.log; bind takes about 21 s on the runner). `SESSION_COST`
   stays, no sweep rule.
   Idle CPU on arm64 is unproven and infra will mail what the first slot on zero shows.
2. **One finding from that run, fixed in the tree after 0.10.0:** a fresh pi slot read
   `needs_you` before any prompt. The released extension spawned every fire-and-forget hook
   independently, so a `ui_prompt_start`/`ui_prompt_end` pair fired a microtask apart could
   land end-first within one kernel tick and leave the `!` set; the extension now hands
   events on one at a time (`tests/pi_extension.rs`, design § *Harnesses*). That run's `!`
   was not the race: its record carried `Notification` alone, 220 ms after `SessionStart`,
   and no `PromptAnswered` (`20261030T052134.104Z`), so one of infra's packages opened a
   dialog at startup that stayed open the whole window and the mark was correct. The
   package is `pi-powerline-footer`, whose welcome card is an 18 s countdown modal
   (`20261030T110702.067Z`); infra turns it off in the footer's config. **The fix reaches
   the fleet only in a release** — a 0.10.1 bump is the owner's call, and infra bumps its
   pin when the tag exists.
3. **Also for 0.10.1, fixed in the tree:** pi parses a skill's frontmatter as strict YAML,
   and the unquoted `description:` of mail-read and mail-reply held `: `, so pi dropped
   both skills at start on the first fleet slot (same message). The two values are now
   double-quoted and `install::tests::every_skill_frontmatter_is_strict_yaml` guards all
   four (calibrated: it failed on the old files).
   *For the owner, not built:* infra suggests keeping the open prompt's `title` (which
   `ui_prompt_start` carries) in the record so the menu could say what a slot waits on. That
   is a change to the approved screens, so it waits on the owner.
4. **Released:** v0.10.0 was cut by the owner on 2026-10-30 (eda6afd), both binaries and
   SHA256SUMS published; infra was mailed the tag and pins it in dev/hub/Dockerfile.
5. Delete this note once the release is pinned and a pi slot has been seen on the fleet.

## The design as sent

The hub holds the message (outbox stamp `20261030T021539.802Z`); the decisions above are
its content, and go into `docs/design.md` § *Harnesses* as they are built. Infra agreed on
2026-10-30 (`20261030T021656.261Z`): one program table, harness on record and wire, the
extension linked by its entrypoint, `tuiMode: regular` baked, measurements from its smoke run.

