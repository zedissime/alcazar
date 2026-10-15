pub mod log;
pub mod process;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::config::ProcessConfig;
use process::{ManagedProcess, ProcessState, ProcessStatus};

struct Entry {
    config: ProcessConfig,
    process: ManagedProcess,
}

/// Owns every supervised process. Config and running-process state live in
/// the same locked map so a process can be added at runtime (via the
/// dashboard's "add service" form) without restarting the daemon.
pub struct ProcessManager {
    entries: RwLock<HashMap<String, Entry>>,
    /// From `[env]` in the config; merged underneath every process's own
    /// `env` whenever it's (re)spawned, including processes added later at
    /// runtime.
    global_env: HashMap<String, String>,
}

impl ProcessManager {
    pub fn start_all(configs: Vec<ProcessConfig>, global_env: HashMap<String, String>) -> Self {
        let mut entries = HashMap::with_capacity(configs.len());
        for config in configs {
            // A process with no `command` (a Kamal-only accessory) has
            // nothing to spawn, so it's always just registered — same as
            // `autostart = false` — regardless of what `autostart` says.
            let process = if config.command.is_some() && config.autostart {
                ManagedProcess::spawn(config.clone(), &global_env)
            } else {
                ManagedProcess::registered(&config)
            };
            entries.insert(config.name.clone(), Entry { config, process });
        }
        Self {
            entries: RwLock::new(entries),
            global_env,
        }
    }

    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    pub async fn log_path(&self, name: &str) -> Option<PathBuf> {
        self.entries.read().await.get(name).map(|e| e.config.log_path())
    }

    /// The process's working directory, e.g. for locating its Kamal config
    /// when fetching remote logs. `None` if the process is unknown or has
    /// no `cwd` set.
    pub async fn cwd(&self, name: &str) -> Option<PathBuf> {
        self.entries.read().await.get(name).and_then(|e| e.config.cwd.clone())
    }

    pub async fn statuses(&self) -> Vec<ProcessStatus> {
        let entries = self.entries.read().await;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries.values() {
            out.push(e.process.status().await);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Snapshot of every process's config (name/cwd/command/env), e.g. for
    /// the dashboard's process table and its read-only config view.
    pub async fn configs_snapshot(&self) -> Vec<ProcessConfig> {
        let entries = self.entries.read().await;
        let mut out: Vec<_> = entries.values().map(|e| e.config.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Requests a graceful stop of a single process by name.
    pub async fn stop(&self, name: &str) -> bool {
        let entries = self.entries.read().await;
        match entries.get(name) {
            Some(e) => {
                e.process.stop();
                true
            }
            None => false,
        }
    }

    /// Stops every *local* process — a Kamal-only accessory (no `command`)
    /// has nothing running here to stop, so it's skipped rather than sent
    /// a no-op stop signal, keeping "Stop all" unambiguously scoped to what
    /// alcazar actually supervises.
    pub async fn stop_all(&self) {
        let entries = self.entries.read().await;
        let names: Vec<String> = entries
            .iter()
            .filter(|(_, e)| e.config.command.is_some())
            .map(|(name, e)| {
                e.process.stop();
                name.clone()
            })
            .collect();
        drop(entries);
        for name in names {
            self.wait_until_stopped(&name).await;
        }
    }

    /// Starts a process that isn't currently running — most commonly one
    /// with `autostart = false` in the config that was only ever
    /// registered, never spawned. A no-op (still returns true) if it's
    /// already online or in the middle of (re)starting. Returns false if
    /// `name` isn't a known process, or if it has no `command` (a
    /// Kamal-only accessory alcazar never controls locally).
    pub async fn start(&self, name: &str) -> bool {
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get(name) else {
            return false;
        };
        if entry.config.command.is_none() {
            return false;
        }
        if !matches!(entry.process.status().await.state, ProcessState::Stopped) {
            return true;
        }
        let config = entry.config.clone();
        let process = ManagedProcess::spawn(config.clone(), &self.global_env);
        entries.insert(name.to_string(), Entry { config, process });
        true
    }

    /// Stops the current instance (if any) and spawns a fresh one from the
    /// original config. Returns false if `name` isn't a known process, or
    /// if it has no `command` (a Kamal-only accessory alcazar never
    /// controls locally).
    pub async fn restart(&self, name: &str) -> bool {
        {
            let entries = self.entries.read().await;
            let Some(e) = entries.get(name) else {
                return false;
            };
            if e.config.command.is_none() {
                return false;
            }
            e.process.stop();
        }
        self.wait_until_stopped(name).await;

        let mut entries = self.entries.write().await;
        let Some(config) = entries.get(name).map(|e| e.config.clone()) else {
            return false;
        };
        let process = ManagedProcess::spawn(config.clone(), &self.global_env);
        entries.insert(name.to_string(), Entry { config, process });
        true
    }

    /// Restarts every *local* process (the dashboard's "restart all"
    /// button). `restart()` already refuses on a Kamal-only accessory (no
    /// `command`), but filtering here too keeps that scoping explicit at
    /// the call site rather than relying on a side effect of `restart`'s
    /// own guard.
    pub async fn restart_all(&self) {
        let names: Vec<String> = self
            .entries
            .read()
            .await
            .iter()
            .filter(|(_, e)| e.config.command.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            self.restart(&name).await;
        }
    }

    /// Adds and spawns a brand-new process. Returns false if the name is
    /// already taken.
    pub async fn add(&self, config: ProcessConfig) -> bool {
        let mut entries = self.entries.write().await;
        if entries.contains_key(&config.name) {
            return false;
        }
        let process = ManagedProcess::spawn(config.clone(), &self.global_env);
        entries.insert(config.name.clone(), Entry { config, process });
        true
    }

    async fn wait_until_stopped(&self, name: &str) {
        const MAX_WAIT: Duration = Duration::from_secs(10);
        const POLL_INTERVAL: Duration = Duration::from_millis(50);

        let deadline = tokio::time::Instant::now() + MAX_WAIT;
        loop {
            let stopped = {
                let entries = self.entries.read().await;
                match entries.get(name) {
                    Some(e) => matches!(e.process.status().await.state, ProcessState::Stopped),
                    None => true,
                }
            };
            if stopped || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_config(name: &str, autostart: bool) -> ProcessConfig {
        ProcessConfig {
            name: name.to_string(),
            cwd: None,
            command: Some("sleep 60".to_string()),
            env: HashMap::new(),
            log: None,
            port: None,
            autostart,
        }
    }

    /// A process with `autostart = false` must be registered (so it shows
    /// up in `alcazar list`/the dashboard) but not actually spawned until
    /// something explicitly starts it — otherwise the option does nothing.
    #[tokio::test]
    async fn autostart_false_registers_without_spawning() {
        let manager = ProcessManager::start_all(
            vec![process_config("on-demand", false)],
            HashMap::new(),
        );

        let statuses = manager.statuses().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ProcessState::Stopped);
        assert!(statuses[0].pid.is_none());

        manager.stop_all().await;
    }

    /// `start()` on a registered-but-never-spawned process actually spawns
    /// it — this is what `alcazar start <name>` relies on to bring up a
    /// process that was skipped at daemon startup because of
    /// `autostart = false`.
    #[tokio::test]
    async fn start_spawns_a_registered_but_never_started_process() {
        let manager = ProcessManager::start_all(
            vec![process_config("on-demand", false)],
            HashMap::new(),
        );

        assert!(manager.start("on-demand").await);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let statuses = manager.statuses().await;
            if statuses[0].state == ProcessState::Online {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "process never came online");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        manager.stop_all().await;
    }

    #[tokio::test]
    async fn start_returns_false_for_unknown_process() {
        let manager = ProcessManager::start_all(Vec::new(), HashMap::new());
        assert!(!manager.start("nope").await);
    }

    fn remote_only_config(name: &str) -> ProcessConfig {
        ProcessConfig {
            name: name.to_string(),
            cwd: None,
            command: None,
            env: HashMap::new(),
            log: None,
            port: None,
            autostart: true,
        }
    }

    /// A process with no `command` (a Kamal-only accessory) must be
    /// registered like `autostart = false` regardless of what `autostart`
    /// says — there's nothing to spawn — and `start`/`restart` must refuse
    /// it rather than exec an empty command.
    #[tokio::test]
    async fn command_less_process_is_registered_and_never_startable() {
        let manager = ProcessManager::start_all(
            vec![remote_only_config("registry")],
            HashMap::new(),
        );

        let statuses = manager.statuses().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ProcessState::Stopped);
        assert!(statuses[0].pid.is_none());

        assert!(!manager.start("registry").await);
        assert!(!manager.restart("registry").await);
        assert_eq!(manager.statuses().await[0].state, ProcessState::Stopped);

        manager.stop_all().await;
    }

    /// "Restart all" / "Stop all" must stay scoped to local processes: a
    /// Kamal-only accessory has nothing running here to restart or stop,
    /// so mixing one into the fleet must not change how (or whether) the
    /// local process actually restarts.
    #[tokio::test]
    async fn restart_all_and_stop_all_skip_command_less_processes() {
        let manager = ProcessManager::start_all(
            vec![process_config("local", true), remote_only_config("registry")],
            HashMap::new(),
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let statuses = manager.statuses().await;
            let local = statuses.iter().find(|s| s.name == "local").unwrap();
            if local.state == ProcessState::Online {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "local process never came online");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        manager.restart_all().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let statuses = manager.statuses().await;
            let local = statuses.iter().find(|s| s.name == "local").unwrap();
            if local.state == ProcessState::Online {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "local process never came back online");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let statuses = manager.statuses().await;
        let registry = statuses.iter().find(|s| s.name == "registry").unwrap();
        assert_eq!(registry.state, ProcessState::Stopped);
        assert!(registry.pid.is_none());

        manager.stop_all().await;
        let statuses = manager.statuses().await;
        let local = statuses.iter().find(|s| s.name == "local").unwrap();
        let registry = statuses.iter().find(|s| s.name == "registry").unwrap();
        assert_eq!(local.state, ProcessState::Stopped);
        assert_eq!(registry.state, ProcessState::Stopped);
    }
}
