mod config;
mod daemon;
mod dashboard;
mod dns;
mod ipc;
mod privileges;
mod proxy;
mod supervisor;

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use config::Config;
use ipc::client;
use ipc::protocol::{ProcessStatusDto, Request, Response};

#[derive(Parser)]
#[command(
    name = "alcazar",
    about = "A process supervisor, reverse proxy, and local DNS in one binary"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start every configured process (backgrounds itself by default), or
    /// just one by name if it's not already running (e.g. an
    /// `autostart = false` process the daemon skipped at startup — the
    /// daemon must already be running for the single-process form).
    Start {
        name: Option<String>,
        #[arg(short, long, default_value = "alcazar.toml")]
        config: PathBuf,
        /// Stay attached to the terminal instead of daemonizing.
        #[arg(long)]
        foreground: bool,
        /// Open the dashboard in the default browser once it's up.
        #[arg(short = 'o', long)]
        open: bool,
        /// Launch the menu bar app (see menubar/) once the dashboard is up.
        /// Looks for a `menubar/` directory next to the config file.
        #[arg(short = 'm', long)]
        menubar: bool,
    },
    /// Stop one process by name, or the whole daemon if no name is given.
    Stop { name: Option<String> },
    /// Restart one process by name, or the whole daemon (stop, then start)
    /// if no name is given.
    Restart {
        name: Option<String>,
        #[arg(short, long, default_value = "alcazar.toml")]
        config: PathBuf,
        /// Stay attached to the terminal instead of daemonizing (only
        /// applies to a whole-daemon restart).
        #[arg(long)]
        foreground: bool,
        /// Open the dashboard in the default browser once it's back up
        /// (only applies to a whole-daemon restart).
        #[arg(short = 'o', long)]
        open: bool,
        /// Launch the menu bar app once the dashboard is back up (only
        /// applies to a whole-daemon restart). See `alcazar start --help`.
        #[arg(short = 'm', long)]
        menubar: bool,
    },
    /// List every process and its status.
    List,
    /// Opens the dashboard in the default browser (doesn't start the daemon
    /// if it isn't already running).
    Open {
        #[arg(short, long, default_value = "alcazar.toml")]
        config: PathBuf,
        /// Launch the menu bar app instead of opening the browser.
        #[arg(short = 'm', long)]
        menubar: bool,
    },
    /// Tail a process's log file.
    Logs {
        name: String,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Truncate one process's log file, or every process's log plus the
    /// daemon's own log if no name is given. Doesn't require the daemon to
    /// be running — it just resolves log paths from the config and empties
    /// the files directly (a process still writing to one keeps appending
    /// to it from offset 0, same as `truncate -s 0`).
    ClearLogs {
        name: Option<String>,
        #[arg(short, long, default_value = "alcazar.toml")]
        config: PathBuf,
    },
    /// One-time setup: registers /etc/resolver/<suffix> so macOS routes
    /// *.<suffix> DNS queries to alcazar (requires sudo).
    Setup {
        #[arg(short, long, default_value = "alcazar.toml")]
        config: PathBuf,
    },
    #[command(hide = true, name = "daemon-child")]
    DaemonChild {
        #[arg(long)]
        config: PathBuf,
    },
    /// Print a shell completion script to stdout — see `README.md` for how
    /// to wire it into zsh/bash/fish.
    Completions { shell: Shell },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let socket_path = default_socket_path();

    match cli.command {
        Commands::Start { name: Some(name), .. } => {
            match client::send_request(&socket_path, Request::Start { name: name.clone() }).await? {
                Response::Ok => {
                    println!("alcazar: started {name}");
                    Ok(())
                }
                Response::Error { message } => anyhow::bail!(message),
                _ => anyhow::bail!("unexpected daemon response"),
            }
        }
        Commands::Start { name: None, config, foreground, open, menubar } => {
            cmd_start(config, foreground, open, menubar, &socket_path).await
        }
        Commands::DaemonChild { config: config_path } => {
            let config = Config::load(&config_path)?;
            daemon::run(config, config_path, socket_path).await
        }
        Commands::Stop { name } => cmd_stop(name, &socket_path).await,
        Commands::Restart { name: Some(name), .. } => {
            match client::send_request(&socket_path, Request::Restart { name: name.clone() }).await? {
                Response::Ok => {
                    println!("alcazar: restarted {name}");
                    Ok(())
                }
                Response::Error { message } => anyhow::bail!(message),
                _ => anyhow::bail!("unexpected daemon response"),
            }
        }
        Commands::Restart { name: None, config, foreground, open, menubar } => {
            cmd_restart_all(config, foreground, open, menubar, &socket_path).await
        }
        Commands::List => cmd_list(&socket_path).await,
        Commands::Open { config, menubar } => cmd_open(config, menubar, &socket_path).await,
        Commands::Logs { name, lines } => cmd_logs(name, lines, &socket_path).await,
        Commands::ClearLogs { name, config } => cmd_clear_logs(name, config),
        Commands::Setup { config } => cmd_setup(config),
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "alcazar", &mut std::io::stdout());
            Ok(())
        }
    }
}

fn default_socket_path() -> PathBuf {
    config::dirs_home().join(".alcazar").join("alcazar.sock")
}

async fn cmd_start(
    config_path: PathBuf,
    foreground: bool,
    open: bool,
    menubar: bool,
    socket_path: &Path,
) -> Result<()> {
    if client::is_daemon_running(socket_path).await {
        anyhow::bail!(
            "alcazar is already running (socket at {}); stop it first with `alcazar stop`",
            socket_path.display()
        );
    }

    // Load + validate up front so config errors surface immediately, whether
    // or not we're about to daemonize.
    let config = Config::load(&config_path)?;

    if foreground {
        println!(
            "alcazar: {} process(es) starting in the foreground, Ctrl+C to stop",
            config.processes.len()
        );
        print_startup_banner(&config);
        if open && config.dashboard.enabled {
            open_in_browser(&config.dashboard.listen);
        }
        if menubar && config.dashboard.enabled {
            open_menubar(&config_path, &config.dashboard.listen);
        }
        println!();
        return daemon::run(config, config_path, socket_path.to_path_buf()).await;
    }

    daemonize(&config_path, socket_path).await?;
    print_startup_banner(&config);
    if open && config.dashboard.enabled {
        open_in_browser(&config.dashboard.listen);
    }
    if menubar && config.dashboard.enabled {
        open_menubar(&config_path, &config.dashboard.listen);
    }
    Ok(())
}

const BANNER: &str = r#"
 ██╗   ██╗██╗██╗   ██╗ █████╗     ██╗      █████╗
 ██║   ██║██║██║   ██║██╔══██╗    ██║     ██╔══██╗
 ██║   ██║██║██║   ██║███████║    ██║     ███████║
 ╚██╗ ██╔╝██║╚██╗ ██╔╝██╔══██║    ██║     ██╔══██║
  ╚████╔╝ ██║ ╚████╔╝ ██║  ██║    ███████╗██║  ██║
   ╚═══╝  ╚═╝  ╚═══╝  ╚═╝  ╚═╝    ╚══════╝╚═╝  ╚═╝

 ██████╗ ███████╗██╗   ██╗ ██████╗ ██╗     ██╗   ██╗ ██████╗██╗ ██████╗ ███╗   ██╗
 ██╔══██╗██╔════╝██║   ██║██╔═══██╗██║     ██║   ██║██╔════╝██║██╔═══██╗████╗  ██║
 ██████╔╝█████╗  ██║   ██║██║   ██║██║     ██║   ██║██║     ██║██║   ██║██╔██╗ ██║
 ██╔══██╗██╔══╝  ╚██╗ ██╔╝██║   ██║██║     ██║   ██║██║     ██║██║   ██║██║╚██╗██║
 ██║  ██║███████╗ ╚████╔╝ ╚██████╔╝███████╗╚██████╔╝╚██████╗██║╚██████╔╝██║ ╚████║
 ╚═╝  ╚═╝╚══════╝  ╚═══╝   ╚═════╝ ╚══════╝ ╚═════╝  ╚═════╝╚═╝ ╚═════╝ ╚═╝  ╚═══╝

              ✊  ¡¡¡ MUERTE A PM2 !!!  ✊
"#;

/// Printed once at the end of every successful `alcazar start` (foreground
/// or daemonized) — purely cosmetic, but the little link table under it is
/// the actually useful part: where to find the dashboard and where the
/// daemon's own log lives, so you don't have to go dig for either.
fn print_startup_banner(config: &Config) {
    println!("{BANNER}");
    let dashboard = if config.dashboard.enabled {
        format!("http://{}", config.dashboard.listen)
    } else {
        "disabled ([dashboard].enabled = false)".to_string()
    };
    println!(" dashboard   {dashboard}");
    println!(" logs        {}", config::daemon_log_path().display());
    println!();
}

/// Best-effort: opens the dashboard URL with the OS's default browser via
/// `open`(1). Never fails the `start` command itself — a browser that
/// doesn't launch isn't worth aborting a successful daemon start over.
fn open_in_browser(listen: &str) {
    // "0.0.0.0" isn't something a browser can navigate to directly on every
    // platform; the daemon itself binds it fine, but the URL we hand to
    // `open` needs an actual loopback address.
    let host_port = listen.replacen("0.0.0.0", "127.0.0.1", 1);
    let url = format!("http://{host_port}");
    if let Err(e) = std::process::Command::new("open").arg(&url).spawn() {
        eprintln!("alcazar: couldn't open browser at {url}: {e}");
    }
}

/// Best-effort: launches the Electron menu bar app from `menubar/` next to
/// the config file (see `menubar/main.js`). Its own single-instance lock
/// means calling this while it's already running just brings the existing
/// tray icon's dashboard forward rather than spawning a second one. Never
/// fails `start` itself -- same reasoning as `open_in_browser`.
fn open_menubar(config_path: &Path, listen: &str) {
    let menubar_dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("menubar");
    // The real native binary, not `node_modules/.bin/electron` -- that's a
    // `#!/usr/bin/env node` script, and resolving `node` via `PATH` at exec
    // time silently fails under `sudo`, which typically resets `PATH` to a
    // minimal `secure_path` that doesn't include an nvm-installed `node`.
    // The failure is invisible from here too: the shebang re-exec happens
    // inside the kernel's `execve()`, so `Command::spawn()` itself still
    // reports success (the fork+initial exec worked) — the process just
    // dies moments later when `env` can't find `node`, with nothing to
    // catch. Going straight for the native binary sidesteps the whole
    // problem: no interpreter, no `PATH` lookup, nothing to fail silently.
    let electron_bin =
        menubar_dir.join("node_modules/electron/dist/Electron.app/Contents/MacOS/Electron");
    if !electron_bin.exists() {
        eprintln!(
            "alcazar: --menubar was passed but no Electron install found at {} (run `npm install` there first)",
            menubar_dir.display()
        );
        return;
    }

    // `Command` resolves a relative program path *after* `current_dir`
    // applies the chdir, not before -- with a relative `electron_bin` and
    // `.current_dir(&menubar_dir)` both set, the program would be looked up
    // as `<menubar_dir>/<electron_bin>` instead of just `<electron_bin>`,
    // failing with ENOENT even though the `exists()` check above just
    // passed. Canonicalizing first sidesteps that entirely.
    let electron_bin = match electron_bin.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "alcazar: couldn't resolve menu bar app path at {}: {e}",
                electron_bin.display()
            );
            return;
        }
    };

    let host_port = listen.replacen("0.0.0.0", "127.0.0.1", 1);
    let mut command = std::process::Command::new(&electron_bin);
    command
        .arg(".")
        .current_dir(&menubar_dir)
        .env("ALCAZAR_DASHBOARD_URL", format!("http://{host_port}"));

    // `alcazar start -m`/`restart -m` under `sudo` (or a setuid-root binary)
    // runs this whole CLI process as root -- unlike the daemon, which drops
    // to the invoking user via `privileges::drop_to_invoking_user()` once it
    // has bound its privileged ports, `cmd_start` never does. A GUI process
    // spawned as root has no connection to the logged-in user's WindowServer
    // session, so Electron would start, fail to create any window/tray, and
    // exit almost immediately -- silently, with no error to report, since
    // nothing about the `spawn()` call itself failed. Drop to the invoking
    // user for this one child, the same way the daemon drops for itself.
    if let Some((uid, gid, home)) = invoking_user() {
        use std::os::unix::process::CommandExt;
        command.uid(uid).gid(gid);
        command.env("HOME", home);
    }

    match command.spawn() {
        Ok(child) => write_menubar_pid_file(child.id()),
        Err(e) => eprintln!(
            "alcazar: couldn't launch menu bar app at {}: {e}",
            menubar_dir.display()
        ),
    }
}

/// If this process is running as root (via `sudo` or a setuid-root binary),
/// returns the (uid, gid, home dir) of the actual invoking user -- same
/// `SUDO_UID`/`SUDO_GID` vs. real-uid-fallback logic as
/// `privileges::drop_to_invoking_user`, just returning the ids to apply to
/// one spawned child instead of dropping the whole process's own privileges.
/// `None` if we're not root, or if we're root with no invoking user to
/// recover (a bare root shell, a system daemon).
fn invoking_user() -> Option<(u32, u32, PathBuf)> {
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| unsafe { libc::getuid() });
    if uid == 0 {
        return None;
    }

    let mut buf = vec![0i8; 16384];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    let home = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }
        .to_string_lossy()
        .into_owned();
    Some((uid, pwd.pw_gid, PathBuf::from(home)))
}

/// Where `open_menubar` records the pid of the `electron` wrapper process it
/// spawned, so a later whole-daemon `alcazar stop` can find and kill it --
/// same `~/.alcazar/pids/` convention as `ProcessConfig::pid_path`, just for
/// a process the daemon itself never supervises.
fn menubar_pid_path() -> PathBuf {
    config::dirs_home().join(".alcazar").join("pids").join("menubar.pid")
}

fn write_menubar_pid_file(pid: u32) {
    let path = menubar_pid_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, pid.to_string());
}

/// Best-effort cleanup run on a whole-daemon `alcazar stop`: without this,
/// the menu bar app (started via `--menubar`, a plain detached process the
/// daemon has no supervision or pid-file machinery for) would keep running
/// with a tray icon pointing at a dashboard that just went away.
///
/// The recorded pid is the real `Electron.app` process (see `open_menubar`
/// -- we exec its native binary directly, no `node_modules/.bin/electron`
/// wrapper script in between), so `SIGTERM` goes straight to it. Verified
/// directly: killing that process this way cleanly tears down its GPU/
/// network/renderer helper processes along with it, the same way quitting
/// any other Chromium-based app cleans up after itself.
fn kill_menubar_if_running() {
    let path = menubar_pid_path();
    let Some(pid) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok())
    else {
        return;
    };
    if unsafe { libc::kill(pid, 0) } == 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let _ = std::fs::remove_file(&path);
}

async fn daemonize(config_path: &Path, socket_path: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("resolving current executable path")?;
    let log_path = config::daemon_log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening daemon log at {}", log_path.display()))?;
    let log_file_err = log_file.try_clone()?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon-child")
        .arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_err);

    // Detach from the current session so the daemon survives the terminal
    // closing (equivalent to what pm2's own daemon process does).
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().context("spawning alcazar daemon")?;
    let pid = child.id();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if client::is_daemon_running(socket_path).await {
            println!(
                "alcazar: daemon started (pid {pid}), logs at {}",
                log_path.display()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!(
        "daemon did not come up within 5s; check {}",
        log_path.display()
    )
}

async fn cmd_stop(name: Option<String>, socket_path: &Path) -> Result<()> {
    let request = match &name {
        Some(name) => Request::Stop { name: name.clone() },
        None => Request::StopAllAndExit,
    };
    match client::send_request(socket_path, request).await? {
        Response::Ok => {
            match name {
                Some(name) => println!("alcazar: stopped {name}"),
                None => {
                    println!("alcazar: daemon stopped");
                    kill_menubar_if_running();
                }
            }
            Ok(())
        }
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected daemon response"),
    }
}

/// Stops the whole daemon (if running) and waits for it to actually exit
/// before starting a fresh one — `StopAllAndExit` acks over IPC before the
/// daemon has finished tearing down processes and freeing its listening
/// ports (see daemon::run), so a naive stop-then-start would race it.
async fn cmd_restart_all(
    config: PathBuf,
    foreground: bool,
    open: bool,
    menubar: bool,
    socket_path: &Path,
) -> Result<()> {
    if client::is_daemon_running(socket_path).await {
        cmd_stop(None, socket_path).await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while client::is_daemon_running(socket_path).await {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("daemon did not stop within 10s; try `alcazar stop` again");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else {
        println!("alcazar: daemon wasn't running, starting fresh");
    }

    cmd_start(config, foreground, open, menubar, socket_path).await
}

/// Opens the dashboard in the default browser without touching the daemon —
/// handy after switching terminal tabs/projects when you just want the tab
/// back, not another `start`. Warns (but still opens) if the daemon doesn't
/// look up, since a stale tab is a much smaller annoyance than a silent
/// no-op that leaves you wondering why nothing happened.
async fn cmd_open(config_path: PathBuf, menubar: bool, socket_path: &Path) -> Result<()> {
    let config = Config::load(&config_path)?;
    if !config.dashboard.enabled {
        anyhow::bail!(
            "dashboard is disabled ([dashboard].enabled = false in {})",
            config_path.display()
        );
    }
    if !client::is_daemon_running(socket_path).await {
        eprintln!("alcazar: daemon doesn't look like it's running — opening anyway, but start it with `alcazar start` if the tab doesn't load");
    }
    if menubar {
        open_menubar(&config_path, &config.dashboard.listen);
        return Ok(());
    }
    println!(
        "alcazar: opening dashboard at http://{}",
        config.dashboard.listen.replacen("0.0.0.0", "127.0.0.1", 1)
    );
    open_in_browser(&config.dashboard.listen);
    Ok(())
}

async fn cmd_list(socket_path: &Path) -> Result<()> {
    match client::send_request(socket_path, Request::List).await? {
        Response::Statuses(statuses) => {
            print_statuses(&statuses);
            Ok(())
        }
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected daemon response"),
    }
}

fn print_statuses(statuses: &[ProcessStatusDto]) {
    println!(
        "{:<20} {:<10} {:<8} {:<10} {:<8} {:<6}",
        "name", "state", "pid", "uptime", "restarts", "port"
    );
    for s in statuses {
        println!(
            "{:<20} {:<10} {:<8} {:<10} {:<8} {:<6}",
            s.name,
            s.state,
            s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            s.uptime_secs
                .map(|u| format!("{u}s"))
                .unwrap_or_else(|| "-".into()),
            s.restarts,
            s.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
        );
    }
}

async fn cmd_logs(name: String, lines: usize, socket_path: &Path) -> Result<()> {
    let log_path = match client::send_request(socket_path, Request::LogPath { name: name.clone() }).await? {
        Response::Path(path) => PathBuf::from(path),
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected daemon response"),
    };

    print_tail(&log_path, lines)?;

    let mut offset = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let mut file = std::fs::File::open(&log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    loop {
        let len = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(offset);
        if len > offset {
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            print!("{}", String::from_utf8_lossy(&buf));
            offset = len;
        } else if len < offset {
            // Log was rotated/truncated; restart from the top.
            offset = 0;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

fn truncate_log(path: &Path) -> Result<()> {
    std::fs::File::create(path)
        .with_context(|| format!("truncating log file {}", path.display()))?;
    Ok(())
}

fn cmd_clear_logs(name: Option<String>, config_path: PathBuf) -> Result<()> {
    let config = Config::load(&config_path)?;

    match name {
        Some(name) => {
            let process = config
                .processes
                .iter()
                .find(|p| p.name == name)
                .with_context(|| format!("no process named {name} in {}", config_path.display()))?;
            truncate_log(&process.log_path())?;
            println!("alcazar: cleared log for {name}");
        }
        None => {
            truncate_log(&config::daemon_log_path())?;
            for process in &config.processes {
                truncate_log(&process.log_path())?;
            }
            println!(
                "alcazar: cleared daemon log and {} process log(s)",
                config.processes.len()
            );
        }
    }

    Ok(())
}

fn cmd_setup(config_path: PathBuf) -> Result<()> {
    let config = Config::load(&config_path)?;
    let suffix = config.dns.suffix.trim_matches('.').to_string();
    let resolver_dir = PathBuf::from("/etc/resolver");
    let resolver_file = resolver_dir.join(&suffix);

    println!("alcazar: registering {} with macOS's per-domain resolver (needs sudo)\n", resolver_file.display());

    let mkdir_status = std::process::Command::new("sudo")
        .args(["mkdir", "-p"])
        .arg(&resolver_dir)
        .status()
        .context("running `sudo mkdir -p /etc/resolver`")?;
    if !mkdir_status.success() {
        anyhow::bail!("`sudo mkdir -p {}` failed", resolver_dir.display());
    }

    let write_script = format!("echo 'nameserver 127.0.0.1' > {}", resolver_file.display());
    let write_status = std::process::Command::new("sudo")
        .args(["sh", "-c", &write_script])
        .status()
        .with_context(|| format!("writing {}", resolver_file.display()))?;
    if !write_status.success() {
        anyhow::bail!("writing {} failed", resolver_file.display());
    }

    println!("\nalcazar: wrote {}", resolver_file.display());
    println!(
        "alcazar: start the daemon (it needs sudo to bind DNS on port 53, e.g. \
         `sudo alcazar start`), then verify with `dig api.{suffix}` or `ping api.{suffix}`"
    );
    Ok(())
}

fn print_tail(path: &Path, lines: usize) -> Result<()> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading log file {}", path.display()))?;
    let all_lines: Vec<&str> = contents.lines().collect();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }
    Ok(())
}
