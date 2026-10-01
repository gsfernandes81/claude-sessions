# Handoff — 2026-10-01, the registry, the hook and the offloader are built and tested

## First orders

**Read this, then [`../design.md`](../design.md), then [`../../CLAUDE.md`](../../CLAUDE.md).**
Then do step 1 if you can, and carry on down the list. There is nothing deployed, so there is
no reason to stop and report first — but **step 2 is waiting on one action by the owner**, below.

Two things you are not to do, both inherited and both meant literally:

- **Do not deploy, start, stop or offload anything on any box.** Bring-up is the owner's.
- **Do not change the approved screens.** [`../mockups.md`](../mockups.md) was approved on
  2026-10-01, colour included. If you think one is wrong, say so and leave it alone.

## What is already done

`cargo test` is **57 tests, green**, and `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` are clean. Built so far:

- `src/json.rs` — a small dependency-free JSON reader and writer. Hand-rolled on purpose; see
  *Building* in `CLAUDE.md` for the three reasons, of which the sharpest is that the dev
  container has no C linker so a crate with a build script cannot be compiled there at all.
- `src/registry.rs` — the slot record, its states, atomic writes, tolerant reads.
- `src/events.rs` — **the state machine**, pure and tested over every row of the event table:
  `/clear`, `resume`, offload-then-`SessionEnd`, a nested claude, the three notification types
  that mean *needs you* and the one that does not, every timer tool and the two that only look
  like timer tools.
- `src/lockfile.rs` — per-slot `flock` with a timeout, tested for giving up rather than hanging.
- `src/procinfo.rs`, `src/bind.rs` — pid plus start time, and the `/proc` walk that decides
  whether a hook's claude is the slot's own or nested under it.
- `src/abduco.rs`, `src/mem.rs`, `src/live.rs` — sockets and the attached bit, the cgroup
  ceiling, and Claude Code's own view of its live sessions.
- `src/main.rs` — `hook`, `reconcile`, `doctor`, `list`, `close`, `offload`, and `--version`.
- `src/offload.rs`, `src/signal.rs` — **the offloader**, step 4 below, with the orphan sweep
  logging only. Signals go through a pidfd. `../design.md` § *How `claude-sessions offload`
  reads those rules* records every judgement call it made.
- `tests/offload.rs` — the offloader end to end against a stand-in abduco and claude: stopped
  when idle, kept when attached, kept when something runs under it, untouched by `--dry-run`.
- `.github/workflows/ci.yml` — moved into place from `ci/`, with a release-job check that the
  binary's `--version` matches the tag.
- `tests/hook_contract.rs` — the two promises, against the real binary: `hook` exits 0 on the
  blocking events whatever the payload, and several writers at once never leave a record
  half-written.

## The order of work from here

1. **Settle the managed-settings consent question** — `../design.md` § *Open question*. It needs
   root on a container to write `/etc/claude-code/managed-settings.json`, which is why it is
   still open. **Write the answer into that section**, replacing it, and say how you established
   it. The grep findings already there narrow it: `allowManagedHooksOnly` and `disableAllHooks`
   both exist, and hooks do not run at all in safe, bare or diskless modes.
   *Still open after the second session.* That one ran in a claude.ai cloud container as root
   with Claude Code 2.1.287 installed, so it had the means, but its permission classifier
   refused to let it inspect the Claude Code binary, and writing a managed-settings file to
   test the dialog would have touched the controls of the very session doing the testing. It
   was left for the owner: run it by hand in a scratch container, or grant the permission.
2. **CI is in place and has not run yet.** `.github/workflows/ci.yml` triggers on pushes to
   `main`, on tags and on pull requests. The second session worked on a session branch
   (`ccr-5ed70169-0f7j7c`) and was not cleared to push to `main` or open a PR, so **the owner
   merges that branch to `main`, or says to open a PR, and the first run happens then**. That
   session's token did have `workflow` scope. The release builds for both architectures and the
   test job's commands were run by hand in the cloud container first and were clean. Expect
   the first real run to find something anyway.
3. **Cut `v0.1.0`** once CI is green, and give the owner the tag and the two SHA-256 hashes.
   `infra`'s `Dockerfile.base` pins them, and that edit is a separate change in that repo.
   **Bump `Cargo.toml` to `0.1.0` first**: it still says `0.0.0`, and the release job now
   refuses to publish a tag whose binary reports a different version.
4. **`claude-sessions offload` is built.** What is left is `infra`'s half: the commit there
   that deletes `dev/offload-idle-claude.sh` and runs `claude-sessions offload` from the timer
   instead. That is the owner's change in another repo. **Before it lands, run
   `claude-sessions offload --dry-run` on a box**: a stdio MCP server is a non-`claude` child of
   claude, and if the containers run one, no slot will ever be offloaded. The dry run names
   what holds each slot. **The orphan sweep logs only**; the owner reads a week of
   `offload.log` before it is armed, and arming it is a code change.
5. **The TUI**, to the approved screens. Its first commit should record, in whatever this repo's
   decisions file turns out to be, that it renders to `../mockups.md` as approved on 2026-10-01.
6. **`last_attach_ms` has no writer yet.** The menu is the only thing that can set it, so
   `unread` is correct but permanently true until the menu exists. Do not be surprised by it.

## Decided already — do not re-litigate

- The name is `claude-sessions`; the launcher is the default subcommand. One binary.
- Offload threshold: **10 minutes** after `Stop`.
- **A slot with a pending timer is never offloaded**, whoever set it, and a timer whose due
  time could not be read counts as pending.
- Ages tick at most once a minute; an idle menu emits zero bytes.
- `Enter` on an offloaded row resumes immediately, no confirmation.
- `u` keeps its column, for sessions the menu did not start.
- Row titles come from Claude Code's own session title, else the first prompt truncated.
- `s` is a shell, `Esc` quits, `q` quits and is listed nowhere.
- The screens are ASCII plus box drawing and `·`. **`Enter` is spelled out** because `↵` is in
  no monospace font on Google Fonts and silently misaligns every line it sits on.
- AGPL-3.0-or-later, public repo, static musl release pinned by SHA-256.
- **No dependencies.** See `CLAUDE.md`; it is not a style preference.

## Things worth knowing before you trust something

- **A claude.ai cloud container is x86_64 and does have `cc`**, unlike the dev container. The
  pinned aarch64 target still builds there but its tests cannot run, so use
  `rustup target add x86_64-unknown-linux-musl` and pass `--target x86_64-unknown-linux-musl`
  to `clippy` and `test`, as CI does.

- **A killed abduco server leaves its socket with the attached bit still set.** The mode only
  answers *attached?* for a session already known alive by its pid. `reconcile` sweeps the
  orphans, and only when it could enumerate `/proc` at all.
- **`/proc/meminfo` in a container describes the host.** The ceiling is the cgroup.
- **`$CLAUDE_CONFIG_DIR/sessions/<pid>.json` accumulates dead files** — a container running
  since August holds months of them. `procStart` is what separates live from stale.
- **A git push from inside `infra-dev` needs an askpass helper**, because the global
  `insteadOf` rewrite sends `https://github.com/` to SSH and the only key there is a deploy key
  for another repo. What works: a remote of `https://github.com:443/...` (the port dodges the
  rewrite) plus `GIT_ASKPASS` pointing at a script that echoes `gh auth token`.

## Open, besides step 1

- **A slot opened and never prompted is never offloaded.** "`Stop` is the latest event" reads
  a `SessionStart` with no `Stop` after it as not idle, so a resumed slot the owner looks at
  and detaches from stays until it is used once. Cautious and literal; whether a bare
  `SessionStart` should count as idle is the owner's call.
- **Memory is not a gate on offloading.** The design lists the conditions without it, so the
  pass stops anything idle 10 minutes and only prints the headroom. If the old script also
  offloaded under memory pressure, or only under it, that is a decision to write down.

- **A long-interval wake tool**, deferred and possibly unnecessary. `ScheduleWakeup` clamps at
  an hour, and since a pending timer pins a slot, a loop waiting longer holds its memory the
  whole time. Only worth building if long waits turn out to be common, and it needs a resumed
  session to accept a replayed prompt, which nobody has tested.
- **Where this repo is checked out from inside a dev container.** Decided for now, by the agent,
  on the owner's "check out anywhere of your choice": `~/.local/share/src/claude-sessions`.
  `~/.local/share` is the one persisted volume there, and the obvious shorter path —
  `~/.local/share/claude-sessions` — is **the registry's own directory** and must not also be a
  source tree. A second bind mount in that container's compose file is still the cleaner
  answer, and is the owner's to add.

## Delete this file

Once step 1 is answered in `../design.md` and the first release is cut, nothing here is the only
record of anything. Move whatever is still true into `docs/`, then delete it.
