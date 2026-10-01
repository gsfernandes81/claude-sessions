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

What this tool does **not** do is keep its own copy of things that are cheaply readable while
a process is alive. The title and the busy flag come from Claude Code's own file for a live
slot; our stored copies are the last-known value, for when it is gone. See `src/live.rs`,
which says the same thing beside the code that does it.

## Slots, conversations, and what is stored

A **slot** is one `abduco` session named `claude-<n>`, i.e. one running `claude` process. A
slot holds a sequence of **conversations** (Claude session ids): `/clear` starts a new one in
the same process, `--resume` re-enters an old one.

The registry is keyed by slot and records:

`slot` · `pid` + process start time (so a reused pid is never mistaken for the original) ·
`session_id` (current) · `cwd` · `title` · `state` · `last_activity` · `last_attach` ·
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
abduco -c claude-<n> env CLAUDE_SESSIONS_SLOT=claude-<n> claude …
```

so every hook inherits the name. **A hook binds to the slot only when its `claude` is the
direct child of that slot's `abduco` server**, checked in `/proc`. A nested claude — a
`claude -p` from a Bash tool call, or a subagent — inherits the variable too and must count
as *work running under* the slot, never rebind its `session_id`. Hook payloads carry
`agent_id`/`agent_type` **on subagents only**, which is a cheaper test than the `/proc` walk
for that case; a `claude -p` spawned from a shell still needs the walk.

Sessions started without the menu (see *Unregistered sessions*) are found by walking `/proc`
from the hook upwards to the first `claude` whose parent is an `abduco` server.

## The event table

`claude-sessions hook` reads the event's JSON on stdin. **It always exits 0** — see
`CLAUDE.md`.

| event | registry effect |
|---|---|
| `SessionStart` (`startup` / `resume` / `clear` / `compact` / `fork`) | bind `session_id`, `cwd`, pid + start time; state live |
| `UserPromptSubmit` | `last_activity = now`, busy, clear `needs_you` |
| `Stop` | `last_activity = now`, idle since now |
| `Notification`, type `permission_prompt` / `elicitation_dialog` / `agent_needs_input` | `needs_you` — never offloaded while set |
| `Notification`, type `idle_prompt` | **nothing.** It fires about a minute after every `Stop` nobody answers; treating it as `needs_you` would make every detached session permanent |
| `PostToolUse` on `ScheduleWakeup` / `CronCreate` / `CronDelete` | add or remove a timer, with its due time |
| `SessionEnd`, reason `clear` **or `resume`** | nothing — a `SessionStart` follows in the same process |
| `SessionEnd`, any other reason | `closed`, unless the slot is marked `offloading`, in which case `offloaded` |

Three details that cost something if missed, read from the vendor hook documentation on
2026-10-01:

- `SessionStart.source` has five values including **`fork`**, and carries
  `seconds_since_last_response` on a resume — a better idle clock than anything computed here.
- `SessionEnd.reason` has six including **`resume`**. Treating `resume` as an end marks a slot
  closed every time a conversation is resumed.
- `CronList` and `TaskStop` are **not** timers and must not pin a slot.

## The offloader

Offloadable when: detached · `Stop` is the latest event · no `needs_you` · **no pending
timer** (owner, 2026-10-01: never, whoever set it) · no non-`claude` descendants · idle past
the threshold.

**The threshold is 10 minutes** after `Stop` (owner, 2026-10-01). The hour floor the old
shell script used existed only because self-scheduled wake-ups were invisible; the timer
records make them visible, so the floor goes.

It holds the slot lock from decision through kill, marks the slot `offloading` before
signalling and `offloaded` after, and keeps `TERM` → grace → `KILL` → `abduco` teardown with
the pid-plus-start-time check at each step.

**Low memory is the container's, not the host's.** `/proc/meminfo` inside a container reports
the whole machine; the ceiling is the cgroup. Read `/sys/fs/cgroup/memory.max` and
`memory.current` — both readable unprivileged on cgroup v2, verified 2026-10-01 — and fall
back to `MemAvailable` only when the limit reads `max`.

**The orphan sweep** collects `daemon run --origin transient` trees whose spawning pid and
start time are gone, with their `bg-pty-host` / `bg-spare` children. **It ships logging what
it would kill and nothing else**, the owner reads a week of that log, and only then is it
armed.

## The menu

- **Lists open slots** — live and offloaded, never closed — plus unregistered `abduco`
  sessions. Order: wants you, then unread, then most recent activity.
- **Opening a row:** live → `abduco -a`; offloaded → start a new slot running
  `claude --resume <session_id>` in its `cwd`. `abduco` runs as a **child**, so on detach the
  menu comes back rather than the login ending — a fresh login costs a Cloudflare Access
  handshake on a metered link.
- **Nothing is ever resumed automatically.** Memory is spent on what the owner opens, in the
  order they open it. `Enter` on an offloaded row resumes immediately with no confirmation
  (owner, 2026-10-01); near the memory ceiling it can instead answer with the no-room offer
  for a *different* slot, which is accepted rather than designed around.
- **Keys:** `Enter` open · `n` new, in the workspace · `c` close · `s` a shell · `Esc` quit ·
  `?` the keys. **`q` also quits and is listed nowhere** (owner, 2026-10-01): it is the first
  key anybody tries, it costs nothing to accept, and it would spend a column in a 40-column
  hint line that `Esc` already covers. It is written here so it is not folklore.
- **Width:** usable from 40 columns up. Below the width of the shortest drawn way out the menu
  refuses to draw at all and the door falls through to a shell, saying why.
- **Startup** is on the ssh path: target under 100 ms.

## Ending a session

`/exit` ends it — the process exits, `abduco` goes with it, `SessionEnd` marks the slot
closed. **`/clear` does not**: it starts a new conversation in the same process, which is
right for *same session, new task* and wrong for *done*. `c` in the menu closes without
opening, for something offloaded last week. Stale slots are never closed automatically; they
sort to the bottom.

**Closing is not destructive and the dialog says so.** What a close stops is the process; the
conversation stays on disk and `claude --resume` brings it back. The only difference between
close and offload is whether the row stays in the list.

## The door

`claude-sessions-door` (shipped by `infra`, beside its other container programs) runs
`claude-sessions`; if that exits non-zero it prints why and `exec "$SHELL" -l`. `$SHELL`
rather than a named shell, because some containers set `bash` and some `fish`. A clean quit
exits 0 and ends the ssh session.

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
- **A killed server leaves its socket behind with the attached bit still set.** This is the
  calibration catch: a menu built on the mode alone shows a dead session as attached and
  refuses to offer it. The liveness test is the pid plus its start time; the mode only ever
  answers *attached?* for a slot already known to be alive.

## Unregistered sessions

Until every client is reconfigured, `ssh <container>` still runs `abduco -A claude claude`,
and so does `make claude` in several repos. The menu lists **`abduco`'s sessions ∪ the
registry**, marking the ones it did not start with `u`. They cannot be named — there is no
session id to map to a transcript, so all there is to show is the `abduco` session name.
Matching `cwd` and start time against transcripts would work and is guesswork; it is worth
building only if those rows turn out to persist.

## Open question that blocks where the hooks live

**Managed settings can make Claude Code stop and ask.** The binary carries a consent dialog —
*"Managed settings require approval"*, with *"these can change where Claude Code runs or what
it can connect to"*, an *"unchanged since your last approval"* memory, and the error string
`Managed-settings consent dialog exited without an answer`. The counts it elides are
`elidedCommandCount`, `elidedSandboxCount` and `elidedIsolationCount`, so those categories are
certainly in scope. Found by grepping 2.1.286 on 2026-10-01.

**Whether `hooks` in a managed settings file triggers it is still not established**, and the
plan is to put these hooks exactly there — in `/etc/claude-code/managed-settings.json`, so
every repo gets them without touching its own `.claude/`. If it does trigger, every launch
after a hook change blocks on a dialog: one keypress inside `abduco`, but any non-interactive
path dies with that error string.

**Establish it like this:** add a hook to that file in a scratch container and start
`claude -p`. It needs root on the container, which is why it is still open — the session that
wrote this had no way to write `/etc`.

**What the same grep did settle**, all from 2.1.286 on 2026-10-01, and all of it bears on where
the hooks go:

- **`allowManagedHooksOnly` is a real policy setting.** A refusal reason reads
  `managed_hooks_only: "the organization allows only managed hooks"`. So managed hooks are a
  first-class concept rather than a side effect, which is an argument for putting them there.
- **`disableAllHooks` is a user setting that turns every hook off** — refusal reason
  `hooks_disabled_in_settings: "hooks are turned off in your settings (disableAllHooks)"`.
  Worth knowing because it would make the registry go blind with no visible symptom; `doctor`
  printing the age of each event is how that would be noticed. Whether a managed setting can
  stop a user turning them off is untested.
- **Hooks do not run at all in some modes.** There are refusals for `safe_mode`, `bare_mode`
  and a `diskless` kind of cloud session. A session running in one of those will not feed the
  registry, so it will show as a slot with no events — which `reconcile` and `doctor` have to
  treat as "no evidence", not as "idle and offloadable".
