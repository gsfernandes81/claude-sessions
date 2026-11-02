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
- **The client is its own program on the phone** (artifact comments, 2026-11-01). It is run as
  `claude-remote dev`, takes an ssh name, and reaches the hub over an ssh tunnel that
  reconnects on its own; nothing new is started from the hub's door. Its home is the door's
  list, drawn on the phone:
  - sessions running in a terminal are listed but cannot be opened there;
  - sessions not running, and sessions already in remote mode, open on Enter;
  - `n` starts a new remote-mode session, if that is not too hard for the MVP.

  A session's content streams only while it is open. Scrollback was fetched eagerly at
  first; since 0.13.7 it is lazy end to end (§ *Lazy history*, the owner's ask of
  2026-11-02).
- **No key chords for the phone.** The mode line keeps Claude's name and icon, but its hint is
  `/auto to cycle` instead of shift+tab, which is awkward in Termux.
- **Question options may be long, and they scroll** around the cursor whatever Claude Code does.
  Claude's preview layout is drawn side by side at 80 columns and stacked under the list at 40.
- **Anything not essential is deferred** from the MVP, with a list kept for later.

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
claude-remote dev (Termux / WSL)  --ssh -T dev claude-sessions relay-->  relay (hub)  --ssh -T <c> claude-sessions tap <slot>-->  tap  --unix socket-->  holder (in the slot's zmx session)  --pipes-->  claude -p
```

| Piece | Runs in | Job |
|---|---|---|
| `claude-remote <ssh name>` (proposed: a second name for this binary) | the owner's phone or WSL | the door's list and the conversation, drawn locally; edits the prompt locally, owns the scrollback, reconnects on its own |
| `claude-sessions relay` | the hub, one per connected view | reaches each container, **reduces** Claude's lines to compact items, carries the view's actions back as Claude's control messages |
| `claude-sessions tap <slot>` | the container, over the hub's `ssh -T` | streams the holder's log from a sequence number; carries lines in |
| `claude-sessions holder <slot>` | the container, as a zmx session's program | owns headless claude's pipes, numbers and logs every line, serves taps, draws one status line |

**The reduction is in the hub.** The owner's brief puts it there: the view talks a minimal
protocol to the hub, and the hub speaks Claude's protocol to the containers. The Fable advisor
argued for reducing in the container instead (one parser, beside the pinned claude). The
owner's words decide it. The cost is small because both halves are this one binary and read
lines through the same `src/sdk.rs`.

## Lazy history

What the phone is sent is a few screens, not the conversation: the owner's ask of
2026-11-02, "we only load parts of the transcript as we scroll (without interrupting the user
unless they hit the edge of what's loaded). The amount loaded should be a multiple of the
screen's rows", and, asked whether the container must still read the whole history, "Seems
wasteful no?". It need not: only claude reads it whole, as its context, which took 1.75 s for
this 12 MB conversation; everything the view added on top — the holder copying the transcript
into its log, the tap streaming all of it to the hub, the relay reducing every line, the phone
receiving every item — is gone.

- **The phone** says its columns and rows (`w`, `r`) in `open`, `new` and `re`. It is sent
  about three screens of rows, the newest, then `{"t":"more","m":…}`: whether anything is
  before them. Scrolled to within a screen of the top of what it has, it sends `older`, once
  until the answer, and gets `{"t":"old","o":[ops…]}`, the stretch before, drawn above what
  it has; the view is measured from the bottom, so it does not move. Only at the very top,
  before a stretch has come, does it stop, with `Loading earlier messages…` under the logo.
- **The relay** keeps every item with the ops that made it, a text's pieces folded into it and
  a result or progress replacing the one before, so any stretch can be sent as it now stands.
  Ops of items the phone has go on as before; of items it has not been sent, nowhere until
  it scrolls to them. A reconnect sends the end again (`reset`), since what the phone held of
  the middle is not known. Its estimate of an item's rows is for loading by, not drawing.
- **The holder** keeps the transcript's length at its start — claude appends to the same file
  from there — and says `"older":true` in its header when there is any. A tap's
  `{"type":"_older","before":B,"want":N}` is the holder's, never claude's: it reads the file
  backwards from byte B and answers on one line, `H<TAB>{"type":"_older","before":B2,
  "more":…,"lines":[…]}`, with at least N prompt and reply lines, back to a prompt that starts
  a turn, so a tool call is never cut from its result. The relay reduces each stretch on its
  own, its items numbered from 2^40 so they never meet the log's.
- Measured on this conversation's transcript (12 MB, 5,341 lines): 18 stretches of whole
  turns, each read in 10 to 80 ms, 4 to 7 KB to the phone each, 322 KB in all for the whole
  history scrolled back to its first line.
- **Older pieces.** A holder from before 0.13.7 has copied its history into its log and says
  no `older`: the relay keeps it all and sends the phone the end, as above. A client from
  before 0.13.7 sends no size and is sent everything in the log as before, which no longer
  holds a resumed conversation's history: install the client with the hub.

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

  ssh's own `ServerAliveInterval` is the owner's ssh config. The link is not silent all the
  same: the hub's sshd sends `ClientAliveInterval 30` with `ClientAliveCountMax 3`, so it
  pings every 30 seconds and drops a dead link after about 90 (infra, 2026-11-01). The phone
  reaches the hub through `cloudflared access ssh`, whose idle timeout is not yet measured.
  It must be measured from Termux before the dead-link rules are fixed. The view reconnects with backoff
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
   - **The hub's door passes a given command through**, confirmed by infra from its code on
     2026-11-01 but not yet run end to end: with `SSH_ORIGINAL_COMMAND` set, it execs the
     command before any terminal test. So `ssh -T -C dev claude-sessions relay` runs relay.
     The containers' door passes commands through the same way.
1. `src/sdk.rs`: read and write Claude's lines. **Done.**
2. `src/reduce.rs`: Claude's lines into numbered items with short keys, as a pure function tested on 2.1.311 fixtures. **Done.**
3. The holder, its log, the `driver` field and the binding, tested under the real zmx.
   **Done.**
   - `src/holder.rs` is the holder and `claude-sessions tap`. One run end to end on
     2026-11-01: zmx → `claude-sessions holder` → a real `claude -p`, a tap sending a prompt
     and reading back numbered lines and the answer, and the zmx screen saying starting,
     working, idle.
   - `src/bind.rs` admits that claude as the slot's own, which unit tests cover. The real hook
     confirms it only from the release that carries this, since the installed hooks run the
     released binary.
   - A holder killed outright (`zmx kill`) leaves its socket file, and the next start removes
     it.
   - **The `driver` field** is in the record (`registry::Driver`), written only as
     `"driver":"holder"`. The launcher's plans take a driver: the menu's resume and
     allocation write `Terminal`, so a holder's slot resumed at a terminal is a terminal's
     again, and the remote view's write `Holder`.
   - **`claude-sessions remote new [DIR] | open SLOT | resume ID CWD TITLE`** is what the
     relay runs in a container, over one `ssh -T`. It opens the slot for the view through the
     launcher's own plans (the allocation lock, the slot's lock, never resuming a running
     conversation), with `zmx attach <slot> env … <holder> holder [--resume ID]` from no
     terminal, and waits for the holder's socket and a second's settling. Its first line out
     is `{"slot":"claude-4"}` or `{"error":"why"}`, and then it is the slot's tap. A slot a
     terminal is running is refused with words to say so (mockup R10); so is a pi slot. A
     start that dies puts the record back where it was, and the error carries claude's first
     line of stderr. One run end to end on 2026-11-01 under an isolated zmx and registry: a new
     slot, a prompt through it, `pong` back.
   - **Mixed releases lose the field.** The installed hooks on this dev container run the
     released binary, which rewrote that run's record without `driver` and did not bind the
     holder's claude. On the fleet, hooks and `remote` are the one installed binary, so this
     holds only until the release that carries the holder.
4. The tap: streaming from a sequence number, lines in, `keep_until` and `last_attach`.
   **Done.** While a tap is connected it stamps `keep_until` three minutes ahead every minute
   (never shortening a longer keep-alive asked for from inside the slot), and writes
   `last_attach` when it connects and when it goes, each under the slot's lock with the
   interactive timeout, skipped rather than waited on. The end of its input is the relay
   gone, and the tap leaves at once, both ways.
5. The relay.
   - **Done first: what views send is in the log.** The holder logs each line a view sends,
     as `{"type":"_in","line":…}`, ahead of anything claude does with it, and drops a second
     answer to one request, so the first answer wins at the one place every answer passes.
     The reducer reads those lines back (`sdk::Input`): an answer resolves its prompt for
     every view, and a relay that reconnects and reads the log again finds it answered
     rather than waiting; a change of model, mode or effort shows at once. Effort is carried
     in the meta op as `e`, since claude's own lines never say it.
   - **Then the relay**, `src/relay.rs`: `claude-sessions relay` on the hub, for one client
     on stdin and stdout. It holds an agent per container as the hub's menu does and passes
     each snapshot on whole, so the client draws its home with the menu's own code; a row
     the holder drives carries `"remote":true` (`Row::remote`, from the record's `driver`).
     It opens one conversation at a time with `claude-sessions remote …` in its container,
     reduces its lines, and turns the client's actions into claude's control messages. One
     run end to end on 2026-11-01 with the local container and a real claude: hello, home,
     `new`, a prompt, `pong` streamed back, the model list twice (a second `initialize` is
     answered like the first), `exit`. **3.6 KB reached the client for all of it**, the home
     included.
   - **The wire, v1.** JSON lines, `t` the type, through `ssh -T -C`:

     | From the client | Means |
     |---|---|
     | `hi {v}` | first; answered `hi {v, hosts}`, then the home |
     | `open {c, k}` · `new {c}` | open a row's conversation (`k` is the row key as the agent sends it) or a new one in container `c` |
     | `re {c, s, run, n}` | after a reconnect: slot `s`, the holder's run, and the last `n` the client holds |
     | `close` | back home; the conversation keeps running |
     | `offload {c, k}` · `archive {c, k}` | the home's `c`: the container's agent's `offload` or `close`, as the door sends them; answered `did {ok, x}` |
     | `say {x, u}` | a prompt or slash command, `/compact` included; sent to claude with `u`, the uuid the phone keeps it pending by, or the relay's own from an older client |
     | `unqueue {u}` | Up: claude's `cancel_async_message` for each uuid, prompts waiting behind the turn taken back to edit |
     | `now` | Ctrl+X Ctrl+S: the prompts waiting behind the turn go now — claude's `background_tasks` (Ctrl+B) while a tool call runs, so the turn takes them in, else its `interrupt` with `send_now` |
     | `perm {r, a}` | `a` is `y`, `a` (always) or `n`, with `x` for what claude is told |
     | `ans {r, a}` | `a` maps each question to its answer; anything else declines |
     | `model {m}` · `effort {e}` · `mode {m}` · `rename {x}` | as Claude's own requests; `e` null is the default |
     | `esc` · `exit` · `models` · `full {id}` | interrupt, end the session, the model list, a tool result whole |
     | `btw {x}` | a side question, answered by its own claude beside the conversation (`claude-sessions remote btw SLOT [MODEL]`, the question on its stdin) and never added to it |

     | From the relay | Means |
     |---|---|
     | `hub {h}` · `snap {c, s}` · `down {c, x}` | the home: the hub's header, a container's snapshot, one not answering; only while no conversation is open, and only when one reads differently |
     | `open {c, s, run}` · `reset` | a conversation is open; `reset` first when the client's run is stale |
     | `back {x, u, q}` | an interrupt caught a prompt (claude's `command_lifecycle` `cancelled`, by its uuid `u`): its text, and whether another waited behind it; an older client skips it |
     | `add` · `app` · `set` · `res` · `done` · `st` · `meta` · `prog` | the reducer's ops (`reduce.rs`); a question's `done` carries `a`, its answers keyed by question, which an older client ignores; a subagent or workflow is `add` of kind `agent` (`n` its type or `Workflow`, `a` its description), and `prog {id, p, st}` replaces the lines under it, `st` being `running`, `completed`, `failed` or `stopped`; an older client skips both |
     | `n {n}` | after each batch: every op of lines up to `n` has been sent |
     | `models {m, c}` · `full {id, x}` · `err {x}` · `end {x}` | answers, a refusal, the conversation's command gone |
     | `btw {x}` or `btw {e}` | a side question's answer, or why there is none; only to the conversation it was asked in |
6. The view, to its screens: approved by the owner on 2026-11-01 as mockups R1–R10 in
   [`mockups.md`](mockups.md) § *The remote view* (R1 and R4b also in
   [`mockups-80.md`](mockups-80.md)). They are binding as the other mockups are.
   **Built, 2026-11-01**: `claude-remote <ssh name>` (`src/client.rs`), the same binary
   called by that name or as `claude-sessions client NAME`; the conversation's model and
   drawing are `src/convo.rs`, tested as text against R1 to R8 at 40 columns.
   - **The home is the hub's own menu** (`Menu::hub`) fed the relay's snapshots, with R9's
     hint line (`View::remote`). A running session no holder drives is dim and `term: `, and
     Enter on it puts R10's words where the hint line was.
   - **The link** is `ssh -T -C -o BatchMode=yes <name> claude-sessions relay`
     (`CLAUDE_REMOTE_SSH` and `CLAUDE_REMOTE_RELAY` override both halves), redialled with
     backoff from 1 to 30 seconds; dead too when something sent has no answer in 5 seconds,
     and a key after 2 minutes of silence sends a `ping` first. So the relay answers every
     message at once, with `pong` when it has nothing else to say yet: a side question takes
     6 to 11 seconds, and through 0.13.6 a quiet conversation's link was dropped before its
     answer came, which went to the dead link's relay (owner, 2026-11-02, "/btw never gets
     back to me"); a side question whose link drops anyway says so in its panel. Back up, it says `hi` and,
     with a conversation open, `re`. An open waits 60 seconds, not 5,
     and the relay answers it at once with `opening`: starting a holder and reading a long
     conversation's history into it takes seconds, and until 0.13.6 the 5-second wait cut the
     link mid-open, the new link's relay knew nothing of the conversation, and every prompt
     typed into it was refused (owner, 2026-11-02, on this very conversation). What is typed
     while a conversation opens waits, spinning, in the client and in the relay, and goes the
     moment it is open; a link that drops mid-open opens the conversation again, or for a new
     session goes home, where it is listed if it started, rather than start a second. A
     rename before claude has said its session leaves `session_id` out, which claude reads as
     its own, and a request claude refuses is said to the client.
   - **Esc, send-now and pastes, as Claude Code 2.1.311 does them** — driven in a pty and
     read off its screen on 2026-11-02, since the owner asked for its behaviour:
     - Esc on a prompt nothing has answered yet — still spinning, or taken in and thinking —
       with nothing queued behind it, hands it back: off the transcript, its text in the
       input. Once anything is drawn, or with a prompt waiting behind it, the prompt stays,
       under `⎿  Interrupted · What should Claude do instead?`, and the waiting one runs.
       claude's own `cancelled` lifecycle cannot tell these apart, so the reducer says
       which prompt was caught and whether one waited (`back`), and the phone decides by what
       it has drawn. A conversation read back from the log shows the rewind but fills no input.
     - A prompt sent while a turn runs waits behind it, with `ctrl+x ctrl+s to send now` under
       it; Ctrl+X then Ctrl+S sends it now, as the terminal does: a tool call running goes to
       the background and the same turn takes the prompt in — `background_tasks`, since headless
       claude's own `send_now` stops the turn and its call instead (`"send_now":"stopped"`,
       measured 2026-11-02) — and with none running, `send_now` drops the turn quietly. Up —
       which an empty prompt hints at with `Press up to edit queued messages` — takes every
       waiting prompt back into the prompt, a line each, above anything typed meanwhile, as
       Claude Code 2.1.311 does (driven in a pty, 2026-11-02), and withdraws them from
       claude's queue; the turn runs on. Claude Code's Esc Esc rewind
       picker is not built.
     - A prompt queued behind a turn of tool calls is taken in by claude between two of them,
       on its own: 2.1.311 headless, as the holder runs it, echoes it and marks it `started`
       after the call it waited behind, and the same turn answers it, as the TUI does. The
       phone draws it where claude takes it, between the calls; nothing is sent for it.
     - Typing a command's name, the **suggestion list** stands over the prompt as Claude
       Code's does: claude's commands (`initialize`'s, with what each does, asked for when a
       conversation opens) and the phone's, those that start with what is typed first, then
       those that have it further in; the name in a column 40% of the width, what it does in
       two lines at most, five lines of list at most, the chosen one `❯` and lavender. Up and
       Down choose, Tab puts it in the prompt with its argument hint grey after it, Enter
       sends it, Esc puts the list away. Claude Code also orders by what was used lately,
       which the phone does not know.
     - **A drag over the conversation selects** and letting go copies, as Claude Code's
       fullscreen mode does with "copy on select": it holds the mouse, so the terminal's own
       selection cannot work, and nor can the phone's while it reads the wheel. The phone asks
       for motion while a button is held (`?1002`, its terminal only), lights the cells in
       Claude Code's `selectionBg`, `264f78`, until the next key, and copies through
       `termux-clipboard-set` and OSC 52 as `/copy` does, saying so in Claude Code's words:
       `copied N chars to clipboard`, or `sent N chars via OSC 52 · if paste fails, hold
       shift while selecting for native copy`. The menu ignores a release or a drag.
     - A name typed whole in the suggestion list is that command on Enter: headless claude
       lists `design:ux-copy` and no `copy`, so `/copy` once became it.
     - A slash command claude or the phone knows is lavender in the prompt, wherever it is,
       as Claude Code draws it (`b1b9f9`, its `permission` colour); `/` before an unknown name
       is text. claude's commands come in the meta op's `sc`, from its init's `slash_commands`.
     - A bracketed paste (`?2004`, the phone's terminal only) past 800 characters or two line
       breaks — fewer on a screen under 13 rows — stands in the prompt as
       `[Pasted text #N +L lines]`, L its line breaks; Backspace after one takes it whole, and
       sending puts each back. A shorter one goes in as itself, tabs as four spaces.
   - **Keys and commands.** Typing, Left, Right, Home, End, Delete, Backspace, Ctrl-A, Ctrl-E,
     Ctrl-U; Ctrl or Alt with Left or Right a word at a time, back to a word's start and on to
     its end, as Claude Code's prompt goes; the prompt soft-wraps at words, the space a row breaks on hanging at its end so
     no row starts with one, as editors and Claude Code do, and a prompt taller than half the
     screen shows half the screen's rows, scrolled to keep the cursor in them, `↑` or `↓`
     leading a row with more past it; `\` then Enter is a newline, as in Claude Code; Up and Down move between the prompt's rows, and off its top or bottom row step through what
     was sent, keeping what was being typed: Down past the newest gives it back, as Claude Code
     does (read off its screen on 2026-11-02); PageUp and PageDown scroll ten lines and a wheel report one, since Termux
     sends a report for every line a finger travels, or three under Windows Terminal
     (`WT_SESSION` set, `TERMUX_VERSION` not), which sends one a notch; End or Esc goes back down. Scrolled
     back, the top moves from where it sat at the bottom, a line a step, and stays on what it shows
     while lines arrive under it; back at the bottom it follows them again; Esc interrupts a turn; Ctrl-C clears the prompt, and on an
     empty one goes home, as `/home` and Ctrl-D do. **Ctrl+\\ and `/disconnect`** go home from
     anywhere in a conversation, a dialog open or not, the session left running, as zmx's
     detach key does in `ssh dev`; Ctrl+\\ on the home quits the client. `/model`,
     `/effort [level]`, `/auto`, `/rename TITLE`, `/exit`; **`/copy [N]`** copies the Nth
     response back, 1 the latest, a response being a turn's text without its tool calls,
     and offers its code blocks one by one when it has any (OSC 52, and
     `termux-clipboard-set` under Termux); **`/btw QUESTION`** asks a side question, shown
     in its own panel until Space, Enter or Esc, and kept out of the conversation, as
     Claude Code's: its claude is the conversation forked unsaved, with no tools, no hooks
     and no MCP servers, on the conversation's model, in the slot's directory, given two
     minutes, and refused when the container has no room for another claude. Anything
     else, `/compact` and `/usage` included, is claude's. Claude echoes a command it runs
     itself in a `<command-name>` wrapper, after its reply: the view draws it as typed, its
     reply under it. A background task's `<task-notification>`, which claude hands the model
     as a prompt, is drawn as its summary, grey under `⎿`, not as its markup.
   - **Ctrl+O and `/expand`** switch the transcript between runs of tool calls summed up, as
     the mockups draw them, and every call on its own with the first lines of its result
     and how many more, as Claude Code's Ctrl+O transcript does; a failed call says `Error`.
   - **Subagents and workflows** are drawn on their own, as Claude Code draws them, not
     summed into a run of tool calls: `● Explore(what it was asked)` and under it its last
     two tool calls and how many more, then `Done (4 tool uses · 21.6k tokens · 7.6s)`; a
     workflow says its phase, how many of that phase's agents are done, and what the
     running ones are doing, then its totals. The dot is green once it ended well and red
     when it failed or was stopped, which the line under it says in words. A subagent's
     handback and structured output are counted, not listed, and its report is what `full`
     answers.
   - **A holder's live socket counts as `driver: holder`** (`holder::serving`) where the
     record's field is missing: the released hooks on this dev container rewrote it, and the
     fleet runs mixed releases for a while after a pin bump.
   - **Measured end to end on 2026-11-01** in a pty at 40×24, the relay run locally behind a
     stand-in ssh, a real claude 2.1.311: the home, `n`, a prompt streamed back with its
     `Worked for 2s · done 9:49 AM`, `/model` listing the real models with the current one
     ticked, `/copy`, `/exit` back home with `claude-1 ended`; and the relay killed under the
     client mid-conversation, redialled in a second, the conversation resumed with nothing
     drawn twice and the next prompt answered.
   - **Claude's markdown is drawn as Claude Code draws it** (`src/markdown.rs`), read from
     2.1.311's bundled renderer since no mockup draws it: headings without their `#`s (the
     first bold, italic and underlined), `**`/`*`/`~~` as styles, inline code and an
     unlabelled code block in lavender, `-` bullets and `1.`/`a.`/`i.` numbering by depth,
     quotes behind `▎`, and tables boxed in `┌─┬─┐` with the header centred and the width
     shared out by Claude Code's rule — a table whose row would wrap past four lines, or
     that still does not fit, drawn as `Header: value` lines instead, as Claude Code does on
     a narrow terminal. Not done: syntax highlighting in a labelled code block, which is
     drawn plain, and links as OSC 8 hyperlinks, drawn as `text (address)`.
   - **An answered question stays in the transcript** as Claude Code draws it, `● User
     answered Claude's questions:` and `⎿  · question → answer`, and its `AskUserQuestion`
     call is not counted among the tool calls. Live, the answers come from the view's
     answer as the holder logged it; on a resume, from the transcript's `toolUseResult`,
     since no request comes for a question settled before. Until the relay's hello the home
     is mockup 10's first frame, `⠋ connecting`, not an empty container.
   - **A resume shows what came before.** The launcher passes the conversation's transcript
     to the holder (`CLAUDE_SESSIONS_HISTORY`). Until 0.13.7 the holder copied its prompts and
     replies into its log as `_history` lines ahead of claude's own; now it serves them
     backwards on request (§ *Lazy history*), and the relay reads each stretch as turns
     already finished. Measured the same day: an archived conversation opened
     from the phone's home showed its two earlier turns and answered a third. Also measured:
     a live permission prompt drawn as R3 with Claude's own "don't ask again" rule, `1`
     running the command; and a running remote session left with Ctrl-C and opened again
     from the home, its history whole.
   - **Not yet like the mockups**, to close next: the logo's second line stops at the model,
     since claude's lines never name the plan; the transcript's line over a permission prompt
     is the request's own description (`Create empty file named x`, where R3 has Claude's
     `Creating…`); R4b's preview beside the list at 80 columns is drawn under it, as R4c;
     the `?` screen still lists the hub's keys. Not built: tool calls expanded (deferred by the
     owner).
   - **Still to measure on the phone**: cloudflared's idle timeout, Termux's OSC 52, and
     long-press selection with wheel reporting on.
