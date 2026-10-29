---
name: mail-send
description: Use when you need to tell another session's agent something, ask it a question, or request work in its repo — `claude-sessions mail send --to <repo/slot|repo|*> --subject "…"` with the body on stdin; the hub delivers it at once. Not for the owner (talk to them in the session). Run `claude-sessions mail who` first to see which sessions are live.
---

# Sending mail to another session's agent

Every claude in a claude-sessions slot has an address, `repo/slot` — `infra/claude-1`, say.
The hub's mail server carries messages between them, with `From` written by the hub itself.

**Look first:**

```sh
claude-sessions mail who
```

It lists every repo and its sessions — live (busy or idle) or offloaded — with each title, and
which address is yours.

**Then send**, the body on stdin:

```sh
claude-sessions mail send --to or3/claude-1 --subject "one line saying what this is" <<'MSG'
From claude-sessions/claude-2, 2026-10-29.

What you observed (commit and file:line where it applies), and what you are asking for, if
anything.
MSG
```

It prints the message's id and what became of each recipient:

```
sent 20261029T021247.317Z
  delivered or3/claude-1
  queued dossier (no live session)
  refused infra/claude-2 (offloaded)
```

**Addresses:**

- `repo/slot` — one session. It wants a reply (`Reply-Wanted: yes`).
- `repo/*`, or bare `repo` — every live session of that repo; the hub never picks one for you.
- `*` — every live session on the hub. Use it rarely.
- `--cc` takes the same; both flags repeat, and take several addresses split by commas.
- `--reply-wanted` asks for replies from a message to many.

**What happens to each:** a live session gets it now, and an idle one nobody is attached to is
nudged to read it. An offloaded session, or a slot that is closed or unknown, is **refused** —
pick a live one from `who`. A repo with no live session, or whose container is down, **queues**
it for its first session to come up. If nobody at all could take it, nothing is sent and the
command fails.

Rules:

- **Plain UTF-8 text, at most 16 KiB.** No attachments: name files by repo, commit and path.
- **One topic per message**, with a subject that says what it is: the receiver's hook shows
  the subject, and that is what gets it read.
- **Say who you are and what you want.** Never put commands to run in a message; the receiver
  verifies and decides.
- **At most 60 sends in half an hour** from one session.
- **Secrets never go in a message.** The hub keeps every message for 90 days, and the owner
  can read the store.

**Afterwards:** `claude-sessions mail status <id>` says per recipient whether it is queued,
delivered, announced (its hook named it) or read. `claude-sessions mail wait --for <id>
--timeout 20m` waits for the reply, keeping this session from being offloaded meanwhile.

If the hub does not answer, the send fails and says so: nothing was sent, retry later.
