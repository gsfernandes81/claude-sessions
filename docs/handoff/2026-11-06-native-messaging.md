# Replacing agent mail with Claude Code's own messaging — investigation, 2026-11-06

**First orders for the next session:** this is a proposal, not a decision. Nothing here is
built. The owner asked for "a design that links all sessions across all containers and does
away with our custom mail work"; what follows is what Claude Code 2.1.311 does, read from
its bundle, and the design it points to. Two experiments (below) were blocked by the
auto-mode classifier because they write into the shared `~/.claude/sessions/`; run them
with the owner's go-ahead before writing code.

## What Claude Code does (2.1.311, read from the bundled JS)

Same-machine messaging is **on** on the fleet, even though `disableAgentView: true` is set:
`ListAgents` in a slot lists the other slots in its container, and `SendMessage` reaches them.
That setting turns off the agent view, not messaging.

- **Registry.** Each claude writes `~/.claude/sessions/<pid>.json`: `pid`, `sessionId`, `cwd`,
  `procStart`, `pidDomain` (`linux:<boot id>:pid:[<pid ns>]`), `messagingSocketPath`, `name`,
  `nameSource` (`user|peer|derived|collision|auto|hook`), `status`, `peerProtocol: 1`,
  `peerFeatures`. Beside it, `<pid>.<sha256(socket path)>.key` (0600) holds
  `{peerToken, procStart, pidDomain}`.
- **Inbox.** A Unix socket, `$XDG_RUNTIME_DIR/cc-socks/<pid>.sock` else
  `/tmp/cc-socks/<pid>.sock` (dir 0700, socket 0600). `--messaging-socket-path` picks another.
  The wire is newline-delimited JSON, at most 1 MiB per line:
  - optional first line `{"type":"auth","token":"<peerToken or CLAUDE_CODE_MESSAGING_TOKEN>"}` —
    **required only on Windows**; on Linux the socket's mode is the only gate;
  - `{"type":"user","msg_id":…,"message":{"role":"user","content":"<cross-session-message
    from=\"uds:<sender socket>\" from-session=\"…\" hop-chain=\"…\" from-name=\"…\"
    from-mode=\"bypass|prompting\">\n<body>\n</cross-session-message>"},"priority":"next",
    "from":"uds:<sender socket>"}` — the sender wraps the body itself, and the receiver
    re-serialises it to check it is canonical;
  - `{"type":"control","action":…}`: `rename`, `peer_message_status` (held, delivered,
    refused, dropped receipts back to the sender), `notify_when_idle` / `peer_idle_notice`,
    artifact yields.
- **Delivery.** An accepted message is queued as a prompt (`origin.kind: "peer"`, `isMeta`),
  so an idle session **wakes and runs it** and a busy one gets it mid-turn. This is what our
  nudge and `Stop`-hook announcement imitate. Claude Code's own prompt already tells the model
  a peer's message is information, not an instruction, and its `isHuman()` gates fail closed
  on a peer origin. The remote view already draws these (`docs/remote-view.md`).
- **Discovery.** `ListAgents` merges three sources: local registry records, cloud sessions,
  and Remote Control ("bridge") sessions from claude.ai. **A local record whose `pidDomain` is
  not the reader's is taken as present without a pid check, as long as its socket accepts a
  connection.** That is the hook for cross-container linking.
- **Receive policy.** The `crossSessionInbound` setting can be `accept`, `hold` or `refuse`.
  Unset, it is mode parity: a sender in a different permission class (`bypass` or
  `prompting`) is **held for the person's approval**. A headless host has no surface to
  approve on, so the hold expires. Our slots run headless under the holder, so the fleet
  would need `crossSessionInbound: "accept"` in managed settings (infra's file).
- **Replies.** A receiver replies only to an address under its own socket's directory, or under
  a default `cc-socks` dir with a name shaped `<digits>.sock` or `<1-16 hex>.sock`. On a reply
  it checks, by `SO_PEERCRED`, that the process listening there is **the same pid that
  delivered the message**. So one relay process has to do both: deliver, and listen on
  the sender's stand-in socket.
- **Limits and gates.** Per-target send pacing, a receive rate limit, hop-chain loop
  detection, and GrowthBook flags (`tengu_*`) that could change behaviour remotely. There is
  no store-and-forward: a send to a dead socket fails with `ENOINBOX`.

## Two ways to link containers

**A. A relay into the native transport (recommended).** The container's `claude-sessions
agent` already runs in every container and already talks to the hub. It would:

1. For every live slot in every **other** container (the hub's roster), hold a stand-in socket
   `/tmp/cc-socks/<hex>.sock`, and write a registry record for it: a foreign `pidDomain`
   (`claude-sessions:<repo>`), `name` `<repo>/<slot>`, `status` from the remote registry, a
   synthetic pid outside the real pid range. Remove both when the slot goes. Local claudes then
   list fleet sessions in `ListAgents` and `SendMessage` to them by name, with no skill.
2. On a frame arriving at a stand-in, forward it to the hub as today, authenticated by the
   container token. The hub stamps the sender from that token plus the declared slot, as mail
   does now, and keeps `From` its own to write.
3. In the target container, connect to the slot's real socket and write the frame with
   `from` and the wrapper's `from=` rewritten to the stand-in for the **sender** there. The
   same agent process listens on that stand-in, so replies pass the pid check and come back
   the same way. `from-name` is set from the hub's identity, never the sender's claim.
4. Offloaded slot: the stand-in stays live, the relay queues the message, and delivers it
   when the slot's new socket appears on resume. That replaces the mailbox and the
   `SessionStart` announcement.
5. Name local sessions `<repo>/<slot>` too. A `SessionStart` hook can return
   `hookSpecificOutput.sessionTitle` (whether that becomes the peer name with
   `nameSource: "hook"` is not yet confirmed), or the agent sends `{"type":"control",
   "action":"rename"}` to the slot's socket.

What goes: `mail send/list/read/reply/wait/status`, `mail-hook` on `UserPromptSubmit`,
`SessionStart` and `Stop`, the three mail skills, the nudge, the mailbox store, and most of
infra's managed mail text. What stays: a slimmer hub router, the container tokens, and slot
liveness checks. Pi slots have no native inbox: they need delivery through the pi extension,
or stay outside this.

**B. Remote Control bridge (`remoteControlAtStartup: true`).** Every session registers with
claude.ai, and `ListAgents` lists the others as "on another machine (Remote Control)". This
needs no code from us, but:

- traffic goes through Anthropic's cloud;
- it needs a claude.ai login;
- every slot shows up in the owner's claude.ai and phone app;
- names are titles, with no `repo/slot`;
- offloaded slots are invisible;
- it's unknown whether it works for the headless `claude -p` the holder runs.

It's cheap to try on one container, but it's a fleet setting, so the owner and infra decide.

## Risks of A

- It rides an **undocumented internal protocol** (`peerProtocol: 1`, the registry format, and
  the foreign-`pidDomain` rule). The fleet pins 2.1.311 (`requiredMaximumVersion`), so a
  bump is deliberate. A test against the pinned claude should go in with it, as the remote
  view already does for claude internals.
- Not verified: that Claude's stale-record sweep (`dlr(record, ownDomain) && pid dead`)
  never deletes a foreign-domain record. The call shape suggests it checks the domain first.
- `crossSessionInbound: "accept"` fleet-wide means any local process as `dev` can prompt any
  slot, as it already can today through the socket. The relay must not widen that beyond
  container-token holders.

## Experiments to run first (blocked this session: they write `~/.claude/sessions/`)

1. Write a record with a foreign `pidDomain` pointing at a Python listener on
   `/tmp/cc-socks/<hex>.sock`; check `ListAgents` lists it, and `SendMessage` to it delivers
   the frame shape above (log `SO_PEERCRED` too). Remove the record and socket afterwards.
2. From that listener, deliver a message to a scratch slot (a headless `claude -p` under the
   holder, cheap model), and have the slot reply. Confirm the reply reaches the stand-in,
   and see what happens without `crossSessionInbound: "accept"`.
