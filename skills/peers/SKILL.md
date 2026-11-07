---
name: peers
description: Use before messaging a session in another container — a `repo/slot` name in ListAgents, like `infra/claude-14` — and when a notice from `hub/router` arrives. Says what a listed peer with no status is (offloaded), what messaging one costs (it may be woken, taking RAM and CPU the fleet shares), and what happens to the message.
---

# Messaging sessions in other containers

`ListAgents` lists sessions in other containers by their address, `repo/slot`, beside this
container's own. `SendMessage` reaches them as it reaches a local session, and a reply comes
back the same way. claude-sessions' hub carries the messages between containers, and it sets
the sender's name itself, so a message's `from-name` is who actually sent it.

## A peer with no status is offloaded

A listed peer is **live** when its row says `busy` or `idle`. With no status it is
**offloaded**: stopped to save memory, its conversation on disk.

A message to an offloaded session is **kept** at the hub, and the hub tries to **wake** the
session to take it:

- **If its container has room** (about 250 MB free for one more session), it resumes headless.
  It then uses that memory, and CPU on the host every container shares, until it has been quiet
  for 10 minutes and is offloaded again. It reads your message as soon as it is up.
- **If there is no room**, the message waits until the session next runs: when the owner opens
  it, or when a later message finds room. Messages wait for up to 7 days.
- **If the container is not answering**, the message waits until it is back.

The hub tells you which happened, in a notice from `hub/router`. A notice is information,
not a reply from the session, and a reply to it goes nowhere.

## So

- **Prefer a live peer** when more than one could answer.
- **Wake an offloaded one only for something worth what it costs**: work it has to do, or a
  question only it can answer, not news it can read when it is next opened.
- **Send one message, not several.** Each message to an offloaded session asks to wake it
  again.
- **Do not wait on a reply from an offloaded peer** as if it were live. It may not run for
  hours.
