use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{watch, RwLock};
use tracing::{error, info, warn};

use super::log::open_log_files;
use crate::config::ProcessConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Starting,
    Online,
    Stopped,
    Crashed,
}

impl fmt::Display for ProcessState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProcessState::Starting => "starting",
            ProcessState::Online => "online",
            ProcessState::Stopped => "stopped",
            ProcessState::Crashed => "crashed",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessStatus {
    pub name: String,
    pub state: ProcessState,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub started_at: Option<Instant>,
    /// From the process's config `port` field — purely informational, set
    /// once at spawn and never changes for the life of this `ManagedProcess`.
    pub port: Option<u16>,
}

/// A single supervised process: owns a background task that keeps
/// (re)spawning the configured command until told to stop, with an
/// exponential backoff between crash-restarts (mirroring pm2's default
/// autorestart behavior — none of the MyApp processes disable it).
pub struct ManagedProcess {
    status: Arc<RwLock<ProcessStatus>>,
    stop_tx: watch::Sender<bool>,
}

const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A process must stay up at least this long to be considered stable and
/// reset the backoff — otherwise a command that fails instantly (bad script,
/// missing binary) would spawn in a tight loop instead of backing off.
const MIN_STABLE_UPTIME: Duration = Duration::from_secs(2);

impl ManagedProcess {
    /// `global_env` (from `[env]` in the config) is merged underneath the
    /// process's own `env` — a key set on the process itself wins on
    /// conflict.
    pub fn spawn(mut config: ProcessConfig, global_env: &HashMap<String, String>) -> Self {
        config.env = merge_env(global_env, &config.env);

        let status = Arc::new(RwLock::new(ProcessStatus {
            name: config.name.clone(),
            state: ProcessState::Starting,
            pid: None,
            restarts: 0,
            started_at: None,
            port: config.port,
        }));
        let (stop_tx, stop_rx) = watch::channel(false);

        tokio::spawn(run_loop(config, status.clone(), stop_rx));

        Self { status, stop_tx }
    }

    /// Registers a process's config without spawning it — used for
    /// `autostart = false` processes so they still show up in `alcazar list`
    /// (as `stopped`) and can be brought up later via `alcazar start <name>`
    /// or `alcazar restart <name>`, without running a background task for
    /// something nobody has asked to start yet.
    pub fn registered(config: &ProcessConfig) -> Self {
        let status = Arc::new(RwLock::new(ProcessStatus {
            name: config.name.clone(),
            state: ProcessState::Stopped,
            pid: None,
            restarts: 0,
            started_at: None,
            port: config.port,
        }));
        let (stop_tx, _stop_rx) = watch::channel(false);
        Self { status, stop_tx }
    }

    pub async fn status(&self) -> ProcessStatus {
        self.status.read().await.clone()
    }

    /// Requests a graceful stop; does not block until it happens.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }
}

async fn run_loop(
    config: ProcessConfig,
    status: Arc<RwLock<ProcessStatus>>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if *stop_rx.borrow() {
            break;
        }

        let log_path = config.log_path();
        let (out_file, err_file) = match open_log_files(&log_path) {
            Ok(files) => files,
            Err(e) => {
                // No file handle to write a marker into — this is the one
                // failure mode that can only show up in `daemon.log`, not
                // the process's own log.
                error!(process = %config.name, error = %e, "failed to open log file");
                mark_crashed(&status).await;
                if sleep_or_stop(&mut backoff, &mut stop_rx).await {
                    break;
                }
                continue;
            }
        };

        // A second handle onto the same log file (sharing the kernel-level
        // offset with out_file/err_file) so we can write our own markers —
        // start attempt, spawn failure, exit reason — interleaved with the
        // child's actual output. Without this, a command that fails before
        // writing anything (bad path, missing binary, syntax error caught by
        // `sh` itself) leaves an empty log and a "crashed" status with no
        // clue why.
        let mut marker = out_file.try_clone().ok();
        let cwd_display = config
            .cwd
            .as_ref()
            .map(|c| c.display().to_string())
            .unwrap_or_else(|| ".".to_string());

        // Reap anything left over under this process's name — from a
        // previous daemon generation that crashed, was force-killed, or
        // simply never got to clean up (the in-memory kill on our own
        // `stop`/restart only covers processes *this* daemon spawned; it
        // can't know about ones a now-dead daemon left orphaned). Without
        // this, a stale process squatting the port survives forever across
        // `alcazar stop`/`start` cycles, since nothing ever revisits it.
        if let Some(pgid) = read_pid_file(&config.pid_path()) {
            log_marker(
                &mut marker,
                &format!("cleaning up leftover process {pgid} from a previous run"),
            );
            kill_process_group(Some(pgid));
            wait_for_pid_to_die(pgid).await;
        }

        // `run_loop` only ever runs behind `ManagedProcess::spawn`, and
        // `ProcessManager` guarantees that's only called when
        // `config.command` is `Some` (a `None` command always goes through
        // `registered()` instead, spawning no `run_loop` task at all — see
        // `ProcessManager::start_all`/`start`/`restart`).
        let command_str = config
            .command
            .as_deref()
            .expect("run_loop requires config.command to be Some");

        log_marker(
            &mut marker,
            &format!("starting `{command_str}` (cwd: {cwd_display})"),
        );

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(command_str)
            .envs(&config.env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file))
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        // Make `sh` its own process group leader so we can kill the whole
        // tree it spawns (e.g. `sh` -> `infisical` -> `yarn` -> `node`) on
        // stop/restart. Without this, `child.start_kill()` below only
        // SIGKILLs `sh` itself: it dies with no chance to forward the
        // signal, orphaning everything under it. The orphan keeps running
        // and keeps holding its port, so the next start of this same
        // process immediately fails with EADDRINUSE and crash-loops.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        info!(process = %config.name, "starting");
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                error!(process = %config.name, error = %e, "failed to spawn");
                log_marker(&mut marker, &format!("failed to spawn: {e}"));
                mark_crashed(&status).await;
                log_marker(&mut marker, &format!("retrying in {backoff:?}"));
                if sleep_or_stop(&mut backoff, &mut stop_rx).await {
                    break;
                }
                continue;
            }
        };

        let spawned_at = Instant::now();
        {
            let mut s = status.write().await;
            s.state = ProcessState::Online;
            s.pid = child.id();
            s.started_at = Some(spawned_at);
        }
        if let Some(pid) = child.id() {
            write_pid_file(&config.pid_path(), pid);
        }

        let stopped = tokio::select! {
            result = child.wait() => {
                match result {
                    Ok(exit) if exit.success() => {
                        info!(process = %config.name, "exited cleanly");
                        log_marker(&mut marker, "exited cleanly (status 0)");
                    }
                    Ok(exit) => {
                        warn!(process = %config.name, code = ?exit.code(), "exited with error");
                        log_marker(&mut marker, &format!("exited with status {:?}", exit.code()));
                    }
                    Err(e) => {
                        error!(process = %config.name, error = %e, "wait failed");
                        log_marker(&mut marker, &format!("wait() failed: {e}"));
                    }
                }
                false
            }
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    let pgid = child.id();
                    kill_process_group(pgid);
                    let _ = child.wait().await;
                    // `child.wait()` above only reaps `sh` — the daemon's
                    // direct child. Anything `sh` forked (yarn -> node ->
                    // the actual dev server) is a grandchild the daemon
                    // isn't the parent of, so it can't be wait()'d; SIGKILL
                    // was sent to the whole group, but nothing confirms it's
                    // actually gone yet. Without this, `restart` can spawn
                    // the next instance while the old grandchild is still
                    // mid-teardown and still holding its port, so the new
                    // one immediately fails with EADDRINUSE.
                    if let Some(pgid) = pgid {
                        wait_for_pid_to_die(pgid).await;
                    }
                    log_marker(&mut marker, "stopped");
                    true
                } else {
                    false
                }
            }
        };

        if stopped {
            let mut s = status.write().await;
            s.state = ProcessState::Stopped;
            s.pid = None;
            break;
        }

        {
            let mut s = status.write().await;
            s.state = ProcessState::Crashed;
            s.pid = None;
            s.restarts += 1;
        }

        if spawned_at.elapsed() >= MIN_STABLE_UPTIME {
            backoff = INITIAL_BACKOFF;
        }
        log_marker(&mut marker, &format!("retrying in {backoff:?}"));

        if sleep_or_stop(&mut backoff, &mut stop_rx).await {
            break;
        }
    }

    // Every `break` above only happens once a stop was requested, but not
    // all of them set the terminal status themselves (e.g. breaking out of
    // the top-of-loop check, or out of a backoff sleep) — do it here
    // unconditionally so `stop`/`restart` callers polling for `Stopped`
    // never wait past the actual task exit.
    let mut s = status.write().await;
    s.state = ProcessState::Stopped;
    s.pid = None;
}

/// Sends SIGKILL to the whole process group of the child we spawned with
/// `setsid` (its pid doubles as its process group id), so anything it
/// forked (e.g. `infisical`/`yarn`/`node`) dies with it instead of being
/// orphaned and left holding its port.
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

/// Reads a pid previously recorded by `write_pid_file`, if any.
fn read_pid_file(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Records the pid (== pgid, since we setsid the child) we just spawned,
/// so a future daemon generation can find and reap it if this one never
/// gets the chance to.
fn write_pid_file(path: &std::path::Path, pid: u32) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, pid.to_string());
}

/// Polls (briefly) until every process in `pid`'s process group is actually
/// gone, so the caller doesn't race the OS still releasing a port held by a
/// grandchild (e.g. a dev server under `sh -> yarn -> node`) right after the
/// kill signal is sent. Checking just `pid` itself (the group leader, `sh`)
/// isn't enough: it can already be reaped while a grandchild the daemon
/// never `wait()`s for is still mid-teardown and still bound to the port.
async fn wait_for_pid_to_die(pid: u32) {
    let pgid = -(pid as i32);
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pgid, 0) } == 0 {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Writes a `[alcazar] ...` line into the process's own log file, so
/// start/spawn-failure/exit events are visible right next to the child's own
/// output (in the dashboard's log panel and `alcazar logs`). Best-effort:
/// silently does nothing if the file handle isn't available.
fn log_marker(file: &mut Option<File>, message: &str) {
    if let Some(f) = file {
        let _ = writeln!(f, "[alcazar] {message}");
    }
}

/// Merges `[env]` underneath a process's own `env` — a key set on the
/// process itself wins on conflict. Skips cloning `global` when there's
/// nothing to merge.
fn merge_env(
    global: &HashMap<String, String>,
    own: &HashMap<String, String>,
) -> HashMap<String, String> {
    if global.is_empty() {
        return own.clone();
    }
    let mut merged = global.clone();
    merged.extend(own.clone());
    merged
}

async fn mark_crashed(status: &Arc<RwLock<ProcessStatus>>) {
    let mut s = status.write().await;
    s.state = ProcessState::Crashed;
    s.restarts += 1;
}

/// Sleeps for the current backoff (doubling it, capped), unless a stop is
/// requested first. Returns true if a stop was requested.
async fn sleep_or_stop(backoff: &mut Duration, stop_rx: &mut watch::Receiver<bool>) -> bool {
    let stopped = tokio::select! {
        _ = tokio::time::sleep(*backoff) => false,
        _ = stop_rx.changed() => *stop_rx.borrow(),
    };
    *backoff = std::cmp::min(*backoff * 2, MAX_BACKOFF);
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the real shape of a supervised command: `sh -c "..."`
    /// where the command backgrounds a grandchild and waits on it (like
    /// `infisical run -- yarn dev`, where `sh` stays alive as a real
    /// process and forks the actual listener as its child). Confirms that
    /// killing the process group `sh` was put in (via the same setsid +
    /// kill(-pid) used in run_loop) reaps the grandchild too, not just sh.
    #[tokio::test]
    async fn stop_kills_grandchild_not_just_direct_child() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 60 & echo $! > /tmp/alcazar_test_grandchild.pid; wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().expect("spawn sh wrapper");
        let sh_pid = child.id().expect("sh has a pid");

        // Give sh a moment to fork the grandchild and write its pid.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let grandchild_pid: i32 = std::fs::read_to_string("/tmp/alcazar_test_grandchild.pid")
            .expect("grandchild pid file")
            .trim()
            .parse()
            .expect("valid pid");
        let _ = std::fs::remove_file("/tmp/alcazar_test_grandchild.pid");

        // Sanity check: grandchild is alive before we kill anything.
        assert_eq!(
            unsafe { libc::kill(grandchild_pid, 0) },
            0,
            "grandchild should be alive before kill"
        );

        kill_process_group(Some(sh_pid));
        let _ = child.wait().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // kill(pid, 0) returns -1/ESRCH once the process is gone.
        let alive = unsafe { libc::kill(grandchild_pid, 0) } == 0;
        assert!(!alive, "grandchild (pid {grandchild_pid}) survived the group kill — orphan bug still present");
    }

    /// Reproduces what actually kept happening in production: a daemon
    /// dies (crash, force-kill, machine restart) without ever running our
    /// stop path, leaving its children alive with no in-memory Child handle
    /// anywhere to clean them up. A brand new daemon generation has no
    /// memory of that — the only thing that can find and reap the orphan is
    /// the pid file left on disk. Confirms `read_pid_file` +
    /// `kill_process_group` (exactly what run_loop does before every spawn)
    /// actually kills a process recorded by a previous, now-gone generation.
    #[tokio::test]
    async fn stale_pid_file_from_dead_generation_gets_reaped_before_next_spawn() {
        let pid_path = std::env::temp_dir().join(format!(
            "alcazar_test_stale_pid_{}.pid",
            std::process::id()
        ));

        // Simulate a previous generation's leftover: a real process, with
        // no Child handle anywhere referencing it (as if that daemon had
        // already exited), whose pid got persisted to disk right after it
        // was spawned.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leftover = command.spawn().expect("spawn leftover process");
        let leftover_pid = leftover.id().expect("leftover has a pid");
        write_pid_file(&pid_path, leftover_pid);

        assert_eq!(
            unsafe { libc::kill(leftover_pid as i32, 0) },
            0,
            "leftover should be alive before cleanup"
        );

        // Exactly what run_loop does at the top of every spawn attempt.
        if let Some(pgid) = read_pid_file(&pid_path) {
            kill_process_group(Some(pgid));
            wait_for_pid_to_die(pgid).await;
        }

        // In production the orphan's parent is already gone (the old
        // daemon), so launchd/init reaps it the instant it dies and
        // `kill(pid, 0)` reliably reports it gone. Here *this test* is
        // still the direct parent, so we must reap it ourselves first —
        // otherwise it's a zombie (dead, but still occupying its pid until
        // waited on), and `kill(pid, 0)` would report it as if still alive.
        let _ = leftover.wait().await;

        let alive = unsafe { libc::kill(leftover_pid as i32, 0) } == 0;
        assert!(!alive, "leftover from a previous daemon generation survived");

        let _ = std::fs::remove_file(&pid_path);
    }

    #[test]
    fn process_env_wins_over_global_env_on_conflict() {
        let mut global = HashMap::new();
        global.insert("NODE_EXTRA_CA_CERTS".to_string(), "global.pem".to_string());
        global.insert("PREFIX".to_string(), "".to_string());

        let mut own = HashMap::new();
        own.insert("NODE_EXTRA_CA_CERTS".to_string(), "own.pem".to_string());
        own.insert("PORT".to_string(), "1234".to_string());

        let merged = merge_env(&global, &own);
        assert_eq!(merged.get("NODE_EXTRA_CA_CERTS"), Some(&"own.pem".to_string()));
        assert_eq!(merged.get("PREFIX"), Some(&"".to_string()));
        assert_eq!(merged.get("PORT"), Some(&"1234".to_string()));
    }

    #[test]
    fn merge_env_with_empty_global_returns_own_unchanged() {
        let global = HashMap::new();
        let mut own = HashMap::new();
        own.insert("PORT".to_string(), "1234".to_string());

        assert_eq!(merge_env(&global, &own), own);
    }
}
