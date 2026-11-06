# Replacing agent mail with Claude Code's own messaging — investigation, 2026-11-06

**First orders for the next session:** build design A below (relay into Claude Code's own
messaging) with **the hub as router**. Nothing is built yet. The owner asked for "a design
that links all sessions across all containers and does away with our custom mail work".
Both experiments have passed (see the end). The decisions are below.

## Settled, 2026-11-06 (owner, and infra's agent infra/claude-14 by mail)

- **The hub stays the router.** Agents send to the hub with today's container token, and the
  hub stamps the sender. The signed hub-free variant at the end is **not** taken, so there is
  no `keygen`, no init service, and no listener on each agent.
- **Sender identity:** the receiving side sets `from-name` from what it verified (the hub's
  stamp, then the local stand-in), never from the sender's claim. The docs will name that
  field as the authoritative one.
- **`crossSessionInbound: "accept"`** goes in **our** drop-in
  (`managed-settings.d/claude-sessions.json`), which `hooks-config` prints, not in infra's
  file. It arrives and leaves with our pin. Auto mode is in the "prompting" class, so
  auto-to-auto was never held. This covers a slot the owner starts in `bypassPermissions`,
  whose messages would otherwise be held and expire unseen while headless.
- **Policy text:** Claude Code's own rules for peer messages replace infra's managed mail text,
  which goes at cutover.
- **Pi slots are out of scope.**
- **No separate traffic log.** The owner reads messages in each session or the remote view.
  Issue #17's view goes with mail.
- **Cutover in one step:** once the relay is proven, mail goes in the same release, with no
  side-by-side release. Then infra removes the `mail` volume and `HUB_MAIL_LISTEN`.
- **Owed to infra:**
  - a check for `make verify` and dev-smoke that the pinned claude speaks `peerProtocol 1`
    and the relay is linked;
  - a relay that switches itself off, and says so, on a protocol mismatch.
- **Queues stay durable:** a queue on the hub's side stays on a persisted volume, as the
  mail store is today, because every deploy recreates every container.
- **`tengu_*` flag risk is accepted by the owner.** Anthropic can change behaviour through
  these flags without a version change.

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

## Experiments: both passed, 2026-11-06 (owner's go-ahead)

1. **A foreign record is listed and delivered to.** We wrote a record with
   `pidDomain: "claude-sessions:test:other-repo"`, `name: "other-repo/claude-3"` and a fake
   pid, and a Python listener on `/tmp/cc-socks/c5e57.sock`. This session's `ListAgents` listed
   it as `other-repo/claude-3 · interactive · idle`, with the slash kept. `SendMessage` to it
   was delivered:
   - the sender first opened the socket twice with nothing written, to check it was live;
   - it sent no auth line, because there was no `.key` beside the record;
   - then it sent one line:
     `{"msgV":1,"msg_id":…,"type":"user","message":{"role":"user","content":"<cross-session-message
     from=\"uds:/tmp/cc-socks/32330.sock\" from-name=\"Agent Intercom\"
     from-mode=\"prompting\">\n…\n</cross-session-message>"},"priority":"next","from":"uds:…"}`.
2. **A round trip through a headless claude.** We started a throwaway `claude -p
   --input-format stream-json` (haiku, scratch cwd, managed settings as on the fleet, auto
   mode). One Python process held a stand-in `/tmp/cc-socks/d00d1.sock` and wrote a frame
   with `from` set to that stand-in into the claude's socket.
   - The idle claude woke and ran the message as a turn.
   - Its reply, `pong`, sent with `SendMessage` to the `from` address, arrived at the
     stand-in from the claude's pid, carrying a `hop-chain`.
   - There was no `peer_message_hold`, with `crossSessionInbound` unset and both sides in
     the prompting class.
   Afterwards the record, sockets and processes were removed.

## Variant considered and not taken: no hub, signed messages between agents

The owner chose the hub as router (2026-11-06). This is kept for the reasoning.

The hub does four jobs for mail: carrying messages between containers, proving who sent them,
keeping the roster, and queueing for a container that is down. All four can move into the
container agents, with signatures standing in for the container token. Dependencies are now
allowed (owner, 2026-11-06), so the signing comes from a crate (`ed25519-dalek`, pure Rust,
builds for musl), not hand-written cryptography.

- **Keys.** Each container has an ed25519 key pair. The private key is that container's
  alone. Every container holds the fleet's public keys, keyed by repo, and that list is the
  trust root. Its source is infra's choice: generated at compose time and mounted read-only
  (no hub at all), or written once by the hub into the shared install volume it already
  writes (the hub at install time only, never in the message path).
- **Envelope.** `{from: repo/slot, to: repo/slot, msg_id, sent_at, from_session, body}` in one
  canonical byte form, plus the sender container's signature. The receiver checks:
  - the signature against `keys[from.repo]`;
  - that `to` names its own repo, so a message can't be redirected;
  - that `sent_at` is inside a window (say 10 minutes);
  - that `msg_id` hasn't been seen inside that window, so a message can't be replayed.
- **Which slot sent it.** The container vouches for its own slots. The sending agent learns
  the slot from the local stand-in connection: `SO_PEERCRED` gives the pid, the claude
  registry gives the session, and our registry gives the slot. So no claude can claim
  another slot's name, which is stronger than today's declared slot header.
- **Transport.** Agent to agent over plain HTTP on the compose network. The signature
  covers the message, not the connection, so a queued or forwarded envelope stays
  verifiable. The sender's agent queues on disk and retries a container that is down.
  Anything in between, including a hub kept as a plain forwarder or Cloudflare later, can
  delay a message but can't forge one. mTLS (rustls) would encrypt and authenticate the
  connection, but proves nothing about a message once it has been queued. It can be added
  later if the link leaves the host.
- **Roster.** Each agent serves its slots (name, status, live or offloaded) and the others
  poll it, or get it pushed on a change. The list of peers is the public-key list.
- **Not checked:** whether containers can reach each other directly on the compose
  network. From here only `hub` was resolved.
