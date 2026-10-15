use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use toml_edit::{value, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// Top-level `alcazar.toml` configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub dns: DnsConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// Environment variables applied to every process, merged underneath
    /// each `[[process]]`'s own `env` (a process's own value for a key wins
    /// on conflict). Not hot-reloadable — only the process list is.
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, rename = "process")]
    pub processes: Vec<ProcessConfig>,
    #[serde(default, rename = "route")]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardConfig {
    #[serde(default = "default_dashboard_enabled")]
    pub enabled: bool,
    #[serde(default = "default_dashboard_listen")]
    pub listen: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: default_dashboard_enabled(),
            listen: default_dashboard_listen(),
        }
    }
}

fn default_dashboard_enabled() -> bool {
    true
}

fn default_dashboard_listen() -> String {
    "127.0.0.1:7777".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DnsConfig {
    /// Domain suffix resolved to 127.0.0.1, e.g. "test" for `*.test`.
    #[serde(default = "default_dns_suffix")]
    pub suffix: String,
    #[serde(default = "default_dns_listen")]
    pub listen: String,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            suffix: default_dns_suffix(),
            listen: default_dns_listen(),
        }
    }
}

fn default_dns_suffix() -> String {
    "test".to_string()
}

fn default_dns_listen() -> String {
    "127.0.0.1:53".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_listen")]
    pub listen: String,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: default_proxy_listen(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

fn default_proxy_listen() -> String {
    "0.0.0.0:443".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProcessConfig {
    pub name: String,
    pub cwd: Option<PathBuf>,
    /// The shell command to supervise. `None` marks a process alcazar never
    /// spawns or controls locally — for an accessory that's only ever
    /// deployed remotely via Kamal (see `kamal_environments()`) and you
    /// just want its logs/status/reboot surfaced in the dashboard next to
    /// everything else. Such a process is always registered the same way
    /// an `autostart = false` one is (see `ProcessManager::start_all`),
    /// regardless of `autostart`'s value, since there's nothing to spawn.
    pub command: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Defaults to ~/.alcazar/logs/<name>.log if omitted.
    pub log: Option<PathBuf>,
    /// The port this process actually listens on, purely for display in
    /// `alcazar list` and the dashboard — not enforced or used for routing
    /// (routes already declare their own target). Not derived automatically
    /// since nothing about a `command` string reliably says what port it
    /// binds; left blank shows as "-" rather than guessing wrong.
    pub port: Option<u16>,
    /// Whether this process is spawned automatically by `alcazar start`.
    /// Defaults to `true` so existing configs keep behaving the way they
    /// always have. Set to `false` to keep a process registered (it still
    /// shows up in `alcazar list`, as `stopped`) without actually spawning
    /// it until someone explicitly asks for it with `alcazar start <name>`
    /// or `alcazar restart <name>` — for services you only need on demand.
    #[serde(default = "default_autostart")]
    pub autostart: bool,
}

fn default_autostart() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    pub host: Option<String>,
    pub host_regex: Option<String>,
    pub target: String,
    #[serde(default)]
    pub priority: i32,
}

/// Expands a leading `~` to the home directory, then joins onto `base_dir`
/// if the (possibly-expanded) path is still relative.
///
/// `~/foo` is relative as far as `Path::is_relative()` is concerned (it
/// doesn't start with `/`), so without this a `cwd`/`log`/`tls_*` value like
/// `~/Dev/app` would get joined onto the config file's directory instead of
/// expanding to the home directory, producing a bogus nested path that never
/// exists (spawn fails with ENOENT).
pub fn resolve_path(base_dir: &Path, path: &Path) -> PathBuf {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => dirs_home().join(rest),
        Err(_) => path.to_path_buf(),
    };
    if expanded.is_relative() {
        base_dir.join(expanded)
    } else {
        expanded
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing config file {}", path.display()))?;
        config.resolve_relative_paths(&base_dir);
        config.validate()?;
        Ok(config)
    }

    fn resolve_relative_paths(&mut self, base_dir: &Path) {
        for process in &mut self.processes {
            process.cwd = process.cwd.take().map(|p| resolve_path(base_dir, &p));
            process.log = process.log.take().map(|p| resolve_path(base_dir, &p));
        }
        self.proxy.tls_cert = self.proxy.tls_cert.take().map(|p| resolve_path(base_dir, &p));
        self.proxy.tls_key = self.proxy.tls_key.take().map(|p| resolve_path(base_dir, &p));
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for process in &self.processes {
            if process.name.trim().is_empty() {
                bail!("a [[process]] entry has an empty name");
            }
            if !seen.insert(process.name.as_str()) {
                bail!("duplicate process name: {}", process.name);
            }
        }

        for route in &self.routes {
            match (&route.host, &route.host_regex) {
                (None, None) => bail!("a [[route]] entry needs either `host` or `host_regex`"),
                (Some(_), Some(_)) => {
                    bail!("a [[route]] entry cannot set both `host` and `host_regex`")
                }
                _ => {}
            }
            if let Some(pattern) = &route.host_regex {
                Regex::new(pattern)
                    .with_context(|| format!("invalid host_regex `{pattern}`"))?;
            }
            if !route.target.starts_with("http://") && !route.target.starts_with("https://") {
                bail!(
                    "route target `{}` must start with http:// or https://",
                    route.target
                );
            }
        }

        if !self.routes.is_empty() && (self.proxy.tls_cert.is_none() || self.proxy.tls_key.is_none()) {
            bail!("[[route]] entries are configured but [proxy] tls_cert/tls_key are missing");
        }

        Ok(())
    }
}

impl ProcessConfig {
    /// Log file path, defaulting to ~/.alcazar/logs/<name>.log.
    pub fn log_path(&self) -> PathBuf {
        match &self.log {
            Some(path) => path.clone(),
            None => default_log_dir().join(format!("{}.log", self.name)),
        }
    }

    /// Path used to remember this process's pid/pgid across daemon restarts
    /// (~/.alcazar/pids/<name>.pid), so a fresh daemon can clean up whatever
    /// a previous, no-longer-running daemon left behind before spawning a
    /// new instance.
    pub fn pid_path(&self) -> PathBuf {
        dirs_home().join(".alcazar").join("pids").join(format!("{}.pid", self.name))
    }

    /// Kamal deploy destinations available for this process, derived from
    /// `<cwd>/config/deploy*.yml` — not user-configured, always derived
    /// fresh so it stays in sync with the service's own repo. A file named
    /// `deploy.<name>.yml` becomes environment `<name>`; if only the bare
    /// `deploy.yml` exists (no destination-specific files), that's a single
    /// implicit environment, surfaced as `"default"` (reaching it needs no
    /// `-d` flag). Returns an empty vec if there's no `cwd` or no
    /// `config/deploy.yml` — most processes aren't Kamal-deployed at all.
    pub fn kamal_environments(&self) -> Vec<String> {
        let Some(cwd) = &self.cwd else {
            return Vec::new();
        };
        let config_dir = cwd.join("config");
        let Ok(entries) = std::fs::read_dir(&config_dir) else {
            return Vec::new();
        };

        let mut envs: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|name| {
                let rest = name.strip_prefix("deploy.")?.strip_suffix(".yml")?;
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                }
            })
            .collect();
        envs.sort_by_key(|name| (environment_rank(name), name.clone()));

        if envs.is_empty() && config_dir.join("deploy.yml").is_file() {
            envs.push("default".to_string());
        }
        envs
    }
}

/// Orders Kamal environments the way they're promoted through in practice
/// (staging -> validation -> production), so the dashboard's tab strip
/// reads left-to-right as a deploy pipeline instead of alphabetically
/// (which would put "production" first). Anything not in this list sorts
/// after the known ones, alphabetically among themselves.
fn environment_rank(name: &str) -> usize {
    match name {
        "staging" => 0,
        "validation" => 1,
        "production" => 2,
        _ => 3,
    }
}

/// Appends a new `[[process]]` table to the config file on disk, preserving
/// every existing comment/formatting/table untouched (unlike a
/// parse-then-serialize round trip through `toml`). Used by the dashboard's
/// "add service" form so a hot-added process also survives a daemon restart.
pub fn append_process_toml(
    path: &Path,
    name: &str,
    cwd: Option<&str>,
    command: &str,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let mut doc = raw
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing config file {}", path.display()))?;

    let mut table = Table::new();
    table.insert("name", value(name));
    if let Some(cwd) = cwd {
        table.insert("cwd", value(cwd));
    }
    table.insert("command", value(command));
    if !env.is_empty() {
        let mut env_table = InlineTable::new();
        for (k, v) in env {
            env_table.insert(k, Value::from(v.clone()));
        }
        table.insert("env", Item::Value(Value::InlineTable(env_table)));
    }

    let processes = doc
        .as_table_mut()
        .entry("process")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    let array = processes
        .as_array_of_tables_mut()
        .context("`process` key in config exists but isn't an array of tables")?;
    array.push(table);

    std::fs::write(path, doc.to_string())
        .with_context(|| format!("writing config file {}", path.display()))?;
    Ok(())
}

pub fn default_log_dir() -> PathBuf {
    dirs_home().join(".alcazar").join("logs")
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where the daemon's own stdout/stderr (not any supervised process's) is
/// redirected to by `daemonize()` in `main.rs`. Shared with the dashboard so
/// it can serve this alongside per-process logs under the same `/api/logs/`
/// endpoints, using this name as the pseudo-process id.
pub const DAEMON_LOG_NAME: &str = "alcazar";

pub fn daemon_log_path() -> PathBuf {
    dirs_home().join(".alcazar").join("daemon.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("alcazar.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn parses_minimal_process() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "back"
            command = "echo hi"
            "#,
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(config.processes.len(), 1);
        assert_eq!(config.processes[0].name, "back");
        assert_eq!(config.dns.suffix, "test");
    }

    /// A `[[process]]` with no `command` at all is valid — it's how a
    /// Kamal-only accessory (never run locally) gets registered purely for
    /// its dashboard logs/reboot controls.
    #[test]
    fn parses_process_without_command() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "registry"
            cwd = "/srv/infra/accessories/registry"
            "#,
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(config.processes.len(), 1);
        assert_eq!(config.processes[0].command, None);
    }

    #[test]
    fn parses_global_env_section() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [env]
            NODE_EXTRA_CA_CERTS = "ssl/devCA.pem"

            [[process]]
            name = "back"
            command = "echo hi"
            "#,
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.env.get("NODE_EXTRA_CA_CERTS"),
            Some(&"ssl/devCA.pem".to_string())
        );
        // Global env isn't merged into ProcessConfig.env itself — that
        // happens at spawn time in ManagedProcess::spawn — so the
        // per-process env here stays empty.
        assert!(config.processes[0].env.is_empty());
    }

    #[test]
    fn autostart_defaults_to_true_and_can_be_disabled() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "back"
            command = "echo hi"

            [[process]]
            name = "on-demand"
            command = "echo bye"
            autostart = false
            "#,
        );
        let config = Config::load(&path).unwrap();
        let back = config.processes.iter().find(|p| p.name == "back").unwrap();
        let on_demand = config.processes.iter().find(|p| p.name == "on-demand").unwrap();
        assert!(back.autostart);
        assert!(!on_demand.autostart);
    }

    #[test]
    fn kamal_environments_orders_staging_validation_production() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "back"
            cwd = "back"
            command = "echo hi"
            "#,
        );
        let cwd = dir.path().join("back").join("config");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("deploy.yml"), "").unwrap();
        // Written deliberately out of order to prove it's not just alphabetical.
        std::fs::write(cwd.join("deploy.production.yml"), "").unwrap();
        std::fs::write(cwd.join("deploy.validation.yml"), "").unwrap();
        std::fs::write(cwd.join("deploy.staging.yml"), "").unwrap();
        std::fs::write(cwd.join("deploy.canary.yml"), "").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.processes[0].kamal_environments(),
            vec![
                "staging".to_string(),
                "validation".to_string(),
                "production".to_string(),
                "canary".to_string(),
            ]
        );
    }

    #[test]
    fn kamal_environments_falls_back_to_default_with_only_bare_deploy_yml() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "storage"
            cwd = "storage"
            command = "echo hi"
            "#,
        );
        let cwd = dir.path().join("storage").join("config");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("deploy.yml"), "").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.processes[0].kamal_environments(), vec!["default".to_string()]);
    }

    #[test]
    fn kamal_environments_empty_without_a_config_dir() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "front"
            cwd = "front"
            command = "echo hi"
            "#,
        );
        std::fs::create_dir_all(dir.path().join("front")).unwrap();

        let config = Config::load(&path).unwrap();
        assert!(config.processes[0].kamal_environments().is_empty());
    }

    #[test]
    fn rejects_duplicate_process_names() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "back"
            command = "echo hi"

            [[process]]
            name = "back"
            command = "echo bye"
            "#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn rejects_route_without_host_or_regex() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[route]]
            target = "http://localhost:3000"
            "#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn rejects_routes_without_tls_config() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[route]]
            host = "api.myapp.test"
            target = "http://localhost:3011"
            "#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn example_config_parses_and_validates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("alcazar.example.toml");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.processes.len(), 5);
        assert_eq!(config.routes.len(), 2);
    }

    #[test]
    fn resolves_relative_cwd_against_config_dir() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "back"
            cwd = "../myapp-back"
            command = "echo hi"
            "#,
        );
        let config = Config::load(&path).unwrap();
        let expected = dir.path().join("../myapp-back");
        assert_eq!(config.processes[0].cwd.as_deref(), Some(expected.as_path()));
    }

    #[test]
    fn expands_tilde_in_cwd_instead_of_joining_it_as_relative() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            [[process]]
            name = "back"
            cwd = "~/Dev/ig-json-form/documentation"
            command = "yarn start"
            "#,
        );
        let config = Config::load(&path).unwrap();
        let expected = dirs_home().join("Dev/ig-json-form/documentation");
        assert_eq!(config.processes[0].cwd.as_deref(), Some(expected.as_path()));
    }

    #[test]
    fn append_process_toml_preserves_existing_content() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"
            # a comment that must survive
            [dns]
            suffix = "test"

            [[process]]
            name = "back"
            command = "echo hi"
            "#,
        );

        let mut env = std::collections::BTreeMap::new();
        env.insert("PORT".to_string(), "4000".to_string());
        append_process_toml(&path, "newsvc", Some("../newsvc"), "yarn dev", &env).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("a comment that must survive"));

        let config = Config::load(&path).unwrap();
        assert_eq!(config.processes.len(), 2);
        let added = config.processes.iter().find(|p| p.name == "newsvc").unwrap();
        assert_eq!(added.command.as_deref(), Some("yarn dev"));
        assert_eq!(added.env.get("PORT"), Some(&"4000".to_string()));
        assert!(added.cwd.as_ref().unwrap().ends_with("newsvc"));
    }

    // Minimal tempdir helper to avoid pulling in the `tempfile` crate for tests.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        let dir = std::env::temp_dir().join(format!("alcazar-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos}-{:?}", std::thread::current().id())
    }
}
