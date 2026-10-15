# alcazar

A single Rust binary that replaces pm2 + Traefik + dnsmasq for a local dev
stack:

- **process supervisor**: restart on crash, log files, orphan cleanup
- **HTTPS reverse proxy**: Host-based routing, WebSocket passthrough
- **local DNS** for `*.test` (or any suffix)
- **web dashboard** and an optional **macOS menu bar app**

One config file (`alcazar.toml`), one CLI.

## Install

Requires Rust ([rustup](https://rustup.rs)).

```
cargo build --release
sudo cp target/release/alcazar /usr/local/bin/alcazar
```

Install it somewhere on `sudo`'s `PATH`: the daemon binds `:443` and `:53`,
so it is started with `sudo alcazar start`.

## Configure

```
cp alcazar.example.toml alcazar.toml   # gitignored: keep secrets here, not in git
alcazar setup --config alcazar.toml    # one-time: registers /etc/resolver/<suffix> (sudo)
```

Trust the CA that signed your `tls_cert`:

```
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain ssl/devCA.pem
```

`alcazar.example.toml` documents every section: `[dns]`, `[proxy]`,
`[dashboard]`, `[env]`, `[[process]]`, `[[route]]`. Notes:

- `~` is expanded in `cwd`, `log`, `tls_cert` and `tls_key`.
- Commands run via `sh -c`, which does **not** source `~/.zshrc`. Put needed
  env vars in `[env]` (shared) or a process's own `env`; the process wins.
- `autostart = false` registers a process without spawning it. Start it later
  with `alcazar start <name>`.
- A `[[process]]` with no `command` is a **Kamal-only accessory**: nothing runs
  locally, but the dashboard still offers Kamal logs and a reboot button.

## Usage

| command | what it does |
|---|---|
| `alcazar start [-o] [-m]` | start the daemon and every `autostart` process. `-o` opens the dashboard, `-m` launches the menu bar app. `--foreground` stays attached |
| `alcazar start <name>` | start one process (daemon must be running) |
| `alcazar stop [<name>]` | stop one process, or everything including the daemon |
| `alcazar restart [<name>] [-o] [-m]` | restart one process, or the whole daemon |
| `alcazar list` | status table (state, pid, uptime, restarts) |
| `alcazar logs <name>` | tail a process's log |
| `alcazar clear-logs [<name>]` | truncate one or all log files |
| `alcazar open [-m]` | open the dashboard, or with `-m` only the menu bar app |

Logs live in `~/.alcazar/logs/<name>.log`, the daemon log in
`~/.alcazar/daemon.log`, the control socket in `~/.alcazar/alcazar.sock`.

## Privileges

Root is only needed to bind `:443`/`:53`. Right after, the daemon drops to
the user who ran `sudo`, and every supervised process runs as that user.

To skip `sudo` entirely, make the binary setuid-root:

```
sudo chown root:wheel $(which alcazar) && sudo chmod u+s $(which alcazar)
```

Reapply after every reinstall. This is a real security trade-off on a shared
machine; see `CLAUDE.md`.

## Dashboard

`http://127.0.0.1:7777` (configurable or disabled in `[dashboard]`):

- live process table with start/restart/stop, "Restart all" / "Stop all"
- log drawer per process: live streaming, ANSI colors, JSON pretty-printing,
  filter, fullscreen
- Kamal tabs per deploy destination (`config/deploy*.yml`) and a confirmed
  "reboot" button per destination. Needs `kamal` and SSH access on the
  daemon's `PATH`
- "Add service" form: hot-spawns the process and appends it to `alcazar.toml`

Routes, DNS and proxy settings are read-only; editing them needs a restart.

## Menu bar app

`menubar/` is a separate Electron app: tray icon toggling the dashboard, plus
per-process log windows that can be pinned always-on-top. `Cmd+Shift+I` opens
the dashboard from anywhere. Run `npm install` in `menubar/` first, then use
`-m`. See `menubar/README.md`.

## Not implemented

Cluster mode, HTTP/2 to the browser, log rotation.

## Tests

```
cargo test
```
