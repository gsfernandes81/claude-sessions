// claude-sessions: tells the session menu what this pi session is doing.
//
// Shipped by `claude-sessions install` beside the skills, and linked by the container's start
// into `$PI_CODING_AGENT_DIR/extensions/`. Each event pi fires is handed to
// `claude-sessions hook` as one JSON object on stdin, as Claude Code hands its hook events,
// with `pi_event` in place of `hook_event_name`; the hook maps it onto the same table
// (`docs/design.md` § *The event table*, § *Harnesses*) and binds the slot by walking
// /proc from the hook to this pi to its zmx, exactly as it does for claude — so a pi nested
// under a tool call never rebinds a slot, and this file needs no idea which slot it is in.
//
// Plain JavaScript, no imports but node's own, so there is nothing to build or install: pi
// loads it through jiti as it loads any extension. The binary's path is written in by
// `install`, since the extension must not depend on PATH inside pi.
//
// Only what the hook reads is sent: the prompt text of a turn, never the system prompt.
// `session_start` and `session_shutdown` are waited for — the first binds the slot before
// the first prompt's event can fire, the second must land before the process exits; every
// other event is fire-and-forget, as Claude Code's async hooks are.
//
// Events are handed on one at a time, in the order pi fired them: the hook for an event is
// spawned only once the previous event's hook has exited. The registry orders what lands by
// the kernel's 10 ms tick of the hook's start, so two hooks spawned a microtask apart — a
// prompt answered as soon as it opened fires `ui_prompt_start` and `ui_prompt_end` that way —
// could land in either order inside one tick, and the end would lose to the start: the amber
// `!` for a prompt nobody is waiting on. The chain never holds pi: a fire-and-forget event
// joins it and returns at once, and only the two waited events return their place in it.
// The one hook alive at a time is not unref'd: a waited event behind it must see it exit,
// and node would otherwise count the loop idle and leave the shutdown's hook unspawned.

import { spawn } from "node:child_process";

const BIN = "__CLAUDE_SESSIONS_BIN__";

const FIELDS = ["reason", "messageType", "prompt", "name", "kind", "title", "aborted"];

let queue = Promise.resolve();

function tell(event, ctx, wait) {
  // Not under a slot and not in any zmx session: nothing is listening.
  if (!process.env.CLAUDE_SESSIONS_SLOT && !process.env.ZMX_SESSION) return undefined;
  // Read now, not when the hook's turn comes: `/new` changes the session id behind it.
  const body = { pi_event: event.type };
  for (const f of FIELDS) {
    if (event[f] !== undefined) body[f] = event[f];
  }
  const sm = ctx.sessionManager;
  body.session_id = sm.getSessionId();
  body.cwd = ctx.cwd;
  const file = sm.getSessionFile();
  if (file) body.transcript_path = file;
  const turn = queue.then(() => hook(body));
  queue = turn.catch(() => {});
  return wait ? turn : undefined;
}

// One `claude-sessions hook` with `body` on its stdin; resolves when it has exited, or at
// once when it could not be started or written to — the hook's failures are its own.
function hook(body) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(BIN, ["hook"], { stdio: ["pipe", "ignore", "ignore"] });
    } catch {
      resolve();
      return;
    }
    child.on("error", () => resolve());
    child.on("exit", () => resolve());
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(body));
  });
}

export default function (pi) {
  pi.on("session_start", (event, ctx) => tell(event, ctx, true));
  pi.on("session_shutdown", (event, ctx) => tell(event, ctx, true));
  for (const name of [
    "before_agent_start",
    "agent_settled",
    "ui_prompt_start",
    "ui_prompt_end",
    "session_info_changed",
  ]) {
    pi.on(name, (event, ctx) => {
      tell(event, ctx, false);
    });
  }
}
