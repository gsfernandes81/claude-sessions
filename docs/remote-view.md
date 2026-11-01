# The remote view (proposed, 2026-11-01)

**Status: a proposal being built, not yet in a release.** When it ships, its durable parts move
into [`design.md`](design.md) and this file goes. Until then [`design.md`](design.md) describes
what runs, and this file describes what is being built beside it.

## What it is, and the owner's decisions

A **client-side TUI** that runs on the owner's phone (Termux) and on WSL, drawing a Claude Code
conversation locally from structured data instead of streaming a terminal. Typing, scrolling and
dialogs are local, so they are instant, and the link carries only what changed. It runs **in
parallel** to today's zmx slots, which are untouched.

The owner's decisions, in the session of 2026-11-01:

- **Option 2 of three**: render Claude's conversation client-side from Claude Code's own
  structured protocol, not mirror a terminal (mosh-style) and not a hybrid.
- **The scrollback is the TUI's own**, in a full-screen view, not the terminal's buffer.
- **The client sends minimal data back and forth to the hub, and the hub speaks Claude Code's
  JSON control protocol to the containers.**
- **Copying is a `/copy` command**, as Claude Code has.
- **Not attachable while it runs.** A conversation open in the remote view is not also attached
  through zmx; the owner ends it and resumes it in a normal slot when needed.
- **Look and feel as close to Claude Code's as possible**, against a pinned Claude Code version.
- **The pin is fleet-wide**, for every session, not only the remote view's (below).
- **The order of authority** for a design question: Claude Code's existing design first, then a
  Fable advisor, then the owner.
- **MVP**: model swapping, permissions, AskUserQuestion, effort changes, compaction, `/rename`,
  exit, and `/copy`.

## The pin

**Claude Code 2.1.311, fleet-wide.** The owner allowed 2.1.311 "if there are no known memory
leak issues or critical issues" (2026-11-01). Checked that day:

- **The issue tracker.** 119 issues mention 2.1.311. None reports a memory leak in it; the open
  leak reports are about the Desktop app, mostly on macOS. The 2.1.311 regressions reported are
  Japanese IME Enter, a Windows rclone working directory, VS Code effort controls for `[1m]`
  models, and the x64 build under Rosetta. None affects arm64 Linux containers.
- **The fleet.** It has run 2.1.311 since 2026-10-27. A busy 45-minute session sat at about
  460 MB RSS.
- **What it gains.** It fixed "`claude -p` and SDK sessions saving transcript messages out of
  order", and the remote view's conversations are exactly such sessions.

The first pick that day was **2.1.309**, the newest of the latest three stable releases
(2.1.306, 2.1.308, 2.1.309: Anthropic's apt stable repository). 2.1.311 was never on the stable
channel; the base image installed npm's `latest` on 2026-10-27, and the stable channel never
downgrades.

**What this repo reads is identical across 2.1.309, 2.1.311 and 2.1.316**, read from the three
binaries on 2026-11-01:

- the hook events
- the three `CLAUDE_CODE_DISABLE_*` names (with `grep -w`)
- the stream-json message types (15 at the top level)
- every control request below

**The pin is infra's to set**, because its base image installs Claude Code. The request went
to infra by agent mail on 2026-11-01. **A bump is a checklist**, run before the pin moves:

1. infra's `scrollvars` line
2. this repo's tests
3. the protocol spike below, run against the new version, with the message shapes compared

## The protocol, measured (2.1.311, 2026-11-01)

The command line:

```sh
claude -p --input-format stream-json --output-format stream-json --verbose \
  --include-partial-messages --permission-prompt-tool stdio \
  [--model M] [--effort L] [--resume ID | --session-id UUID]
```

**`--permission-prompt-tool stdio` is required.** Without it, a tool that needs permission is
denied on the spot (`system/permission_denied`, no request reaches the host), and
`AskUserQuestion` is missing from the tool list (26 tools instead of 29). `--tools
default,AskUserQuestion` is read as one unknown name and leaves only the MCP tools.

Each MVP feature, as driven by a script against the real binary (`src/sdk.rs` builds and reads
these lines, and its tests hold the exact shapes):

| Feature | Sent | Answered |
|---|---|---|
| start | `control_request` `initialize` | commands (skills included), models with each one's `supportedEffortLevels`, account, output style: about 38 KB |
| permission | claude sends `can_use_tool`: tool name, input, description, suggestions | `control_response` `{behavior:"allow", updatedInput}` or `{behavior:"deny", message}`. A read-only command such as `echo` is allowed without asking. |
| question | `can_use_tool` with tool `AskUserQuestion`, `input.questions[]` | allow, with `updatedInput` = the input plus `answers: {question: label}` |
| model | `set_model {model}` | `{model: <resolved id>}`, plus a `user` line `<local-command-stdout>Set model to …` |
| effort | `apply_flag_settings {settings:{effortLevel}}`; `null` resets | success. There is no effort request of its own; Claude Code's own `/effort` sends exactly this to a remote session. |
| rename | `rename_session {title, source:"host", session_id}` | success, and `system/session_title_changed {title}` |
| compact | `/compact` as a user message | `system/status` compacting, the `SessionStart:compact` hooks, `system/compact_boundary {pre_tokens, post_tokens}`, the summary as a `user` line, `<local-command-stdout>Compacted`, a `result` |
| interrupt | `interrupt` | `{still_queued:[]}`, then a `user` line `[Request interrupted by user]` and `result/error_during_execution` |
| exit | `end_session` | success; the process exits |

**It is verbose.** One small session wrote 177 KB: the `initialize` answer, a `system/init`
listing every tool and command before every turn, two lines per hook run, thinking-token
estimates, rate-limit readings, and a stream event per delta. `get_context_usage` alone answers
with about 47 KB. That is why the hub reduces it before the metered link.

Hooks fire as they do for an interactive claude; the registry's events are unchanged.

## The pieces

```
view (Termux / WSL)  --ssh -T dev claude-sessions relay-->  relay (hub)  --ssh -T <c> claude-sessions tap <slot>-->  tap  --unix socket-->  holder (in the slot's zmx session)  --pipes-->  claude -p
```

| Piece | Runs in | Job |
|---|---|---|
| `claude-sessions view` | the owner's phone or WSL | draws the conversation, edits the prompt locally, owns the scrollback |
| `claude-sessions relay` | the hub, one per connected view | reaches each container, **reduces** Claude's lines to compact items, carries the view's actions back as Claude's control messages |
| `claude-sessions tap <slot>` | the container, over the hub's `ssh -T` | streams the holder's log from a sequence number; carries lines in |
| `claude-sessions holder <slot>` | the container, as a zmx session's program | owns headless claude's pipes, numbers and logs every line, serves taps, draws one status line |

**The reduction is in the hub.** The owner's brief puts it there: the view talks a minimal
protocol to the hub, and the hub speaks Claude's protocol to the containers. The Fable advisor
argued for reducing in the container instead (one parser, beside the pinned claude). The
owner's words decide it. The cost is small because both halves are this one binary and read
lines through the same `src/sdk.rs`.

## Decisions, by the order of authority

Claude Code's own design settles the look: its fullscreen renderer is the model. The internal
questions below went to a Fable advisor on 2026-11-01, and its answers are adopted except where
the owner had already spoken (above). Two items need the owner, and both are mockups.

- **A remote slot is a claude slot.** It keeps the `claude-<n>` name and is not a new harness.
  It gets one record field, `driver: "holder"`; when the field is absent, the slot is driven by
  a terminal, as today.
  - Old hubs and agents read such a record as a plain claude row.
  - Archived, mail addresses and the offloader are unchanged.
  - Resuming it in a normal slot is the same record with the field cleared.
  - A mark on remote rows in the menu, like pi's dim `pi: `, changes the approved mockups.
    **Owner.**
- **The holder runs under zmx.** zmx already provides the listing, the offloader's teardown,
  `ZMX_SESSION` and the attach guard. The holder:
  - writes one plain line at start and on each state change, never on a timer, so attaching to
    it from the old menu shows what it is;
  - discards its stdin, because the mail nudge types into detached sessions;
  - writes claude's stderr to `<registry>/<slot>.stderr`, as the launcher does, so a failed
    start is still reported.

  The hook's binding admits `claude` → `claude-sessions` → `zmx` as the slot's own claude. A
  `claude -p` nested under that claude still reads as nested.
- **History comes from two sources, converted into one shape.**
  - **The holder's log** covers everything it saw, in order and addressable by sequence number.
    It is kept per conversation, as claude's raw lines, so a better reducer can reread it.
  - **The transcript JSONL** covers what came before: on a `--resume`, stream-json does not
    re-emit history.
  - **Compaction** is one item in the history, drawn as Claude Code draws it; the scrollback
    stays.
- **No application heartbeat; liveness is detected lazily.** Idle is zero bytes. The link is
  dead when:
  - the ssh child exits;
  - something the view sent gets no acknowledgement within about 5 seconds;
  - a keypress after 2 minutes of silence gets no answer to the probe it sends first.

  ssh's own `ServerAliveInterval` is the owner's ssh config. The view reconnects with backoff
  from 1 to 30 seconds and a hello naming the last sequence number it holds. While
  disconnected, the scrollback stays readable and typing still works, but sending is refused,
  and the footer says so in words. That footer is a screen Claude Code does not have.
  **Owner**, for its mockup.
- **A prompt waiting with nobody connected is the amber `!` the menu already has.** Hooks fire
  under `-p`, so `Notification` sets `needs_you`. There is no timeout, as Claude Code has none.
- **A connected view keeps the slot loaded.** zmx never sees a remote slot as attached. So while
  a view is connected, the tap re-stamps `keep_until` the way `keepalive` does, and writes
  `last_attach` when a view connects and when it leaves. That keeps unread right too.
- **Several views at once are all read-write, and the first answer wins.** A late answer to a
  prompt is turned down as answered elsewhere, never applied to the next prompt.
- **The wire is JSON lines with short keys, through `ssh -C`.** A `v` and a feature list go both
  ways in the hello; unknown item types are dropped. `initialize`'s 38 KB answer is cached by
  its hash, and context usage is fetched only on demand.
- **Opening an offloaded conversation goes through the launcher's plan**: under the slot's lock,
  never resuming a conversation that is already running, with the holder as the command.

## Build order

0. **Measure first.** Measured on 2026-11-01 unless marked:
   - **zmx creates a session from a client with no terminal.** `zmx attach <name> <cmd>
     </dev/null` returns 0 at once, and the session stays, with 0 clients and its program the
     daemon's direct child. `zmx run -d` is no use here: it types the command into a login
     shell, which becomes the parent.
   - **`claude -p` reads `claude`** in `/proc/<pid>/comm`.
   - **TERM ends `claude -p`** with status 143 in 0.7 s, and `--resume <id>` then continues
     the same conversation with what it said intact. So the offloader's TERM is safe for a
     remote slot.
   - **Not yet measured:** that the hub's door passes a given command through. That is
     infra's to confirm.
1. `src/sdk.rs`: read and write Claude's lines. **Done.**
2. The reducer: Claude's lines into items, as a pure function, tested on fixtures.
3. The holder, its log, the `driver` field and the binding, tested under the real zmx.
4. The tap: streaming from a sequence number, lines in, `keep_until` and `last_attach`.
5. The relay.
6. The view, once its mockups are approved.
