# alcazar menu bar app

A small Electron app that puts alcazar in the macOS menu bar. It never
talks to the Rust daemon directly — everything goes through the dashboard's
existing local HTTP API (`http://127.0.0.1:7777` by default), the same API
a browser tab would use.

## Setup

```bash
cd menubar
npm install
npm start
```

Or let the CLI do it: `alcazar start -m` / `alcazar restart -m` launch this
app automatically (it execs
`menubar/node_modules/electron/dist/Electron.app/Contents/MacOS/Electron`
directly next to the config file — the real native binary, not the
`node_modules/.bin/electron` npm wrapper script, see below — and passes the
real dashboard URL via `ALCAZAR_DASHBOARD_URL`). Under `sudo`, it also drops
from root to the invoking user before spawning (`invoking_user()` in
`src/main.rs`, same `SUDO_UID`/`SUDO_GID` logic as
`privileges::drop_to_invoking_user`) — a GUI process launched as root has no
connection to the logged-in user's WindowServer session and would just exit
moments after starting, with no window, no tray, and nothing printed.

## What it does

- **Tray icon** (`assets/trayIconTemplate.png` / `@2x.png`) — click to
  toggle the dashboard window; right-click for a menu listing every
  process, with a checkbox per name to pin/unpin its log window.
- **Pinned log windows** — load the dashboard's own `?pin=<name>` URL (see
  the boot code at the end of `static/dashboard.html`, which auto-opens
  that process's log full-screen when the param is present). Each one is
  `alwaysOnTop(true, 'screen-saver')` +
  `setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true })`, sized to
  the screen the cursor is on, and its bounds are persisted to
  `~/Library/Application Support/alcazar-menubar/pinned-windows.json` so
  they reopen automatically next launch.
- **`Cmd+Shift+I`** — global shortcut (works even with no alcazar window
  focused) that opens/raises the dashboard.
- **Single-instance lock** — a second launch just brings the existing
  dashboard forward instead of spawning a duplicate tray icon. This means
  editing `assets/trayIconTemplate*.png` needs the app fully quit (via the
  tray menu, or `sudo alcazar stop`) before the next launch will pick it
  up — a still-running old instance never re-reads the icon file.

## Why the native binary, not `node_modules/.bin/electron`

That `.bin/electron` shim is a `#!/usr/bin/env node` script — resolving
`node` via `PATH` at exec time silently fails under `sudo`, which typically
resets `PATH` to a minimal `secure_path` that doesn't include an
nvm-installed `node`. The failure is invisible to the Rust side too: the
shebang re-exec happens inside the kernel's `execve()`, so
`Command::spawn()` still reports success — the process just dies moments
later when `env` can't find `node`, with nothing to catch. Execing the
native `Electron.app` binary directly sidesteps the whole problem: no
interpreter, no `PATH` lookup, nothing to fail silently.

## Being killed cleanly

`alcazar stop` (whole-daemon stop, no process name) also kills this app if
it was started via `-m`: `open_menubar` in `src/main.rs` records the pid of
the real `Electron.app` process (the one it just spawned directly — see
above) to `~/.alcazar/pids/menubar.pid`, and `kill_menubar_if_running` sends
it `SIGTERM` on stop. Verified directly: that alone cleanly tears down its
GPU/network/renderer helper processes too, the same way quitting any other
Chromium-based app cleans up after itself.
