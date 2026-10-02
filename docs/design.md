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
`session_id` (current) · `cwd` · `title` · `first_prompt` · `state` · `last_activity` · `last_attach` ·
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
| `SessionStart` (`startup` / `resume` / `clear` / `compact` / `fork`) | bind `session_id`, `cwd`, pid + start time; state live. A **different** `session_id` also drops `title` and `first_prompt`, so a new conversation never wears the old one's name |
| `UserPromptSubmit` | `last_activity = now`, busy, clear `needs_you`; the first one of a conversation sets `first_prompt` — one line, at most 120 characters, the title of last resort |
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
armed. Arming it is a code change, not a flag, so a timer's command line cannot do it.

### How `claude-sessions offload` reads those rules

Built 2026-10-01 in `src/offload.rs`. One pass per invocation, run from the box's timer;
`--dry-run` decides and reports without signalling. Where the rules above left a choice, this
is the choice and why:

- **"`Stop` is the latest event"** is `last_stop_ms >= last_activity_ms` and not `busy`.
  `Stop` sets both to its own time, and everything after it — a prompt, a nested claude's
  events, a `needs_you` notification, a `SessionStart` — moves `last_activity_ms` past it.
  **Consequence worth knowing:** a slot that was resumed or started and then left without a
  prompt has no `Stop` after its `SessionStart`, so it is never offloaded until it is used
  once. Owner, 2026-10-01: leave it so for now.
- **Resumable or kept.** A slot with no recorded `session_id` or `cwd` is kept: stopping it
  would be a close with extra steps. Registered and unregistered slots get the same rules, so
  a `u` slot whose hooks did record those is offloadable and comes back as a registered one.
- **Not being able to look keeps it.** No abduco socket (attached cannot be ruled out), a
  `/proc` that cannot be listed (descendants cannot be ruled out), a record with no pid — each
  is a reason to keep, because the offloader needs evidence to act, never to hold off.
- **"No non-`claude` descendants"** walks the whole tree under the slot's claude, through any
  nested claude, and ignores zombies. A stdio MCP server would be such a descendant and would
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
- **The kill's own `SessionEnd` hook cannot write.** The offloader holds the slot lock from
  decision through kill, so that hook waits its 400 ms, gives up and logs it, and the
  offloader writes `offloaded` itself. A `hook.log` line per offload is expected.
- **The sweep infers "spawner gone" from the parent**: a transient daemon whose parent is no
  longer a `claude` has been reparented away from the session that started it. That inference
  has not met a real daemon — none was running where this was written. If the daemon detaches
  on purpose, every one will be listed, live or not; each log line carries the full command
  line so a spawner pid in it, if there is one, can replace the inference before arming.
- **Logged to `offload.log`** beside the registry: every stop, every failed stop, and every
  would-be sweep kill. Slots kept are printed to stdout only, since a pass every few minutes
  would otherwise bury the lines that matter.

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
