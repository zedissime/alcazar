# alcazar

See README.md for what this is and how to use it.

## Process supervision internals (non-obvious, learned the hard way)

Every supervised command is spawned as `sh -c "<command>"`. Two things about
how that's killed matter enough to be worth flagging before touching
`src/supervisor/process.rs`:

1. **Group kill, not direct-child kill.** The spawned `sh` is put in its own
   process group via `setsid()` in `pre_exec`, and stopping/restarting sends
   `SIGKILL` to `-pgid`, not just the `sh` pid. A plain `child.start_kill()`
   only kills `sh`: wrapper commands like `infisical run -- yarn dev` die
   with no chance to forward the signal to what they forked, orphaning it —
   still running, still holding its port, causing the next start to fail
   with `EADDRINUSE` and crash-loop forever.

2. **Pid file survives across daemon generations.** Killing the process
   group only helps if *this* daemon's in-memory `Child` handle still
   exists. If the daemon itself dies uncleanly (crash, `kill -9`, machine
   restart) before stopping a child, nothing in memory remembers that child
   ever existed, and it's orphaned with no way for the next daemon to find
   it. Each process's pid is persisted to `~/.alcazar/pids/<name>.pid`
   before every spawn attempt and checked (and killed if still alive)
   before the next one — this is what actually makes restarts idempotent
   across daemon crashes, not just within one daemon's lifetime.

Both are covered by tests in `src/supervisor/process.rs` — see
`stop_kills_grandchild_not_just_direct_child` and
`stale_pid_file_from_dead_generation_gets_reaped_before_next_spawn`. If you
change the spawn/kill path, keep both passing (or extend them) since the
failure mode they catch doesn't show up until hours of real usage, not on
the first restart.

3. **The `stop` path must wait for the whole group to die, not just `sh`.**
   `child.wait()` only reaps the daemon's direct child (`sh`); grandchildren
   (`yarn` -> `node` -> the actual dev server) aren't the daemon's children,
   so it can never `wait()` on them directly. After `kill_process_group()`,
   `wait_for_pid_to_die()` polls `kill(-pgid, 0)` — signal 0 against the
   *group*, not the leader's own pid — until nothing in it answers, before
   the state is marked `Stopped` and a `restart` is allowed to spawn the
   next instance. Without this, `restart`/`stop` can return "stopped" while
   a grandchild is still mid-teardown and still holding its port, so the
   very next spawn immediately fails with `EADDRINUSE`.

4. **Group-kill doesn't reach a process that detaches its own process
   group.** Some dev-server tooling (seen with Docusaurus/webpack-dev-server
   under `yarn start`) forks a child that ends up in a *different* pgid than
   the `sh` that spawned it — SIGKILL to `-pgid` can't touch it no matter how
   correct the daemon-side logic is, since the escaped process is no longer
   a member of that group at signal-delivery time. `compiler`,
   `gateway`, and `docs` in `alcazar.toml` work around this at the config
   level: their `command` is prefixed with `lsof -i :<port> | awk '$1 ==
   "node" {print $2}' | while read pid; do kill -9 $pid; done;` before the
   real start command, killing by *port* instead of by process tree. If a
   process in `alcazar.toml` keeps crash-looping with "address already in
   use" despite everything above, check whether the offending pid's `pgid`
   (via `ps -eo pid,ppid,pgid,command`) actually matches the pid recorded in
   `~/.alcazar/pids/<name>.pid` — a mismatch means it's escaped its group and
   needs this same kill-by-port workaround, not a fix to the group-kill code.

## Privilege drop after binding

`daemon::run` binds `:443` (proxy) and `:53` (DNS) *first*, while still
root, then calls `privileges::drop_to_invoking_user()` before doing
anything else — including `ProcessManager::start_all()`. From that point on,
the daemon and every process it spawns run as the user that invoked `sudo`
(read from `SUDO_UID`/`SUDO_GID`/`SUDO_USER`), not root.

This matters because every supervised process used to run as root, which
broke anything relying on OS user identity: Postgres peer-auth over a unix
socket authenticated as role `root` instead of the real user, and anything
a process wrote to disk (`node_modules`, build output, `~/infisical-keyring`)
ended up root-owned — which then breaks *again* the moment you fix this,
since those pre-existing root-owned files/dirs need a one-time `sudo chown`
back to yourself before a now-unprivileged process can touch them.

`drop_to_invoking_user()` also `chown`s `~/.alcazar` (logs/pids/socket) back
to the invoking user before dropping, since every prior run had created it
root-owned (mode 755 — group membership alone doesn't grant write access).
It does **not** know about state directories created by tools *other* than
alcazar itself (e.g. `~/infisical-keyring`) — those need the same one-time
manual `chown` if a process that used them under the old root-everything
daemon starts failing with `permission denied` after upgrading.

If `SUDO_UID` isn't set, `drop_to_invoking_user()` falls back to the real
uid (`getuid()`) rather than giving up immediately — that's what makes a
**setuid-root binary** work as the no-`sudo`-at-all alternative to `sudo
alcazar start`: `chmod u+s` (owned by `root:wheel`) forces the *effective*
uid to 0 on exec, but the *real* uid stays the invoking user's, exactly like
`SUDO_UID` does for the `sudo` path — just recovered a different way, since
there's no `SUDO_USER` env var to read a username off directly (see
`username_for_uid`, which uses `getpwuid_r`, not `getpwuid`, since the
non-reentrant version isn't safe to call from inside a multi-threaded tokio
runtime). Only when *both* the effective and real uid are 0 — a genuine root
shell, or a system daemon with no invoking user to speak of — does the drop
get skipped with a warning, keeping the daemon running as root rather than
refusing to start.

**The setuid bit does not survive `cargo install --path .`** — that writes
a fresh, non-setuid, non-root-owned file, silently turning `alcazar start`
back into "needs `sudo`" (or a confusing permission-denied bind failure, if
run without it) until it's reapplied:
```
sudo chown root:wheel ~/.cargo/bin/alcazar && sudo chmod u+s ~/.cargo/bin/alcazar
```
Worth an alias if you reinstall often. This is a real security trade-off,
not just a convenience toggle — a setuid-root binary that shells out
(`sh -c "<command>"` per process, `kamal` for remote logs/reboot) is a
meaningfully bigger attack surface than needing a password prompt each
time, and unlike `sudo`, it grants that brief root window to *any* local
account, not just ones in `sudoers`. Fine for a single-user dev laptop;
think twice on a shared machine.

Splitting bind from serve (`proxy::bind`/`proxy::serve`,
`dns::bind`/`dns::serve`) is what makes the ordering possible — the listener
is bound before the drop, then handed into the accept/query loop that runs
after it. A bind failure (e.g. `:53` already held by another process) is
fatal to `daemon::run` by design, not logged-and-ignored: a daemon that
silently limped along without DNS is exactly the kind of failure mode this
project is trying to eliminate. Known past offender: a system-level
`dnsmasq` `LaunchDaemon` left over from the pre-alcazar traefik-based setup
squatting `127.0.0.1:53` — `sudo launchctl list | grep dnsmasq` and
`/Library/LaunchDaemons/homebrew.mxcl.dnsmasq.plist` are where to look if
this happens again.

## Root vs. user environment

Even with the privilege drop above, `sh -c "<command>"` is never an
interactive shell, so it never sources `~/.zshrc` regardless of which user
runs it. Any env var a process needs (e.g. `GITHUB_TOKEN` for a scoped npm
registry) that only lives in a shell profile will silently be missing —
put it in `alcazar.toml`'s per-process or top-level `[env]` instead of
relying on shell inheritance.

## `command` is optional: Kamal-only accessories with nothing to run locally

`ProcessConfig::command` (`src/config.rs`) is `Option<String>`, not
`String`. A `[[process]]` block with no `command` at all — just `name` and
a `cwd` pointing at a Kamal-deployed repo — is valid, for accessories
(`~/Dev/infra/accessories/*`) that are only ever deployed remotely and
were never meant to run on this machine: alcazar can't spawn "nothing", so
there's no start/stop/restart for these, but they still get everything
`kamal_environments()`-driven — Kamal log tabs and the per-env "reboot"
sub-row (`kamal app boot [-d <env>]`) — since both only need `cwd`, not a
running local process.

This reuses, rather than adds to, the `autostart = false` machinery
described below: `ProcessManager::start_all` treats `command.is_none()` the
same as `autostart = false` (always `ManagedProcess::registered()`,
regardless of what `autostart` itself says — a command-less process has
nothing to spawn no matter that setting), and `start`/`restart` both refuse
outright on such a process instead of trying to `sh -c ""` it. If you touch
either code path, keep the `command_less_process_is_registered_and_never_startable`
test in `src/supervisor/mod.rs` passing alongside the `autostart = false`
ones.

`ProcessManager::stop_all`/`restart_all` (the dashboard's "Stop all"/
"Restart all") also filter on `command.is_some()` explicitly, rather than
relying on `stop()`/`restart()`'s own per-process guards as the only thing
keeping a command-less process untouched — so "Stop all"/"Restart all" stay
scoped to local processes even if those per-process guards ever change.
See `restart_all_and_stop_all_skip_command_less_processes`.

Every Kamal "reboot" button (`attachRebootControls` in
`static/dashboard.html`) asks for a browser `confirm()` before firing —
unlike the local start/restart/stop buttons, it's never a local, easily
undone action: it always runs `kamal app boot` against a real remote
server, immediately, with no daemon-side undo.

The dashboard (`static/dashboard.html`) renders these in a completely
separate "Kamal accessories" table (`renderKamalRows`/`#kamalSection`), not
as rows in the local-process table with start/stop/restart hidden and
columns full of "–" — that was the original implementation and is why the
section read as broken rather than just different: pid/port/uptime/
restarts/command don't apply to something alcazar never runs, so a
differently-shaped table (one row per Kamal deploy destination: name, env,
reboot) is what actually distinguishes the two categories instead of just
degrading the same one. `renderProcesses`/`renderKamalRows` both filter
`/api/processes` on `command == null`, so a process render in exactly one
of the two tables, never both, never neither. A process that *is* run
locally **and** happens to be Kamal-deployed (has both a `command` and a
`config/deploy.yml`) still only shows up in the local table, with its
per-env reboot sub-rows nested under it (`kamalEnvRow`) — the Kamal-only
table is strictly for `command == null`.

Clicking any row (local, its Kamal sub-row, or a row in the accessories
table) opens the same log panel via `openLogPanel(name, source?)` — a
slide-over drawer from the right (`#logPanel`, fixed + `transform:
translateX`), not the old inline panel below the table. `source` is
explicit when the click already identifies one env (an accessories-table
row or a sub-row); omitted, it defaults to "Local" for a process with a
`command`, or the first Kamal env for one without (there's no local log to
default to). "fullscreen" widens the same drawer to the full viewport
rather than switching to a different layout — keep it that way if you
touch the CSS; a `#logBackdrop` click or Escape closes it via the shared
`closeLogPanel()`.

## `autostart = false` processes are registered, not just skipped

`ProcessConfig.autostart` (default `true`) lets a `[[process]]` be declared
in the config without `alcazar start` spawning it. This is **not**
implemented by leaving the process out of `ProcessManager`'s map — it's
still inserted, via `ManagedProcess::registered()`, which sets status to
`Stopped` with no pid and, critically, spawns no `run_loop` background task
(unlike `ManagedProcess::spawn()`). This is what makes it show up correctly
in `alcazar list`/the dashboard and stay startable later.

Because of this, `ProcessManager::restart(name)` already works to bring up
a never-started `autostart = false` process with zero special-casing: it
calls `stop()` (a no-op send on an unwatched channel), waits for `Stopped`
(already true), then spawns fresh — so `alcazar restart <name>` and the new
`alcazar start <name>` (`ProcessManager::start`, `src/supervisor/mod.rs`)
both end up doing the same `ManagedProcess::spawn()` call. If you change how
"already running" is detected in either path, keep both behaviors — and the
`autostart_false_registers_without_spawning` /
`start_spawns_a_registered_but_never_started_process` tests in
`src/supervisor/mod.rs` — in sync.

## Remote (Kamal) log endpoints run a different subprocess pattern

`src/dashboard/api.rs`'s `api_log_tail_remote`/`api_log_stream_remote`
shell out to `kamal app logs [-d <env>] [--follow]` in the process's own
`cwd` — this is not the file-tail pattern the local `/api/logs/*` endpoints
use (`tokio::fs` polling an offset), it's a genuine subprocess spawned by
the *daemon itself*, not something forwarded to a supervised child. Two
things follow from that:

1. **The daemon's own `PATH`/SSH environment matters here, separately from
   the "root vs. user environment" issue above.** Every supervised
   process's env comes from `alcazar.toml`'s `[env]`, but `kamal` is
   invoked directly by the daemon process with whatever `PATH` (and SSH
   agent socket / loaded keys) was present when `alcazar start` itself was
   run. A daemon started via `sudo alcazar start` from a shell without
   rbenv shims on `PATH` will fail to find `kamal` at all. There's no
   config knob for this today — a spawn failure just surfaces as one
   `[alcazar] failed to run kamal app logs: ...` line in the log panel
   instead of hanging.
2. **The live-tail (`--follow`) case relies on `kill_on_drop` to end the
   SSH session.** The spawned `Child` is moved into (and kept alive by)
   the same `tokio::spawn`ed task that reads its stdout/stderr lines and
   forwards them over SSE; the loop only exits when the process exits or
   `tx.send` fails (client disconnected), and dropping the `Child` at that
   point is what actually kills the underlying `kamal app logs --follow`
   SSH session. If you refactor this, keep the `Child` in scope for the
   whole read loop — moving it out early would leak an SSH session per
   abandoned dashboard tab, same failure shape as the direct-child-only
   kill bug described at the top of this file, just for a different
   subprocess.

`ProcessConfig::kamal_environments()` (`src/config.rs`) sorts the detected
destinations with `environment_rank()`, not plain alphabetical order:
staging → validation → production, then anything else alphabetically. This
is deliberate — it's meant to read as a deploy pipeline in the dashboard's
tab strip — so don't "simplify" it back to `envs.sort()`; that would put
"production" before "staging" and "validation".

`api_kamal_boot` (`POST /api/processes/<name>/kamal/<env>/boot`, same file)
reuses the same `kamal_environments()` list to render a "reboot" button per
destination as a sub-row under each Kamal-deployed process, and shells out
`kamal app boot [-d <env>]` the same way the log endpoints do — same
daemon-`PATH`/SSH caveat from point 1 above applies. Unlike the `--follow`
log stream, this is a one-shot command awaited with `wait_with_output()`
under a 120s timeout, so there's no `kill_on_drop`/dangling-SSH-session
concern here: the request just doesn't resolve until `kamal` exits or the
timeout fires.

## The dashboard HTML is compiled into the binary, not read at runtime

`src/dashboard/api.rs` serves `static/dashboard.html` via
`include_str!("../../static/dashboard.html")` — a compile-time embed, not a
file read on each request. Editing `static/dashboard.html` on disk has zero
effect on a running daemon until `cargo build`/`cargo install --path .` and
a daemon restart (`sudo alcazar restart`). This bit us directly while
building the `?pin=` mode below: the edit was correct on disk but silently
inert for an entire debugging session because nothing about the running
process had changed.

## Menu bar app (`menubar/`): a separate Electron project, not part of the Rust binary

`menubar/` is its own `package.json`/Electron app that puts alcazar in the
macOS menu bar — a tray icon toggling the dashboard, plus per-process log
windows that can be "pinned" (always-on-top, screen-filling, visible even
over a full-screen app). It talks to the daemon exclusively through the
dashboard's existing local HTTP API (`http://127.0.0.1:7777`), never
touching the Rust process or its IPC socket directly. See
`menubar/README.md` for the app's own internals.

Three things worth knowing if you touch the integration between the two:

1. **Pinned windows reuse the dashboard's own log-rendering code instead of
   reimplementing SSE handling in Electron.** A pinned window just navigates
   to `/?pin=<name>`; a few lines at the very end of `static/dashboard.html`
   (after `refreshProcesses()`) detect that query param, call the existing
   `openLogPanel(name)`, and add the `fullscreen` CSS class — the same code
   path a manual click already exercises. Don't duplicate `EventSource`/tail
   logic in `menubar/main.js` for this; extend the dashboard's own
   `openLogPanel`/`loadLogSource` instead, same as any other log-panel
   feature.
2. **The menu bar app's own single-instance lock
   (`app.requestSingleInstanceLock()`) means a still-running old instance
   silently absorbs a new launch** (`second-instance` just raises the
   existing dashboard) **instead of ever re-executing its startup code** —
   including re-reading `assets/trayIconTemplate*.png`. Concretely: editing
   the tray icon and then just running `alcazar restart -m` again does
   nothing visible if an old instance is still alive, because the new
   process quits immediately without recreating the `Tray`. The app needs
   to actually be gone first (see point 3) — this is not a caching bug in
   the image loading, there's no image loading happening at all on that
   second launch.
3. **`alcazar stop` (whole-daemon, no process name) also kills the menu bar
   app, but by design not by tracking `Electron.app`'s own pid.**
   `open_menubar` (`src/main.rs`) spawns
   `menubar/node_modules/.bin/electron`, which is a *Node wrapper* (see the
   `electron` npm package's `cli.js`) that itself `child_process.spawn`s the
   real `Electron.app` binary and forwards `SIGTERM`/`SIGINT` to it. That
   wrapper's pid — not the grandchild `Electron.app` pid — is what gets
   recorded to `~/.alcazar/pids/menubar.pid` and signaled by
   `kill_menubar_if_running`. Sending `SIGTERM` to the wrapper lets it
   relay the signal to the real Electron process, whose own parent-death
   handling for its GPU/network/renderer helpers tears the rest down —
   verified directly (`kill -TERM <wrapper pid>` cleanly took out the whole
   process tree, helpers included). Tracking the grandchild pid instead
   would be the same class of mistake as the direct-child-only kill bug
   described at the top of this file, just one level removed.

4. **`alcazar open -m` launches only the menu bar app.** `cmd_open` takes a
   `menubar` flag and calls the same `open_menubar` as `start -m`/`restart -m`
   (same sudo/setuid user-drop, same single-instance-lock caveat from point 2)
   instead of `open_in_browser`. It never starts or touches the daemon.
