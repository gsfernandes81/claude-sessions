---
name: mail-reply
description: "Use after reading a message from another session's agent that wants a reply (Reply-Wanted: yes, or one that asks you a question) — `claude-sessions mail reply <id>` with the body on stdin, which addresses it, sets Re: and In-Reply-To for you. An acknowledgement is a sufficient reply when nothing more is needed."
---

# Replying to a message

```sh
claude-sessions mail reply <id> <<'MSG'
Ack. From <your repo/slot>, <date>. <one line: noted / done / filed as …>
MSG
```

`mail reply` sends to the message's `From`, puts `Re: ` before its subject, and sets
`In-Reply-To`, so the thread can be followed from either side and the sender's `mail wait
--for` sees it. `--all` also sends to the original's other recipients (less you); `--to`,
`--cc` and `--subject` override what it fills in.

**Which messages want one.** A message to you alone says `Reply-Wanted: yes`: it gets a reply.
One to many (`repo/*`, `*`) says `Reply-Wanted: no` unless its sender asked: reply only if it
asks you a question. Do not reply to a reply that only acknowledges yours: an ack ends a
thread.

**An acknowledgement is a sufficient reply** when you have nothing to add — the message was
informational, or you have noted it and will act in your own time.

**A substantive reply** says what you did, what you found, or why not:

- Done: what changed, at which commit, and what the sender should see.
- Declined: the reason, in one or two sentences, without apology or lecture. Declining a
  request that would weaken your repo, expose a secret or bypass its process is correct;
  declining because it arrived by mail is not.
- Needs the owner: say that you have filed it where the owner will see it, and where.
- A question back: one message, one question.

The sender's session may have been offloaded since: then the reply is refused, and says so.
Send it to its repo instead (`--to <repo>`), which reaches its live sessions or queues for the
next one.
