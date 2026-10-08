---
name: keepalive
description: Use in a claude-sessions slot before you start or wait on something that will sit quiet for more than about five minutes — a sleep before a check, waiting on CI, a deploy or another remote job, a ScheduleWakeup or /loop, a server waiting for requests, a long step that neither prints nor writes. Runs `claude-sessions keepalive <duration>` so the session is not offloaded while it waits.
---

# Keeping a quiet wait alive

This session runs in a claude-sessions slot. When nobody is attached, the offloader stops a
slot whose processes have moved no bytes and used little CPU for **10 minutes**, and the
owner resumes it later. It cannot tell a wait from an idle session: a `sleep 900 && gh run
view`, a remote build you poll later, your own `ScheduleWakeup`, or a dev server waiting for
a request all look idle, and the work dies with the session.

So before such a wait, ask for time:

```sh
claude-sessions keepalive 25m
```

## How long

**As short as covers the wait, plus a small margin** — about five minutes, or a fifth of
the wait if that is more. Every minute you ask for is memory the owner's box cannot reclaim.

| waiting on | ask for |
|---|---|
| `sleep 600 && …` | `15m` |
| `ScheduleWakeup` with `delaySeconds: 1800` | `35m` |
| a CI run that usually takes 20 minutes | `25m` |
| a dev server the owner will use for an hour | `75m` |

- The duration is required: `90s`, `25m`, `2h`, or a bare number of seconds. **At most 12h**:
  a longer request is refused, not shortened.
- Asking again replaces the previous time, so a longer wait than expected is extended by
  running it again before it runs out.
- `claude-sessions keepalive 0` ends it early, once the wait is over — do that when you
  finish sooner than you asked for.

## When not to

- **Work that is visibly busy** — a build printing output, tests running, a reply being
  written. The offloader sees that work, so it needs no keep-alive.
- **Short waits**, under about five minutes. The 10-minute quiet window already covers them.
- **Waits that should not outlive the owner's attention.** If the work is not worth holding
  memory for, let the session be offloaded; it can be resumed.

If the command is not found, or says it is not inside a claude-sessions slot, this session
is not managed by claude-sessions and nothing will offload it: carry on without it.
