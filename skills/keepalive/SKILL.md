---
name: keepalive
description: Use in a claude-sessions slot before you start or wait on something that will sit quiet for more than about five minutes — a sleep before a check, waiting on CI, a deploy or another remote job, a ScheduleWakeup or /loop, a server waiting for requests, a poll that prints a line now and then. Runs `claude-sessions keepalive <duration>` so the session is not offloaded while it waits.
---

# Keeping a quiet wait alive

This session runs in a claude-sessions slot. When nobody is attached, the offloader stops a
slot whose processes have used little CPU and moved only a trickle of data — up to tens of
KB a minute — for **10 minutes**, and the owner resumes it later. It cannot tell a wait from an
idle session: a `sleep 900 && gh run view`, a remote build you poll later, your own
`ScheduleWakeup`, a dev server waiting for a request, or a loop printing a line a minute all
look idle, and the work dies with the session.

So before such a wait, ask for time:

```sh
claude-sessions keepalive 25m
```

## How long

**As short as covers the wait, plus a margin** of five minutes, or a fifth of the wait if
that is more. Every minute you ask for is memory the owner's box cannot reclaim. Quiet time
still counts during a keep-alive, so when it ends a session that stayed quiet is offloaded at
the next pass: the margin is all the slack there is.

| waiting on | ask for |
|---|---|
| `sleep 600 && …` | `15m` |
| `ScheduleWakeup` with `delaySeconds: 1800` | `36m` |
| a CI run that usually takes 20 minutes | `25m` |
| a dev server the owner will use for an hour | `72m` |

- The duration is required: `90s`, `25m`, `2h`, or a bare number of seconds. **At most 12h**:
  a longer request is refused and sets nothing.
- Asking again replaces the previous time, so a wait that runs long is extended by running it
  again before it runs out.
- `claude-sessions keepalive 0` ends it early — do that when the wait is over sooner than you
  asked for.

## When not to

- **Work that is visibly busy** — compiling, tests running, a download, a reply being
  written. The offloader sees that work, so it needs no keep-alive.
- **Short waits**, under about five minutes. The 10-minute quiet window already covers them.
- **Waits that should not outlive the owner's attention.** If the work is not worth holding
  memory for, let the session be offloaded; it can be resumed.

If the command is not found, or says it is not inside a claude-sessions slot, this session
is not managed by claude-sessions and nothing will offload it: carry on without it.
