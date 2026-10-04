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

A **slot** is one `abduco` session named `claude-<n>`, i.e. one running `claude` process. A
slot holds a sequence of **conversations** (Claude session ids): `/clear` starts a new one in
the same process, `--resume` re-enters an old one.

The registry is keyed by slot and records:

`slot` · `pid` + process start time (so a reused pid is never mistaken for the original) ·
`session_id` (current) · `cwd` · `title` (custom) · `ai_title` · `first_prompt` · `state` · `last_activity` · `last_attach` ·
`needs_you` · `timers` (each with its due time, and whether it recurs)

**States:** `attached` / `detached` — read from `abduco`, never stored — plus `offloaded` and
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
abduco -c claude-<n> env CLAUDE_SESSIONS_SLOT=claude-<n> \
  sh -c 'e=$1; shift; exec "$@" 2>>"$e"' sh <registry>/claude-<n>.stderr claude …
```

so every hook inherits the name. The `sh -c` captures claude's stderr to a file the
failed-resume dialog reads (mockup 5), and **its `exec` is load-bearing**: it replaces the
shell with claude, so claude is still the abduco server's direct child, which is the binding
test below. Without it the shell stays in between and no hook ever binds — `tests/launch.rs`
runs this exact line under the real hook to pin that. The capture path is an argument, not an
environment variable, so nothing of ours leaks into claude's environment. **A hook binds to the slot only when its `claude` is the
direct child of that slot's `abduco` server**, checked in `/proc`. A nested claude — a
`claude -p` from a Bash tool call, or a subagent — inherits the variable too and must count
as *work running under* the slot, never rebind its `session_id`. Hook payloads carry
`agent_id`/`agent_type` **on subagents only**, which is a cheaper test than the `/proc` walk
for that case; a `claude -p` spawned from a shell still needs the walk.

Sessions started without the menu (see *Unregistered sessions*) are found by walking `/proc`
from the hook upwards to the first `claude` whose parent is an `abduco` server.

## The event table

`claude-sessions hook` reads the event's JSON on stdin. **It always exits 0** — see
`CLAUDE.md` — even if it panics: the panic is caught and written to `hook.log`, never to the
terminal.

| event | registry effect |
|---|---|
| `SessionStart` (`startup` / `resume` / `clear` / `compact` / `fork`) | bind `session_id`, `cwd`, pid + start time; state live. A **different** `session_id` also drops `title`, `ai_title`, `first_prompt` and the per-event times, so a new conversation never wears the old one's name or reads as prompted by the old one's prompt; then the titles are read from the transcript at `transcript_path`, as on every `Stop`. Every source but `compact` leaves claude at its prompt: not busy, `needs_you` cleared, `ready_ms = now`; `compact` changes none of those |
| `UserPromptSubmit` | `last_activity = now`, busy, clear `needs_you`; the first one of a conversation sets `first_prompt` — one line, at most 120 characters, the title of last resort |
| `Stop` | `last_activity = now`, idle since now |
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

Offloadable when: detached · `Stop`, or a start at the prompt, is the latest event · no
`needs_you` · **no pending timer** (owner, 2026-10-01: never, whoever set it) · no
non-`claude` descendants · idle past the threshold.

**The threshold is 10 minutes** after `Stop` (owner, 2026-10-01). The hour floor the old
shell script used existed only because self-scheduled wake-ups were invisible; the timer
records make them visible, so the floor goes.

It holds the slot lock from decision through kill, marks the slot `offloading` before
signalling and `offloaded` after, and keeps `TERM` → grace → `KILL` → `abduco` teardown with
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
the whole machine; the ceiling is the cgroup. Read `/sys/fs/cgroup/memory.max` and
`memory.current` — both readable unprivileged on cgroup v2, verified 2026-10-01 — and fall
back to `MemAvailable` only when the limit reads `max`.

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

- **"`Stop` is the latest event"** is the later of `last_stop_ms` and `ready_ms` being at or
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
- **Not being able to look keeps it.** No abduco socket (attached cannot be ruled out), a
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
  reported and the slot left `offloading` for the next pass to decide again. Then the abduco
  server, recorded at decision time and only if the claude's parent really is `abduco`, gets
  2 s to exit by itself and a `TERM` if it does not; its socket is removed only once that
  server is known dead.
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

## The menu

- **Lists every live and offloaded slot, every unregistered `abduco` session, and every
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
- **`Enter` on a closed row resumes it in a slot**, `claude --resume <id>` under `abduco` in
  its start directory. If a slot's record already names the conversation, that slot resumes
  it — both ways in then take one slot lock; otherwise a new slot is allocated, the never-
  resume-running check made under the allocation lock, and the record, naming the
  conversation, written before the lock goes, so a second menu finds it starting.
- **An offloaded slot is listed only with a conversation on disk** — its `transcript_path`,
  or for a record older than 0.3.3 the path Claude Code keeps it at under `CLAUDE_CONFIG_DIR`
  — with an exchange in it. A slot offloaded before its first prompt, or `/clear`ed and left,
  has nothing to resume (issue #5), and the resume path refuses such a slot too, for one whose
  transcript went after the list was read.
- **Opening a row:** live → `abduco -a`; offloaded → start a new slot running
  `claude --resume <session_id>` in its `cwd`. `abduco` runs as a **child**, so on detach the
  menu comes back rather than the login ending — a fresh login costs a Cloudflare Access
  handshake on a metered link. Coming back from an attach is what writes `last_attach_ms`,
  the only writer it has, so that is what clears `unread`.

### How the menu acts, where the rules above left a choice

Built 2026-10-02 in `src/launch.rs`; each is tested there with stand-in `abduco` and `claude`.

- **A resume keeps the slot's name.** The record is reused — same title — and marked
  registered, so an unregistered slot comes back as one of ours.
- **Deciding happens under the slot's lock; running happens after it.** Under the lock the
  record is re-read, the conversation is checked not to be running anywhere — Claude Code's
  own live-session files and every other record — and the record is marked live with its pid
  cleared before the lock is let go. A second menu then sees a slot that is starting, not one
  it may resume. If the conversation *is* running in another abduco slot, that slot is
  attached instead; anywhere else, the resume is refused.
- **A live record with no pid** is a slot starting elsewhere: attached if its socket exists,
  refused for 30 s otherwise, and after that treated as a start that died and is resumable —
  so a menu killed mid-start cannot strand a slot.
- **A failed resume** is a start that ends within 10 s without a `SessionStart` binding it: the
  record goes back to offloaded or closed, whichever it was, and mockup 5 shows the last lines
  of the captured stderr, control characters removed — or says it wrote nothing to stderr,
  rather than leaving a gap that reads as a lost reason. A start into a `cwd` that no longer exists is refused before it
  runs, and a leftover socket of the same name is refused with a pointer to `reconcile`.
- **New slots take the lowest free `claude-<n>`** under a registry-wide lock with a timeout,
  writing the record before the lock goes. A closed slot's name is reused: closed slots are
  not listed, their conversations are, from the store, under their own ids. A new slot that
  dies before binding is marked closed and reported, since it has no row yet.
- **Room is checked once.** After offloading to make room, the open goes ahead without asking
  the cgroup again: its figure includes page cache that is not freed at once, and a second
  check could refuse the room just made.
- **`c` on a live slot stops it** with the offloader's own `TERM` → `KILL` path and abduco
  teardown, pid and start time checked at each step, then marks it closed. A row with no record
  at all is refused: there is nothing to identify its process by safely.
- **The programs are overridable** for tests and odd installs: `CLAUDE_SESSIONS_ABDUCO` and
  `CLAUDE_SESSIONS_CLAUDE` name `abduco` and `claude`; `CLAUDE_SESSIONS_WORKSPACE` names where
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

`/exit` ends it — the process exits, `abduco` goes with it, `SessionEnd` marks the slot
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

## abduco, measured 2026-10-01

- **Attached is a file mode, not a listing to parse.** `~/.abduco/<name>@<hostname>` has the
  owner-execute bit set while a client is attached: `srwx------` attached, `srw-------`
  detached. One `stat` per slot, no subprocess.
- **The socket name carries the hostname**, which these containers derive from their alias —
  so renaming a container orphans every session in it.
- **Two clients can attach to one session at once.** *Attached elsewhere* is not an exclusive
  lock, and opening a row must not assume it is alone at the terminal.
- **A session's name is read from its abduco's command line the way abduco reads it** —
  getopt-style, so `-fA work` names `work`, and only `-e` takes a value. `reconcile` sweeps a
  socket only when every live abduco could be named and none names it; one it cannot name
  turns the sweep off, because that process might own any socket in the directory (issue #3).
- **A killed server leaves its socket behind with the attached bit still set.** This is the
  calibration catch: a menu built on the mode alone shows a dead session as attached and
  refuses to offer it. The liveness test is the pid plus its start time; the mode only ever
  answers *attached?* for a slot already known to be alive.

## Unregistered sessions

Until every client is reconfigured, `ssh <container>` still runs `abduco -A claude claude`,
and so does `make claude` in several repos. The menu lists **`abduco`'s sessions ∪ the
registry**, under Idle, since nothing reports what they are doing. They cannot be named while
they run — there is no session id to map to a transcript, so all there is to show is the
`abduco` session name. Matching `cwd` and start time against transcripts would work and is
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

It installs the six events of the table, `PostToolUse` matched to exactly the three timer
tools, with a 5 s timeout and **1 s on `SessionEnd`** — a longer one would raise the budget
every `SessionEnd` hook on the box shares. `src/hooks_config.rs` has the reasons and the tests
that hold it to the state machine. A drop-in rather
than `managed-settings.json` itself because Claude Code merges `managed-settings.json` first and
then every `managed-settings.d/*.json` alphabetically, and `infra` can own one file for this
tool without editing a shared one.

**The question was whether that file makes Claude Code stop and ask.** The binary carries a
consent dialog — *"Managed settings require approval"*, *"unchanged since your last
approval"*, and the error `Managed-settings consent dialog exited without an answer` — found
by grepping 2.1.286. Had hooks in a managed settings *file* triggered it, every launch after a
hook change would block on a keypress inside `abduco`, and a non-interactive path would die.

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
