# Design

Everything here was either measured or decided by the owner. Where a thing was measured, the
date and the method are given, because a fact nobody has run is not a measurement however
long it has sat in a comment.

## Why not just `claude --resume`

The first question anyone should ask of this tool, and it deserves a direct answer: Claude
Code already lists past conversations and reopens one, and
`$CLAUDE_CONFIG_DIR/sessions/<pid>.json` already records the live ones. So what is left?

**Three things, and each of them is a way to lose work rather than a convenience.**

1. **`--resume` does not know what is running.** It lists conversations *on disk*. Pick one
   that is already live in another process and you get a second process on the same
   conversation, which forks it — two transcripts, diverging, and no warning. The first job of
   a menu here is to know that a row is live and **attach** instead of resuming, and that is
   the one thing neither `--resume` nor the sessions file will tell you.
2. **What records a live session disappears exactly when it matters.**
   `sessions/<pid>.json` is pid-keyed and exists only while the process does. Offload a slot
   and its file is gone — and an offloaded slot is precisely when you need to know which
   conversation belonged to it, in which directory, to bring it back.
3. **Nothing anywhere records attention or absence.** That a permission prompt is waiting.
   That a timer is pending, so the slot must not be stopped. And **when you last looked** —
   which is the whole of `unread`, and cannot be derived from anything Claude Code keeps,
   because it is a fact about the owner rather than about the session.

**A row's title is what Claude Code's own session selector shows**: the conversation's
custom title (the owner's own name for it), else the title Claude Code generates, else the
first prompt. Both titles are read from the end of the conversation's transcript — its
`custom-title` and `ai-title` entries — by the hook, on `SessionStart` and `Stop`, before it
takes the slot's lock (`src/transcript.rs`). Until 0.3.1 the menu preferred the `name` in
Claude Code's live sessions file, which on the boxes showed one of Claude's long replies in
place of a title (owner, 2026-10-03); that file is no longer read for titles at all.

## Slots, conversations, and what is stored

A **slot** is one `zmx` session named `claude-<n>`, i.e. one running `claude` process. A
slot holds a sequence of **conversations** (Claude session ids): `/clear` starts a new one in
the same process, `--resume` re-enters an old one.

The registry is keyed by slot and records:

`slot` · `pid` + process start time (so a reused pid is never mistaken for the original) ·
`session_id` (current) · `cwd` · `title` (custom) · `ai_title` · `first_prompt` · `state` · `last_activity` · `last_attach` ·
`needs_you` · `timers` (each with its due time, and whether it recurs)

**States:** `attached` / `detached` — read from `zmx list`, never stored — plus `offloaded` and
`closed`. **Unread is derived, not stored:** a `Stop` later than `last_attach` means the
session finished something while you were away.

**Storage:** `~/.local/share/claude-sessions/`, one JSON file per slot, written
tmp-then-rename under a per-slot `flock` taken **with a timeout**. `~/.local/share` is a
persisted volume in every dev container, and keeping it out of `$CLAUDE_CONFIG_DIR` leaves
Claude's directory Claude's.

**The same per-slot lock is held by whoever acts on that slot** — the menu attaching or
resuming, the offloader from decision through kill, `close`. That is what closes the
decide-then-signal race, and what makes "never resume a conversation already running" true
across two simultaneous ssh logins rather than merely likely.

## Binding a hook to a slot

The menu starts each slot as:

```
zmx attach claude-<n> env CLAUDE_SESSIONS_SLOT=claude-<n> \
  CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 CLAUDE_CODE_DISABLE_MOUSE=1 \
  CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL=1 \
  sh -c 'e=$1; shift; exec "$@" 2>>"$e"' sh <registry>/claude-<n>.stderr claude …
```

so every hook inherits the name. The `sh -c` captures claude's stderr to a file the
failed-resume dialog reads (mockup 5), and **its `exec` is load-bearing**: it replaces the
shell with claude, so claude is still the zmx daemon's direct child, which is the binding
test below. Without it the shell stays in between and no hook ever binds — `tests/launch.rs`
runs this exact line under the real hook to pin that. The capture path is an argument, not an
environment variable, so nothing of ours leaks into claude's environment. **A hook binds to the slot only when its `claude` is the
direct child of that slot's `zmx` daemon**, checked in `/proc`. A nested claude — a
`claude -p` from a Bash tool call, or a subagent — inherits the variable too and must count
as *work running under* the slot, never rebind its `session_id`. A hook fired in a subagent's
own context — a tool call it makes — carries `agent_id`, which is a cheaper test than the
`/proc` walk for that case; a `claude -p` spawned from a shell still needs the walk. **Except
`SubagentStart` and `SubagentStop`:** the slot's own claude fires them, and there `agent_id`
names the agent the event is about, so they are bound by the walk like any other (issue #10).

**The three `CLAUDE_CODE_DISABLE_*` give claude the terminal's own scrollback** (owner,
2026-10-04, issue #8), for a new slot and a resume alike. Every slot is reached over ssh,
mostly from a phone, and Claude Code's default renderer owns scrolling itself: the alternate
screen with a virtualised scrollback makes each swipe a round trip asking the server to
repaint; mouse tracking, on in every renderer, makes Termux send swipes to claude instead of
scrolling its own buffer; and the inline renderer keeps older output in its viewport unless
virtual scroll is off too. With all three, output lands in the terminal's buffer, scrolling
is local and instant, and long-press selection works. Only the first has a settings key
(`tui`), so the launcher — which makes the process — is where the mode is chosen. **Each is
set only if the login has not set it**, so a value it brings (ssh `SendEnv`) wins; the
launcher leaves such a variable off the line and claude inherits it. **Only an empty value
turns a feature back on**: Claude Code tests all three for truthiness, so `=0` disables as
surely as `=1` (infra's review of 0.3.8 read it in the binary), and a login wanting the
fullscreen renderer back must bring `NAME=`, not `NAME=0`. These are the one deliberate exception to nothing of ours in claude's
environment: they are meant for claude.

The costs, accepted: the classic renderer flickers more and leaves debris after a resize
(Termux resizes on every keyboard show and hide); what needs the fullscreen renderer (focus view, the diff
panel) refuses and says so; and menus, links and collapsible blocks are keyboard-only. It
applies to every login, a laptop's too, because it is about claude over ssh rather than about
a device; if that proves wrong, the follow-up is a toggle key, not a revert.

**Re-check the names at each Claude Code pin bump.** They were read from the 2.1.289 binary,
and a rename fails silently — passed, never read, and scrolling is back to round trips with
no message:

```sh
strings "$(command -v claude)" | grep -aowE 'CLAUDE_CODE_DISABLE_(ALTERNATE_SCREEN|MOUSE|VIRTUAL_SCROLL)' | sort -u
```

Expect all three names. **`-w` is load-bearing**: the binary also holds
`CLAUDE_CODE_DISABLE_MOUSE_CLICKS`, which an unanchored match counts as `…_MOUSE`, so a rename
of `…_MOUSE` alone would still read three of three (infra, 2026-10-04). infra runs this as
`make verify`'s `scrollvars` line against the claude a container actually has, since Claude Code
updates itself in place.

**Open: `/tui` inside a slot.** Claude Code's `/tui` switch relaunches claude with
`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` dropped and the other two kept, so `/tui fullscreen`
puts that one session back on the alternate screen — where, with mouse reporting still off,
a swipe is arrow keys again. Whether the relaunched process is still the zmx daemon's direct
child, so the slot stays bound to it, is not yet known.

**They only work because the holder keeps off the alternate screen.** v0.3.8 shipped them
under abduco, whose client switches the terminal to the alternate screen on every attach; there,
with mouse reporting off, Termux and Windows Terminal turn a swipe or the wheel into Up/Down
keys, so scrolling became prompt history, and the terminal's own buffer was out of reach
anyway. Every test passed, against a stand-in holder. That is why zmx replaced abduco
(*zmx, measured*), and why `tests/zmx_real.rs` drives the real zmx.

Sessions started without the menu (see *Unregistered sessions*) are found from `ZMX_SESSION`,
which zmx sets inside every session, when a `zmx` daemon is above the hook.

## The event table

`claude-sessions hook` reads the event's JSON on stdin. **It always exits 0** — see
`CLAUDE.md` — even if it panics: the panic is caught and written to `hook.log`, never to the
terminal.

| event | registry effect |
|---|---|
| `SessionStart` (`startup` / `resume` / `clear` / `compact` / `fork`) | bind `session_id`, `cwd`, pid + start time; state live. A **different** `session_id` also drops `title`, `ai_title`, `first_prompt` and the per-event times, so a new conversation never wears the old one's name or reads as prompted by the old one's prompt; then the titles are read from the transcript at `transcript_path`, as on every `Stop`. Every source but `compact` leaves claude at its prompt: not busy, `needs_you` cleared, `ready_ms = now`; `compact` changes none of those |
| `UserPromptSubmit` | `last_activity = now`, busy, clear `needs_you`; the first one of a conversation sets `first_prompt` — one line, at most 120 characters, the title of last resort |
| `Stop` | `last_activity = now`, idle since now; **`background` = the payload's `background_tasks`**, replacing what was there (issue #9) |
| `SubagentStart` | `subagent: <agent_type>` joins `background` unless listed (issue #10); **not** activity |
| `SubagentStop` | **`background` = the payload's `background_tasks`**, as on `Stop`, but one with no list is no news, not an empty list (#12); **not** activity |
| `Notification`, type `permission_prompt` / `elicitation_dialog` / `agent_needs_input` | `needs_you` — never offloaded while set |
| `Notification`, type `idle_prompt` | **nothing.** It fires about a minute after every `Stop` nobody answers; treating it as `needs_you` would make every detached session permanent |
| `PostToolUse` on `ScheduleWakeup` / `CronCreate` / `CronDelete` | add or remove a timer, with its due time |
| `SessionEnd`, reason `clear` **or `resume`** | nothing — a `SessionStart` follows in the same process. The hook takes no lock for it, so it cannot race that `SessionStart`; a lock failure in `hook.log` names its event and reason |
| `SessionEnd`, any other reason | `closed`, unless the slot is marked `offloading`, in which case `offloaded` |

Three details that cost something if missed, read from the vendor hook documentation on
2026-10-01:

- `SessionStart.source` has five values including **`fork`**, and carries
  `seconds_since_last_response` on a resume — a better idle clock than anything computed here.
- `SessionEnd.reason` has six including **`resume`**. Treating `resume` as an end marks a slot
  closed every time a conversation is resumed.
- `CronList` and `TaskStop` are **not** timers and must not pin a slot.

## The offloader

Offloadable when: detached · `Stop`, or a start at the prompt, is the latest activity
(`SubagentStart`/`SubagentStop` are not activity) · no
`needs_you` · **no pending timer** (owner, 2026-10-01: never, whoever set it) · **no
background work listed** (`Stop`'s list, kept between `Stop`s by `SubagentStart`/`SubagentStop`) · no non-`claude` descendants · idle past the threshold,
counted from the later of that event and **the last write to the conversation's transcript or
any of its subagents'**.

**In-process background work is invisible to the process table** (issue #9, found by infra's
reviewers and reproduced on v0.4.0). A background subagent, a Workflow run, a teammate or a
cloud session runs inside claude's own process, so there is no descendant to see, and after the
parent's `Stop` nothing moved the record: ten minutes later a live pass would have killed four
working agents. Three things close it:

- **`Stop` says what is still running.** Its payload carries `background_tasks` — Claude Code's
  task registry filtered to backgrounded work that is running or pending, each with a `type`
  in words (`subagent`, `workflow`, `shell`, `monitor`, `teammate`, `cloud session`, …) and a
  description. Read in the 2.1.291 binary and then seen on the wire: a turn that left
  `sleep 45` running in the background sent `{"type":"shell","status":"running",
  "description":"Sleep for 45 seconds",…}`, and a quiet turn sent `[]`. Recorded as
  `background`, it keeps the slot for as long as it is not empty. A task finishing wakes claude
  to handle its notification, and that turn ends in a `Stop` with the list as it now is — so
  the hold releases itself, and the ten minutes start from that `Stop`. A `SessionStart` from a
  different process clears the list, since a new process cannot be running the old one's work.
  **Claude Code's own housekeeping is left out** — `dream` (auto-dream, which tidies its memory
  files after a turn), `auto-mode scan` and `memory import`: each ends "ambient", waking no turn
  and writing no transcript (read in the binary), so no later list would ever drop one, and the
  auto-dream fork's `SubagentStop` lists its own task as running. The cost is that one still
  running is invisible: a dream that outlasts the idle threshold can be stopped partway, and
  Claude Code's lock and abort handling recover it. **So is its watch on an artifact it
  published** (owner, 2026-10-06) — the live-updates socket and its presence companion,
  labelled `monitor` but told apart by their fixed descriptions (`live updates for artifact …`,
  `presence on artifact …`): listeners, not work, as Claude Code's own keep-alive test agrees. An idle one is retired after 3.5 hours,
  ambient again, so it would hold a slot for hours and then for good; and whoever wants the
  replies to a comment is attached, which already keeps the slot.
- **Between `Stop`s, agents announce themselves.** `SubagentStart` adds the agent it announces
  to the list, and **`SubagentStop` takes its payload's `background_tasks` whole, as `Stop`
  does** — except that one with no list is no news, where `Stop` reads none (#12): Claude
  Code builds both from the same task registry. This is what holds work started in a turn
  the owner ended with Esc (issue
  #10): an Esc fires no `Stop`, so the list would otherwise be the previous turn's, and an agent
  launched in the interrupted turn would hold the slot only while it kept writing. Seen on
  2.1.291 under a pty on 2026-10-06: a background agent launched, the owner pressed Esc, and
  the agent ran on for forty seconds, sent its `SubagentStop` — whose list still named it — and
  woke claude for a turn that fired `UserPromptSubmit` and ended in `Stop` with an empty list.
  So a background agent is held until the turn its end wakes, which is also what keeps one
  that another installed hook sends back to work after its `SubagentStop`. The registry's list
  never names a foreground agent (read in the binary: only backgrounded tasks pass its filter),
  and a foreground agent the Esc cut off sends no `SubagentStop` of its own (issue #11) — so
  it is listed from its `SubagentStart` until the next `SubagentStop` or `Stop` says otherwise:
  the keep direction, and a turn still running holds the slot regardless. The other side of
  the same rule: a foreground agent whose entry an earlier `SubagentStop` already replaced, then
  sent to the background with no hook (Ctrl+B, or Claude Code's auto-background), then left by
  an Esc, is held only by its writes until a later `SubagentStop` lists it — as in 0.4.3. Both events come
  from the slot's own claude, so they are bound by the `/proc` walk, not by their `agent_id`
  (which there names the agent, not the context the hook fired in); a nested `claude -p`'s
  agents stay activity only. **Neither event is activity**: they edit the list and nothing
  else. Claude Code also sends `SubagentStop`, with an empty `agent_type`, for internal agents
  it never announced — seen about thirty seconds into both runs, after a `Stop` once and after
  the Esc once, carrying the list — and as activity one would make an idle slot read busy until
  the owner's next turn. An agent's work is seen as its transcript's writes (below).
- **Writes are activity.** A turn writes its transcript as it goes, and a background subagent
  writes its own under `<conversation>/subagents/` (Workflow runs a level or two deeper). The
  offloader takes the newest of those modification times as one more "last thing that
  happened". That covers a turn no hook announces, should there be one — a finished task's
  notification was seen to fire `UserPromptSubmit`, a fired `ScheduleWakeup` has not been
  watched — and a subagent that is working between the parent's `Stop`s. It errs one way only: a write for some other reason
  delays an offload, never causes one.

**An Esc fires no hook at all, and the transcript says so instead.** Seen on 2.1.291 under a
pty with every hook logging (2026-10-06): Esc mid-reply and Esc mid-tool each left
`UserPromptSubmit` as the last event — no `Stop`, no `StopFailure`, no `PostToolUse` for the
interrupted tool — so the record read `busy` (and, if the Esc dismissed a permission prompt,
waiting for you) until the next turn ended: the slot was never offloaded and the menu drew it
under Working. What the interrupt does write is a `user` entry whose text begins
`[Request interrupted by user` — `]` mid-reply, ` for tool use]` mid-tool — timestamped at the
Esc, after the cut-off reply or the tool's rejected result, with only bookkeeping after it.
So when the hooks left a slot busy or waiting and its transcript's last conversational entry is
that marker, newer than the latest activity the hooks recorded, **the turn ended at the marker**: the
offloader counts idleness from it as it would from a `Stop`, and the menu draws the row under
Idle. The two read one rule (`SlotRecord::esc_ended`). The offloader still holds the slot for
anything `background` lists, as it does for an agent the interrupted turn started; the menu
does not read the list, so that row is Idle either way.
Only the tail is read, and only for such a slot; a last line that cannot be read whole — a
reply longer than the tail — is no answer, never an older marker's. **Only the marker itself
counts** (issue #10): a list of exactly one text part that says exactly
`[Request interrupted by user]` or `[Request interrupted by user for tool use]`, as Claude Code
writes it and as its own checks read it. A prompt the owner types arrives as a string, so one that begins with the phrase is
a prompt; subagent (`isSidechain`) lines are skipped. An Esc also clears waiting-for-you
without knowing whose question it was: the prompt on screen is the likely one, and a question
from a background agent would be the cost — infra's reviewers and this tool agree on clearing.

**Still open:** a turn started by something that neither writes nor hooks has not been found,
and would not be seen.

**The threshold is 10 minutes** after `Stop` (owner, 2026-10-01). The hour floor the old
shell script used existed only because self-scheduled wake-ups were invisible; the timer
records make them visible, so the floor goes.

It holds the slot lock from decision through kill, marks the slot `offloading` before
signalling and `offloaded` after, and keeps `TERM` → grace → `KILL` → zmx teardown with
the pid-plus-start-time check at each step.

**A slot with no conversation on disk is closed, not offloaded** (issue #5). A slot opened
and never spoken to has a `session_id` and no transcript: Claude Code writes a new session's
file at its first prompt, and `claude --resume` on it exits at once. A slot `/clear`ed and
left has a file — `/clear` writes the new conversation's at once (infra, 2026-10-03), holding
only its bookkeeping and the command's own local entries — but nothing in it to come back
to. So "a conversation on disk" means a transcript with an exchange in it: a reply from
Claude, or a prompt the owner typed (`transcript::has_exchange`). Such a slot is stopped by the same rules and
the same path, but marked `closed`: an `offloaded` row would promise a resume that cannot
happen.

**Low memory is the container's, not the host's.** `/proc/meminfo` inside a container reports
the whole machine; the ceiling is the cgroup. Read `/sys/fs/cgroup/memory.max`,
`memory.current` and `memory.stat` — readable unprivileged on cgroup v2, verified 2026-10-01 —
and count as used `memory.current` less `inactive_file` (claude-sessions#7; `src/mem.rs` says
why the inactive list only). There is no host fallback: `MemAvailable` once stood in for an
unreadable `memory.current`, which against a container limit read the host's free memory as
use. With no limit, or nothing readable, room is unknown, and unknown never refuses.

**The orphan sweep** collects `daemon run --origin transient` trees whose spawning pid and
start time are gone, with their `bg-pty-host` / `bg-spare` children. It shipped logging
only, and was **armed on 2026-10-03** on the owner's word — and on a fact that makes it safe:
**this tool runs with agent view disabled**. Agent view's supervisor is meant to outlive the
session that started it (it keeps background sessions running after the terminal closes), so
on a box with agent view on the sweep's rule would pick out working supervisors; with it off
there is no legitimate one, and a transient daemon left behind is a leak. `--dry-run` still
only logs what it would kill.

### How `claude-sessions offload` reads those rules

Built 2026-10-01 in `src/offload.rs`. One pass per invocation, run from the box's timer;
`--dry-run` decides and reports without signalling. Where the rules above left a choice, this
is the choice and why:

- **"`Stop` is the latest activity"** is the later of `last_stop_ms` and `ready_ms` being at or
  after `last_activity_ms`, and not `busy`. `ready_ms` is the last `SessionStart` that opened a
  conversation at its prompt — `startup`, `resume`, `clear` or `fork`, which the vendor docs
  describe as "you can type right away" — so a slot resumed or started and then left is
  offloadable ten minutes later like any other (owner, 2026-10-03, after asking for this to be
  checked). A `compact` start is never readiness, and leaves `busy` and `needs_you` alone:
  auto-compaction can come in the middle of a turn, and the docs do not say it cannot.
- **Resumable or kept.** A slot with no recorded `session_id` or `cwd` is kept: stopping it
  would be a close with extra steps. Registered and unregistered slots get the same rules, so
  an unregistered slot whose hooks did record those is offloadable and comes back as a
  registered one.
- **Offload or close is decided after stop or keep.** `decide` answers whether a slot may be
  stopped; `judge` then makes it a close when the record's transcript — `transcript_path`, or
  for an older record the path derived under `CLAUDE_CONFIG_DIR` — holds no exchange, or is
  not there at all. Every
  reason to keep holds a close exactly as it holds an offload. A slot with no `session_id`
  is still kept rather than closed: there the hooks never bound and nothing is known, while
  a recorded id with no transcript is evidence there is nothing to lose. `--dry-run` prints
  `would close` for it, in the shape of `would offload`.
- **A close writes nothing before the signal.** The `SessionEnd` it provokes already reads as
  a close, and a stop that fails leaves the slot `live` for the next pass, where an offload
  leaves it `offloading`. Afterwards the record is `closed`, the line in `offload.log` says
  `closed … no conversation on disk to resume`, and the pass's summary counts it apart.
- **The menu's make-room path asks the same.** Mockup 4 offers only a slot `judge` would
  offload, because the dialog promises it is resumable from disk; accepted, the stop is
  `judge`'s again under the lock, so a transcript that went in between makes it a close.
- **Not being able to look keeps it.** A zmx that does not answer for the slot (attached cannot
  be ruled out), a
  `/proc` that cannot be listed (descendants cannot be ruled out), a record with no pid — each
  is a reason to keep, because the offloader needs evidence to act, never to hold off.
- **"No non-`claude` descendants"** walks the whole tree under the slot's claude, through any
  nested claude, and ignores zombies. `claude.exe` counts as claude too — Claude Code's helper
  processes have carried that name, and infra's old offloader measured and exempted them;
  counted as work they would hold every slot forever. `node` is work (issue #2). A stdio MCP server would be such a descendant and would
  pin its slot for good; the owner runs none (2026-10-01), so the rule stands as written. If
  one is ever added, this is the line that has to learn about it — `offload --dry-run` names
  what is holding each slot.
- **Memory is reported, not a gate** (owner, 2026-10-01: no need to gate on it). The pass
  prints the cgroup headroom and stops whatever is idle regardless.
- **Signals go through a pidfd**, opened before the start-time check, so a pid reused between
  the check and the signal cannot be hit. `TERM`, 5 s, `KILL`, 3 s; a process still there is
  reported and the slot left `offloading` for the next pass to decide again. Then, if the
  claude's parent really was a `zmx` daemon, zmx is given up to 2 s to drop the session —
  it does so the moment its program exits; its daemon lingers about 2.4 s more and exits by
  itself, which nothing waits for.
- **The lock is taken only by a slot about to be stopped** (issue #1). The pass reads `/proc`
  once, holding no lock, and decides every slot from that and its record as listed; a slot
  that is kept — nearly every slot, nearly always — never touches its lock, and `--dry-run`
  takes none at all. A candidate then takes its lock, re-reads its record and `/proc`, and
  decides again before anything is signalled. Hook events other than `SessionEnd` wait up to
  2 s for a slot's lock (inside their 5 s timeout) rather than `SessionEnd`'s 400 ms, because a
  dropped `UserPromptSubmit` leaves a working claude reading as idle.
- **The kill's own `SessionEnd` hook cannot write.** The offloader holds the slot lock from
  decision through kill, so that hook waits its 400 ms, gives up and logs it, and the
  offloader writes `offloaded` (or `closed`) itself. A `hook.log` line per offload is expected.
- **The sweep infers "spawner gone" from the parent**: a transient daemon whose parent is no
  longer a `claude` has been reparented away from the session that started it — sound only
  with agent view disabled, as above. **A daemon under 10 minutes old is left alone**, as is
  one whose age cannot be read: a tree caught between its spawner exiting and its own exit is
  not a leak yet. The kill is `TERM` to the whole tree, deepest first, the offloader's grace,
  then `KILL` to what is left, every signal checked against pid and start time. **A dry run
  applies the same age check**, printing `would keep, too young` where the live sweep keeps
  and `WOULD KILL` only where it would kill, so the dry run reads as the live sweep would act.
- **Logged to `offload.log`** beside the registry: every stop, every failed stop, and every
  sweep kill, kept-too-young tree and would-be kill. Slots kept are printed to stdout only, since a pass every few minutes
  would otherwise bury the lines that matter.

## Activity, measured (0.4.5: reported, not acted on)

**Why.** Every rule above reads Claude Code: hook payloads, its task list's filters, which
agents announce themselves. Eight review rounds of #10 kept finding corners of that reading,
and a self-update can move any of them without a word; when one moves, the cost is a working
claude stopped. The owner's direction (2026-10-06): try a rule that asks the kernel instead —
what a slot's processes *do* means the same on every Claude Code version and every machine —
with **freezing** (`SIGSTOP`, a reversible pause that leaves a cold process for swap to take)
in place of killing. 0.4.5 is the first step: every pass, live or dry, says what that rule
would do beside what the offloader did, and nothing else changes. The freeze comes after the
fleet's own numbers have been read. `src/activity.rs` has the details and the tests.

- **Bytes, not CPU.** A model turn is a stream from the API and claude writes its transcript
  as it goes, so work is bytes through `read`/`write` — `rchar + wchar` in `/proc/<pid>/io`,
  the same count on a Pi 4 as on x86. CPU time is not device-invariant: the same work costs a
  slow CPU more of it. Wake-ups (voluntary context switches, summed over threads) are logged
  beside the bytes as a second opinion. Measured on 2.1.291 with agent view off, here, under
  a real `offload --dry-run`: idle 95–139 B/s and 21–25 wakeups/s; streaming a reply
  9,342 B/s and 47 wakeups/s. Bytes part idle from work by about 70×, wake-ups by 2×.
- **Which processes: the environment.** With agent view off, claude is one process: an
  in-process subagent shows as claude's own bytes, and its tools as claude's children
  (measured: a background agent's `sleep` appeared as `bash` → `sleep` under claude, nothing
  outside its tree). Every process a slot's claude starts inherits `CLAUDE_SESSIONS_SLOT`,
  including one that double-forks away to init, so `/proc/*/environ` finds them however they
  were reparented; the recorded claude and its descendants are added in case one cleared it.
  The recorded claude counts only while it is the process recorded, by start time. Not
  `claude-sessions` itself — a hook, the status line, or a pass run from a slot's shell
  carries the variable and descends from claude, and would count its own reading of `/proc`.
  A reaped child's whole-lifetime counters are folded into its parent's by the kernel, so a
  tool that ran and exited between passes is still counted — and one that lived across a pass
  is counted twice in the window it exits, as is a reaped hook: both err towards active.
  Agent view, which runs a service outside any slot, stays off.
- **The line: each slot's own floor.** A slot's floor is its quietest window in the last
  24 hours, hour by hour; a window counts as active above ten times the floor learned
  *before* it, held between 512 B/s and 4 KB/s, and a slot with no floor yet is held to
  512. A Claude Code that idles noisier raises its own floor; the base stops a near-silent
  floor from making a stray read look like a turn. All bytes, so no device enters. **Its
  blind spot:** a slot only ever seen busy learns that work as its floor, and then the
  4 KB/s cap is its only protection — measured streaming is 9,342 B/s, about 2.3× the cap,
  so work that averages under 4 KB/s from a slot's first windows can read as quiet. The
  logs show the floor beside every rate, which is how to spot it.
- **The verdict.** Quiet — under the line in every window — for the offloader's 10 minutes,
  and detached by zmx's count: *would freeze*. Otherwise *would keep*, with the reason:
  first reading, attached, attachment unknown, or quiet under 10m. Each pass prints
  `claude-1: measured — 95 B/s over 180s, 25.2 wakeups/s, 1 process(es), line 945 B/s from
  floor 95, quiet 14m; the activity rule would freeze it`. Wake-ups read `?` in a window
  where a thread exited, since its count leaves the sum.
- **What is unknown counts as active**: a slot's first reading, a process not seen last time
  (its whole history falls in the window), a process that is there but cannot be read (the
  slot is reported as unknown and kept), and a window as long as the quiet period, which
  cannot say when in it the bytes fell. A window under a minute — a pass run by hand right
  after the timer's — is left for the next, with a line saying so.
- **State** is `activity.state` beside the registry — not `.json`, which the registry reads
  as a slot — written whole and renamed into place. Two passes at once may each write it; the
  later wins and the other's window is measured again.
- **What it cannot see**: a claude waiting in process, silently. **The common case is its
  own timer** — a `ScheduleWakeup` or a cron a claude set itself, routine on this fleet and
  the reason the offloader never stops a slot with one pending (owner, 2026-10-01). The
  activity verdict has no input for timers, so such a slot will read *would freeze* beside
  the offloader's *kept — a timer is pending*; a freeze would stop the timer firing, and
  thawing on attach is no substitute for a wake-up nobody is there to see. The freeze
  decision has to keep that rule. Rarer: a tool blocked on a remote that sends nothing,
  which a freeze pauses and an attach resumes.

**Before it acts, the owner decides:** freezing replaces the 10-minute kill rule (owner,
2026-10-01), and a frozen row needs a word and a place the approved mockups do not have.

## The status line

`claude-sessions statusline` prints `RAM: 1.0G / 3.0G used, Load: 2.3, or3-dev` (owner,
2026-10-06) for Claude Code's status line: the menu's working-set figure against the
container's limit — or the host's total and available memory where there is no limit — the
one-minute load, and the hostname, which names the dev container. Anything unreadable is a
`?`. `hooks-config` installs it as `statusLine` in the same drop-in as the hooks; a managed
setting outranks a user's own, so it replaces any status line set per user. It reads a few
small files and nothing else, because Claude Code runs it often. The session JSON Claude
Code writes on its stdin is drained, but for at most a moment's quiet, so a pipe whose writer
stays open cannot hold it. The owner's example is 41 columns: on a 40-column phone Claude
Code truncates it from the right, losing the hostname first.

## The menu

- **Lists every live and offloaded slot, every unregistered `zmx` session, and every
  conversation on disk started in the workspace that is not running**, **grouped by state**
  (owner, 2026-10-03): `Needs you`, `Working`, `Idle`, `Offloaded`, `Closed`, `Archived`, an
  empty group not drawn, one blank line between groups. **A heading is a labelled rule with a
  count**, `── Closed · 7 ────` (owner, 2026-10-03, chosen from three drawn options): the rule
  dim and the name bold, the whole Needs-you line amber. A rule is a glyph, so it divides the
  list on a monochrome terminal too, and the count says how many sit behind `… N more`. **From
  60 columns up the rows are indented two columns** under their headings; at 40 the two
  columns stay with the titles.
  A row's group is the first that fits: closed, offloaded, a prompt waiting, mid-turn, else
  idle; an unregistered session, which nothing describes, is idle. Within a group, most recent
  activity first, except Idle, where unread rows come first. A row is its title and its age —
  **no marks and no numbers**: an unread title is bold, a session attached somewhere else is
  drawn dim, and amber is the Needs-you heading. Closed rows are not counted as open in the
  header.
- **The archive** (owner, 2026-10-03) hides a closed conversation in the menu and changes
  nothing of Claude Code's: `claude --resume` and `/resume` still find it. It is one small file
  per conversation id in `archive/` beside the registry (`src/archive.rs`), saying `archived`
  or `kept` and when. A conversation is archived when it has gone 30 days unused, worked out
  as the list is read, or when `c` on its Closed row put it there — **with no question
  asked**, since nothing is lost — and it has not been used since: one resumed by hand after
  it was archived is in use again. `c` on an archived row takes it out, and `Enter` on one
  resumes it and does the same; either way it is marked kept, which gives it 30 days from
  then before age archives it again, so an old one does not fold straight back. **The
  Archived group is shut whenever the menu starts**, and again whenever it empties: one
  heading, `── Archived · 12 ──`, and `Enter` on it opens or shuts it.
- **The Closed group is read from Claude Code's own store**, `$CLAUDE_CONFIG_DIR/projects`
  (owner, 2026-10-03; `src/store.rs`), not from the registry: every conversation on disk,
  whoever started it — a slot of ours, a `claude` run by hand, the old `ssh` path, a
  conversation a slot held before a `/clear`. Until then Closed listed only slots of ours, and
  only the conversation each held last, so a conversation from before a `/clear`, or from
  anywhere but the door, could be reached only with `claude --resume`. **Only conversations
  started in the workspace** are listed (owner, 2026-10-03), as Claude Code's `/resume` lists
  one directory's; a conversation's start directory is the first `cwd` in its transcript whose
  name is the directory the file sits in. Not listed: one that is running — Claude Code's
  live-session files say which, and a slot's current conversation is that slot's row — and
  one with no exchange in it (*The offloader*). One conversation can be in two project
  directories after its `cwd` changed; the newest copy wins. A row's age is its last entry's
  own `timestamp`, never the file's modification time, which `/clear` was seen to touch on
  other conversations' files. Files are re-read only when their size or modification time
  changes, so the two-second poll stays cheap.
- **`Enter` on a closed row resumes it in a slot**, `claude --resume <id>` under `zmx` in
  its start directory. If a slot's record already names the conversation, that slot resumes
  it — both ways in then take one slot lock; otherwise a new slot is allocated, the never-
  resume-running check made under the allocation lock, and the record, naming the
  conversation, written before the lock goes, so a second menu finds it starting.
- **An offloaded slot is listed only with a conversation on disk** — its `transcript_path`,
  or for a record older than 0.3.3 the path Claude Code keeps it at under `CLAUDE_CONFIG_DIR`
  — with an exchange in it. A slot offloaded before its first prompt, or `/clear`ed and left,
  has nothing to resume (issue #5), and the resume path refuses such a slot too, for one whose
  transcript went after the list was read.
- **Opening a row:** live → `zmx attach <slot> false`; offloaded → start a new slot running
  `claude --resume <session_id>` in its `cwd`. The `false` is a guard: `zmx attach` *creates*
  a session that is not there, so one that ended between the list and the keypress would come
  back as a stray shell; given a command, zmx ignores it for a session that exists and runs
  it for one that does not, where `false` ends at once and leaves nothing. `zmx` runs as a
  **child**, so on detach the
  menu comes back rather than the login ending — a fresh login costs a Cloudflare Access
  handshake on a metered link. Coming back from an attach is what writes `last_attach_ms`,
  the only writer it has, so that is what clears `unread`.

### How the menu acts, where the rules above left a choice

Built 2026-10-02 in `src/launch.rs`; each is tested there with stand-in `zmx` and `claude`,
and the start, attach and close paths again against the real zmx in `tests/zmx_real.rs`.

- **A resume keeps the slot's name.** The record is reused — same title — and marked
  registered, so an unregistered slot comes back as one of ours.
- **Deciding happens under the slot's lock; running happens after it.** Under the lock the
  record is re-read, the conversation is checked not to be running anywhere — Claude Code's
  own live-session files and every other record — and the record is marked live with its pid
  cleared before the lock is let go. A second menu then sees a slot that is starting, not one
  it may resume. If the conversation *is* running in another zmx slot, that slot is
  attached instead; anywhere else, the resume is refused.
- **A live record with no pid** is a slot starting elsewhere: attached if zmx lists it,
  refused for 30 s otherwise, and after that treated as a start that died and is resumable —
  so a menu killed mid-start cannot strand a slot.
- **A failed resume** is a start that ends within 10 s without a `SessionStart` binding it: the
  record goes back to offloaded or closed, whichever it was, and mockup 5 shows the last lines
  of the captured stderr, control characters removed — or says it wrote nothing to stderr,
  rather than leaving a gap that reads as a lost reason. **zmx does not report its program's
  exit status** — `zmx attach` returns 0 for a detach and for a program that ended alike, and 1
  only when the program was gone before the client connected — so mockup 5 says `ended`
  (owner, 2026-10-06), or `was killed for memory` when the container's OOM-kill count
  (`memory.events`) rose during the start: the one thing the number used to tell. A start into a `cwd` that no
  longer exists is refused before it runs, and a zmx session already holding the slot's name
  with nothing recorded in it is refused, naming `zmx kill`: zmx would attach to it and
  ignore the resume.
- **New slots take the lowest free `claude-<n>`** under a registry-wide lock with a timeout,
  writing the record before the lock goes. A closed slot's name is reused: closed slots are
  not listed, their conversations are, from the store, under their own ids. A new slot that
  dies before binding is marked closed and reported, since it has no row yet.
- **Room is what the working set leaves** (claude-sessions#7): `memory.current` less the
  cgroup's `inactive_file`, the page cache the kernel reclaims first. Raw `memory.current`
  counted cache as use — an interrupted self-update's ~650 MB once read as a session's worth of
  RAM with nothing running — and the menu would have offered to offload a session for room the
  kernel would simply have taken back. The header, mockup 4's figures, `doctor` and the
  offloader's `memory:` line all use it, and the last two say the cache apart.
- **Room is checked once.** After offloading to make room, the open goes ahead without asking
  the cgroup again: what the stopped claude read stays charged as active page cache for a
  while, which the working set still counts, and a second check could refuse the room just
  made.
- **`c` on a live slot stops it** with the offloader's own `TERM` → `KILL` path and zmx
  teardown, pid and start time checked at each step, then marks it closed. A row with no record
  at all is refused: there is nothing to identify its process by safely.
- **The programs are overridable** for tests and odd installs: `CLAUDE_SESSIONS_ZMX` and
  `CLAUDE_SESSIONS_CLAUDE` name `zmx` and `claude`; `CLAUDE_SESSIONS_WORKSPACE` names where
  `n` and `s` start (default `/workspace` where it exists, else home).
- **Ctrl-C quits the menu**, as `q` and `Esc` do; raw mode delivers it as a byte, and a menu
  that ignored it would read as hung. While a child has the terminal, the menu itself ignores
  SIGINT and SIGQUIT — by a handler, not `SIG_IGN`, so the child still gets the default.
- **A status line that does not fit wraps** rather than being cut: it is usually the reason
  something was refused.
- **Wording the mockups did not draw**, written in their voice and the owner's to change:
  closing an offloaded slot says "Offloaded. Hides the row, not the conversation — claude
  --resume still finds it."; no room with nothing offloadable says "Nothing can be offloaded
  right now. Close a session to make room." with only `Esc back`; a failed resume of a closed
  row says "Left closed." where mockup 5 says "Left offloaded.".
- **Nothing on the screen is numbered, so messages name a session by its title**, quoted and
  cut to 24 characters: `"retire the old tunnel" ran in /gone, which is gone …`. Mockup 6's
  `detached · it is still running` names none, because the cursor is still on it.
- **Nothing is ever resumed automatically.** Memory is spent on what the owner opens, in the
  order they open it. `Enter` on an offloaded row resumes immediately with no confirmation
  (owner, 2026-10-01); near the memory ceiling it can instead answer with the no-room offer
  for a *different* slot, which is accepted rather than designed around.
- **Keys:** `Enter` open (on the Archived heading, open or shut it) · `n` new, in the
  workspace · `c` close (on a closed row, archive; on an archived one, unarchive) · `s` a
  shell · `Esc` quit ·
  `?` the keys. **`q` also quits and is listed nowhere** (owner, 2026-10-01): it is the first
  key anybody tries, it costs nothing to accept, and it would spend a column in a 40-column
  hint line that `Esc` already covers. It is written here so it is not folklore.
- **The cursor is a highlighted row** (owner, 2026-10-02), moved with the arrow keys or the
  mouse; `Enter` opens the highlighted row, and `c` closes it. **It only ever selects a
  session, or the Archived heading** (owner, 2026-10-03): it is an index into the rows, never a
  screen line, and the Archived heading is a row of its own so that `Enter` can open it, so
  the arrows step over every other heading and the blank lines, and a click on one of those or
  on the fold does nothing. The mockups draw no
  highlight because they are plain text — the highlight is reverse video, which a monochrome
  terminal shows too. Mouse input is **click reporting only** (`?1000` with SGR `?1006`),
  never motion (`?1003`): motion reports would put bytes on the metered link every time a
  pointer crossed the window, which is the idle traffic the menu exists not to make.
- **Rows keep their places within their group while the menu is open.** The order above is
  taken once, at start; afterwards a row that stays in its group stays where it is, and a row
  that is new or has changed group joins the top of its group — it has moved anyway, and the
  top is where the eye looks for what changed. The cursor goes with its row. Mockup 6 is the
  evidence for staying put — back from a session, it is no longer unread and is the most
  recent, and is still where it was.
- **What does not fit folds into `… N more`**, on the list's last line, counting the sessions
  below it — not the Archived heading. Closed and Archived are last, so they fold first. The list scrolls to keep the cursor's
  session above the fold, and scrolling up to a group's first session brings its heading.
- **Anything that takes a while shows a spinner** (owner, 2026-10-04; mockups 10 and 11):
  Docker Compose's ten braille frames, one every 80 ms, a little faster than Compose's 100.
  It stands in for the age of the session being worked on, and the status line says what is
  happening without naming the session — `closing session`, `resuming session`, `offloading
  session for room`, `archiving session` — or, for a new session or a shell, on the status
  line alone. **It shows only after a quarter of a second**, so quick actions never flash it.
  **It steps one frame at a time on a fixed cadence**, each deadline set from the last rather
  than from when the frame was drawn, so it never skips a frame and never stutters (the owner
  saw a mockup step unevenly). **Only the lines that change are written**, so a frame costs a
  line or two, and an idle menu still writes nothing. The work runs on its own thread
  (`src/work.rs`); an action that hands the terminal to a child asks the menu's thread to do
  it, so the spinner covers what comes before and after the child but never draws over it.
  While it turns, keys are ignored except `q`, Ctrl-C and a resize, so nothing can be done to
  a session mid-close. **The first frame does not wait for the list**: given a quarter of a
  second the list is usually read, and if not the frame is drawn with the spinner where the
  list will go, and `n`, `s` and `?` work meanwhile.
- **Ages tick together, once a minute.** They are measured from a clock floored to the
  minute, so rows that went idle at different seconds roll over in the same redraw rather
  than one redraw each.
- **A dialog draws the session it is about above itself, under its group's heading**, and
  names it by title in the box (mockups 3 to 5). A reading taken while the question is up
  cannot redirect it: the dialog remembers its session, not a position, and goes if the
  session does.
- **The footer sits at the bottom of the terminal** (owner, 2026-10-03): the closing rule,
  status and hints take its last lines, blank between them and the list; the mockups show
  them straight after the content only because they are drawn shorter than a terminal.
- **Width:** usable from 40 columns up. Below the width of the shortest drawn way out the menu
  refuses to draw at all and the door falls through to a shell, saying why.
- **Startup** is on the ssh path: target under 100 ms.

## Ending a session

`/exit` ends it — the process exits, zmx drops the session, `SessionEnd` marks the slot
closed. **`/clear` does not**: it starts a new conversation in the same process, which is
right for *same session, new task* and wrong for *done*. `c` in the menu closes without
opening, for something offloaded last week. Stale slots are never closed automatically; they
sit in their group. The one close the offloader makes is of an idle slot with no
conversation on disk, which has nothing to offload (*The offloader*).

**Closing is not destructive and the dialog says so.** What a close stops is the process; the
conversation stays on disk, and is listed under Closed to resume it from. The difference between close and offload is the group the row is in and that the offloader
never stops a slot on its own initiative to close it — except one with no conversation to
offload, whose row would not be listed either way.

## The door

`claude-sessions-door` (shipped by `infra`, beside its other container programs) runs
`claude-sessions`; if that exits non-zero it prints why and `exec "$SHELL" -l`. `$SHELL`
rather than a named shell, because some containers set `bash` and some `fish`. A clean quit
exits 0 and ends the ssh session. **When the terminal goes away under the menu** — an ssh link
dropping, routine on a phone — the menu exits **129** (128 + SIGHUP, what a shell reports for
a hangup) and writes nothing: there is nobody to tell (issue #6). Nothing in the binary prints
with `print!`/`println!`/`eprintln!`, which panic when the other end has gone; a lint holds
that, and panics unwind rather than abort, so not even a bug dumps core into the menu's
working directory, a git checkout.

## What `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` is, and why it is not enough

Claude Code keeps one file per live session, named by pid, holding `pid`, `sessionId`, `cwd`,
`startedAt`, **`procStart`** (the `/proc` start-time ticks), `version`, `kind`
(`interactive` | `bg`), `entrypoint`, `name` + `nameSource` + `nameSince`, `status`,
`updatedAt` and, on background sessions, `jobId` and `agent`. Found by reading it on
2026-10-01.

It is the **fast path and the corroboration, not the contract**: it is undocumented internal
state that moves with the binary, it is pid-keyed so dead files accumulate, and nothing in it
records *when you last looked*, which is exactly what `unread` is. The hooks remain the
source of truth.

## zmx, measured 2026-10-06

zmx 0.8.1 replaced abduco on the owner's choice after v0.3.8 (*Binding a hook to a slot*), over
dtach and shpool: dtach keeps no history, so a dropped link loses the screen; shpool runs one
daemon for every session and is not packaged for the Pi. zmx is one daemon per session, as
abduco was, and keeps a terminal emulator (libghostty-vt) fed with the session's output.
Measured against the release binary here, and against its source:

- **No alternate screen.** Its client sends no `?1049h`; output reaches the terminal's own
  buffer, which is the point. `tests/zmx_real.rs` holds that, calibrated by abduco's bytes.
- **Every attach replays the session's history into the terminal:** about 1.08× the bytes the
  session printed, the same on every attach, capped near 10,000 lines (≈480 KB at 80 columns,
  ≈120 KB once ssh compresses it; ≈90 KB compressed at 40 columns). That is the price, on the
  metered link, of history surviving a dropped link. Reattaching at a narrower width rewraps
  the stored lines and uses up the cap, so moving between phone and laptop trims history.
- **Detaching resets the terminal** (`ESC c`), which Termux answers by clearing its scrollback:
  history lives in the session, not the terminal, and comes back on the next attach.
- **The very first attach misses the first few milliseconds** — what the program wrote before
  the creating client connected — and replays them on the next attach. Creating the session
  from a client with no terminal and attaching after loses them outright, so the menu does not.
  Claude takes far longer to draw anything.
- **`zmx list` is the reading.** Tab-separated `name= pid= clients= created=` per session —
  `pid` is the program, so a slot's claude — or `name= err= status=` for a daemon that did not
  answer within its second. It removes a dead daemon's socket itself when the connection is
  refused, so it never reports a corpse as attached, as abduco's socket bit did. The menu calls
  it when the socket directory changes, after anything it does, and at most a minute apart
  otherwise: every listing connects to every daemon and each logs it, and a two-second poll
  would rewrite the logs (2 MB each, then wiped) all day. A session attached from another
  terminal shows dim up to a minute late.
- **`zmx attach` creates what is not there** — hence `zmx attach <slot> false` to attach — and on
  a name that exists ignores the command, hence a start only ever on a free name.
- **It must be installed as `zmx`.** A slot's claude binds only when its parent's comm — the
  executable's file name — is `zmx`; a copy under another name starts sessions that never
  bind, which is what a renamed binary did in this repo's own test run.
- **`ZMX_SESSION` is set inside every session**, and `zmx attach` from inside one switches the
  calling terminal rather than nesting. Every zmx call the binary makes strips it, and
  `ZMX_SESSION_PREFIX`.
- **The daemon outlives its program by about 2.4 s**, though the session leaves the listing at
  once. **A daemon killed with `KILL` leaves its program running**, reparented to init — abduco
  hung up its child; zmx does not — so such a claude reads as a live slot with no zmx above it.
- **Its exit status is not reported**, by the client or the logs.
- **An upgrade that changes zmx's protocol kills every session** (its README). A container
  recreate ends them all anyway; an in-place zmx upgrade must be treated as one.

## Unregistered sessions

A `zmx attach work claude` somebody types is real work that nothing here started. The menu
lists **zmx's sessions ∪ the registry**, under Idle, since nothing reports what they are
doing. They cannot be named while they run — there is no session id to map to a transcript,
so all there is to show is the zmx session name. Matching `cwd` and start time against transcripts would work and is
guesswork. Once one ends, its conversation is in Claude Code's store like any other, and is
listed under Closed, by its own title, to resume in a slot of ours.

## Where the hooks live: `/etc/claude-code/managed-settings.d/`

**Settled 2026-10-01 from the vendor documentation, not yet by a run.** The hooks go in a
file-based managed settings drop-in, `/etc/claude-code/managed-settings.d/claude-sessions.json`,
so every repo in a container gets them without touching its own `.claude/`. **The binary
prints that file**, so the hooks installed and the events handled cannot drift apart:

```
claude-sessions hooks-config /usr/local/bin/claude-sessions \
  > /etc/claude-code/managed-settings.d/claude-sessions.json
chmod 0644 /etc/claude-code/managed-settings.d/claude-sessions.json
```

It installs the eight events of the table and the status line, `PostToolUse` matched to
exactly the three timer tools, with a 5 s timeout and **1 s on `SessionEnd`** — a longer one would raise the budget
every `SessionEnd` hook on the box shares. `src/hooks_config.rs` has the reasons and the tests
that hold it to the state machine. A drop-in rather
than `managed-settings.json` itself because Claude Code merges `managed-settings.json` first and
then every `managed-settings.d/*.json` alphabetically, and `infra` can own one file for this
tool without editing a shared one.

**The question was whether that file makes Claude Code stop and ask.** The binary carries a
consent dialog — *"Managed settings require approval"*, *"unchanged since your last
approval"*, and the error `Managed-settings consent dialog exited without an answer` — found
by grepping 2.1.286. Had hooks in a managed settings *file* triggered it, every launch after a
hook change would block on a keypress inside the session, and a non-interactive path would die.

**It does not, by the documentation's own scoping.** The dialog is a feature of
*server-managed* settings, the ones fetched from the claude.ai admin console:

- `code.claude.com/docs/en/managed-settings` — the delivery table gives server-managed
  settings as "Fetched at startup and polled hourly; see changes that need approval", and the
  file-based mechanism as "Read at startup and reloaded when a file changes", with no approval.
  Under *Where and when a policy applies*: "**Changes that need approval**: … a server-managed
  change to a setting that needs approval, such as a hook or an `env` variable, waits for the
  developer to accept the dialog in an interactive session".
- `code.claude.com/docs/en/server-managed-settings` § *Security approval dialogs* lists hooks
  ("any hook definition") among what needs approval, and every case it describes is about
  *delivered* settings and the fetch's cache — approval memory is keyed by the credential the
  settings fetch uses.

So the strings in the binary are that server-managed dialog, and a file written by root in the
image is trusted because only root could write it. A non-interactive run would not have died
anyway: for server-managed settings the documented behaviour is that `claude -p` "applies them
for that run only".

**What would still change this: the first start on a box.** Put the hooks in the drop-in, start
`claude` in a slot, and see no dialog — that is the confirming run, and it costs nothing
because it is the bring-up anyway. If a dialog does appear, the fallback is the user settings
file, which `infra` also owns in these containers.

**Do not also deliver these hooks through server-managed settings.** That *would* raise the
dialog, and the managed tier uses the first source that delivers any policy key — server
first — so a server-managed policy would also shadow this file entirely.

**Facts from the same reading that bear on the hooks** (documentation, 2026-10-01):

- **A user cannot turn them off by accident.** `docs/en/hooks`: "`disableAllHooks` set in user,
  project, or local settings can't disable those managed hooks. Only `disableAllHooks` set at
  the managed settings level can disable managed hooks."
- **`allowManagedHooksOnly`** is a managed-only lock: "Your user, project, local, and plugin
  hooks are blocked." Not needed here, and not ours to set — it is a policy about other hooks.
- **Edits to the file are picked up without a restart** — the file-based mechanism is
  "reloaded when a file changes" — so a hook change rolled out by `infra` reaches running
  slots.
- **Hooks do not run at all in some modes.** The 2.1.286 grep found refusals for `safe_mode`,
  `bare_mode` and a `diskless` kind of cloud session. A session in one of those feeds the
  registry nothing and shows as a slot with no events, which `reconcile` and `doctor` treat as
  "no evidence", never as "idle and offloadable".
- **An unreadable file is silently no policy.** If the OS denies the read, "every session
  starts without that source's policies", recorded in `/status` and `claude doctor`. The
  drop-in must be world-readable (`0644`), or the hooks vanish with no other symptom.
