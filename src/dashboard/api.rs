use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tracing::warn;

use super::DashboardState;
use crate::config::ProcessConfig;

type ResBody = BoxBody<Bytes, hyper::Error>;

const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<DashboardState>,
) -> Result<Response<ResBody>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    let response = if method == Method::GET && path == "/" {
        html_response(DASHBOARD_HTML)
    } else if method == Method::GET && path == "/api/processes" {
        api_processes(&state).await
    } else if method == Method::GET && path == "/api/config" {
        api_config(&state).await
    } else if method == Method::POST && path == "/api/stop-all" {
        state.manager.stop_all().await;
        ok_json()
    } else if method == Method::POST && path == "/api/restart-all" {
        state.manager.restart_all().await;
        ok_json()
    } else if method == Method::POST && path == "/api/config/processes" {
        api_add_process(req, &state).await
    } else if method == Method::GET && path.starts_with("/api/logs/") && path.contains("/remote/") && path.ends_with("/stream") {
        let (name, env) = parse_remote_log_path(&path, "/stream");
        api_log_stream_remote(name, env, &state).await
    } else if method == Method::GET && path.starts_with("/api/logs/") && path.contains("/remote/") {
        let (name, env) = parse_remote_log_path(&path, "");
        api_log_tail_remote(name, env, &query, &state).await
    } else if method == Method::GET && path.starts_with("/api/logs/") && path.ends_with("/stream") {
        let name = &path["/api/logs/".len()..path.len() - "/stream".len()];
        api_log_stream(name, &state).await
    } else if method == Method::GET && path.starts_with("/api/logs/") {
        let name = &path["/api/logs/".len()..];
        api_log_tail(name, &query, &state).await
    } else if method == Method::POST
        && path.starts_with("/api/processes/")
        && path.ends_with("/start")
    {
        let name = &path["/api/processes/".len()..path.len() - "/start".len()];
        api_process_action(name, &state, Action::Start).await
    } else if method == Method::POST
        && path.starts_with("/api/processes/")
        && path.ends_with("/restart")
    {
        let name = &path["/api/processes/".len()..path.len() - "/restart".len()];
        api_process_action(name, &state, Action::Restart).await
    } else if method == Method::POST
        && path.starts_with("/api/processes/")
        && path.ends_with("/stop")
    {
        let name = &path["/api/processes/".len()..path.len() - "/stop".len()];
        api_process_action(name, &state, Action::Stop).await
    } else if method == Method::POST
        && path.starts_with("/api/processes/")
        && path.contains("/kamal/")
        && path.ends_with("/boot")
    {
        let (name, env) = parse_kamal_boot_path(&path);
        api_kamal_boot(name, env, &state).await
    } else {
        not_found()
    };

    Ok(response)
}

enum Action {
    Start,
    Restart,
    Stop,
}

// ---- process list ----

#[derive(Serialize)]
struct ProcessView {
    name: String,
    state: String,
    pid: Option<u32>,
    uptime_secs: Option<u64>,
    restarts: u32,
    cwd: Option<String>,
    /// `None` for a Kamal-only accessory alcazar never spawns or controls
    /// locally — see `ProcessConfig::command`.
    command: Option<String>,
    port: Option<u16>,
    kamal_environments: Vec<String>,
}

async fn api_processes(state: &DashboardState) -> Response<ResBody> {
    let statuses = state.manager.statuses().await;
    let configs = state.manager.configs_snapshot().await;
    let config_by_name: HashMap<&str, &ProcessConfig> =
        configs.iter().map(|c| (c.name.as_str(), c)).collect();

    let views: Vec<ProcessView> = statuses
        .into_iter()
        .map(|s| {
            let cfg = config_by_name.get(s.name.as_str()).copied();
            ProcessView {
                pid: s.pid,
                uptime_secs: s.started_at.map(|t| t.elapsed().as_secs()),
                restarts: s.restarts,
                cwd: cfg
                    .and_then(|c| c.cwd.as_ref())
                    .map(|p| p.display().to_string()),
                command: cfg.and_then(|c| c.command.clone()),
                kamal_environments: cfg.map(|c| c.kamal_environments()).unwrap_or_default(),
                port: s.port,
                state: s.state.to_string(),
                name: s.name,
            }
        })
        .collect();

    json_response(StatusCode::OK, &views)
}

async fn api_process_action(name: &str, state: &DashboardState, action: Action) -> Response<ResBody> {
    let ok = match action {
        Action::Start => state.manager.start(name).await,
        Action::Restart => state.manager.restart(name).await,
        Action::Stop => state.manager.stop(name).await,
    };
    if ok {
        ok_json()
    } else {
        json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": format!("no such process: {name}") }),
        )
    }
}

// ---- config (read-only) ----

#[derive(Serialize)]
struct RouteView {
    host: Option<String>,
    host_regex: Option<String>,
    target: String,
    priority: i32,
}

#[derive(Serialize)]
struct ConfigView {
    dns_suffix: String,
    dns_listen: String,
    proxy_listen: String,
    routes: Vec<RouteView>,
}

async fn api_config(state: &DashboardState) -> Response<ResBody> {
    let routes = state
        .routes
        .iter()
        .map(|r| RouteView {
            host: r.host.clone(),
            host_regex: r.host_regex.clone(),
            target: r.target.clone(),
            priority: r.priority,
        })
        .collect();
    let view = ConfigView {
        dns_suffix: state.dns.suffix.clone(),
        dns_listen: state.dns.listen.clone(),
        proxy_listen: state.proxy_listen.clone(),
        routes,
    };
    json_response(StatusCode::OK, &view)
}

// ---- add a new process ----

#[derive(Deserialize)]
struct AddProcessRequest {
    name: String,
    cwd: Option<String>,
    command: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

async fn api_add_process(req: Request<Incoming>, state: &DashboardState) -> Response<ResBody> {
    let body = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return bad_request(&format!("failed to read request body: {e}")),
    };
    let payload: AddProcessRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return bad_request(&format!("invalid JSON: {e}")),
    };
    if payload.name.trim().is_empty() || payload.command.trim().is_empty() {
        return bad_request("name and command are required");
    }

    if let Err(e) = crate::config::append_process_toml(
        &state.config_path,
        &payload.name,
        payload.cwd.as_deref(),
        &payload.command,
        &payload.env,
    ) {
        warn!(error = %e, "failed to persist new process to config file");
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({ "error": e.to_string() }),
        );
    }

    let config_dir = state.config_path.parent().unwrap_or_else(|| Path::new("."));
    let cwd = payload
        .cwd
        .map(PathBuf::from)
        .map(|c| crate::config::resolve_path(config_dir, &c));
    let config = ProcessConfig {
        name: payload.name,
        cwd,
        command: Some(payload.command),
        env: payload.env.into_iter().collect(),
        log: None,
        port: None,
        autostart: true,
    };

    if state.manager.add(config).await {
        json_response(StatusCode::CREATED, &serde_json::json!({ "ok": true }))
    } else {
        json_response(
            StatusCode::CONFLICT,
            &serde_json::json!({
                "error": "a process with that name is already running (it was still added to the config file — restart alcazar to reconcile)"
            }),
        )
    }
}

// ---- logs ----

/// The dashboard serves alcazar's own daemon log through the same
/// `/api/logs/<name>` endpoints as any supervised process, addressed by the
/// reserved pseudo-name `crate::config::DAEMON_LOG_NAME` — it isn't a
/// process the supervisor knows about, so it can't come from
/// `manager.log_path()`.
async fn resolve_log_path(name: &str, state: &DashboardState) -> Option<PathBuf> {
    if name == crate::config::DAEMON_LOG_NAME {
        Some(crate::config::daemon_log_path())
    } else {
        state.manager.log_path(name).await
    }
}

async fn api_log_tail(name: &str, query: &str, state: &DashboardState) -> Response<ResBody> {
    let lines = parse_lines_param(query).unwrap_or(200);
    let Some(path) = resolve_log_path(name, state).await else {
        return not_found();
    };

    let lines_out: Vec<String> = match tokio::fs::read_to_string(&path).await {
        Ok(contents) => {
            let all: Vec<&str> = contents.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].iter().map(|s| s.to_string()).collect()
        }
        Err(_) => Vec::new(),
    };

    json_response(StatusCode::OK, &serde_json::json!({ "lines": lines_out }))
}

fn parse_lines_param(query: &str) -> Option<usize> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == "lines" {
            v.parse().ok()
        } else {
            None
        }
    })
}

async fn api_log_stream(name: &str, state: &DashboardState) -> Response<ResBody> {
    let Some(path) = resolve_log_path(name, state).await else {
        return not_found();
    };

    let (tx, rx) = mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        let mut offset = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let len = match tokio::fs::metadata(&path).await {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if len < offset {
                offset = 0;
            }
            if len <= offset {
                continue;
            }

            let Ok(mut file) = tokio::fs::File::open(&path).await else {
                continue;
            };
            if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).await.is_err() {
                continue;
            }
            offset = len;

            for line in String::from_utf8_lossy(&buf).lines() {
                let payload = format!("data: {}\n\n", serde_json::to_string(line).unwrap_or_default());
                if tx.send(Bytes::from(payload)).await.is_err() {
                    return; // client disconnected
                }
            }
        }
    });

    sse_response(rx)
}

// ---- remote (Kamal) logs ----

/// Splits a `/api/logs/<name>/remote/<env>[<suffix>]` path into `(name,
/// env)`. `suffix` is whatever trailing segment (e.g. `/stream`) the caller
/// already matched on and wants stripped first.
fn parse_remote_log_path<'a>(path: &'a str, suffix: &str) -> (&'a str, &'a str) {
    let trimmed = &path["/api/logs/".len()..path.len() - suffix.len()];
    trimmed.split_once("/remote/").unwrap_or((trimmed, ""))
}

/// Builds the `kamal app logs` invocation shared by the tail and stream
/// endpoints. `env == "default"` means the process has only a bare
/// `config/deploy.yml` (see `ProcessConfig::kamal_environments`), so no
/// `-d` flag is passed.
fn kamal_logs_command(cwd: &Path, env: &str) -> Command {
    let mut cmd = Command::new("kamal");
    cmd.arg("app").arg("logs").arg("--skip-hooks");
    if env != "default" {
        cmd.arg("-d").arg(env);
    }
    cmd.current_dir(cwd).stdin(Stdio::null()).kill_on_drop(true);
    cmd
}

/// Splits a `/api/processes/<name>/kamal/<env>/boot` path into `(name,
/// env)`.
fn parse_kamal_boot_path(path: &str) -> (&str, &str) {
    let trimmed = &path["/api/processes/".len()..path.len() - "/boot".len()];
    trimmed.split_once("/kamal/").unwrap_or((trimmed, ""))
}

/// Runs `kamal app boot` (rebooting the app on the destination's servers)
/// for one Kamal environment of a supervised process. `env == "default"`
/// mirrors `kamal_logs_command`: no `-d` flag for a process with only a
/// bare `config/deploy.yml`.
async fn api_kamal_boot(name: &str, env: &str, state: &DashboardState) -> Response<ResBody> {
    let Some(cwd) = state.manager.cwd(name).await else {
        return not_found();
    };

    let mut cmd = Command::new("kamal");
    cmd.arg("app").arg("boot");
    if env != "default" {
        cmd.arg("-d").arg(env);
    }
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(child) => match tokio::time::timeout(Duration::from_secs(120), child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut combined = output.stdout;
                combined.extend_from_slice(&output.stderr);
                let log = String::from_utf8_lossy(&combined).to_string();
                if output.status.success() {
                    json_response(StatusCode::OK, &serde_json::json!({ "ok": true, "output": log }))
                } else {
                    json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &serde_json::json!({ "error": "kamal app boot failed", "output": log }),
                    )
                }
            }
            Ok(Err(e)) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": format!("kamal app boot failed: {e}") }),
            ),
            Err(_) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": "kamal app boot timed out after 120s" }),
            ),
        },
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({ "error": format!("failed to run kamal app boot: {e}") }),
        ),
    }
}

async fn api_log_tail_remote(name: &str, env: &str, query: &str, state: &DashboardState) -> Response<ResBody> {
    let Some(cwd) = state.manager.cwd(name).await else {
        return not_found();
    };
    let lines = parse_lines_param(query).unwrap_or(200);

    let mut cmd = kamal_logs_command(&cwd, env);
    cmd.arg("-n")
        .arg(lines.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let lines_out = match cmd.spawn() {
        Ok(child) => match tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut combined = output.stdout;
                combined.extend_from_slice(&output.stderr);
                String::from_utf8_lossy(&combined)
                    .lines()
                    .map(|s| s.to_string())
                    .collect()
            }
            Ok(Err(e)) => vec![format!("[alcazar] kamal app logs failed: {e}")],
            Err(_) => vec!["[alcazar] kamal app logs timed out after 15s".to_string()],
        },
        Err(e) => vec![format!("[alcazar] failed to run kamal app logs: {e}")],
    };

    json_response(StatusCode::OK, &serde_json::json!({ "lines": lines_out }))
}

async fn api_log_stream_remote(name: &str, env: &str, state: &DashboardState) -> Response<ResBody> {
    let Some(cwd) = state.manager.cwd(name).await else {
        return not_found();
    };

    let mut cmd = kamal_logs_command(&cwd, env);
    cmd.arg("--follow").stdout(Stdio::piped()).stderr(Stdio::piped());

    let (tx, rx) = mpsc::channel::<Bytes>(16);
    match cmd.spawn() {
        Ok(mut child) => {
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            tokio::spawn(async move {
                // Keeping `child` alive in this task's scope means
                // `kill_on_drop` fires as soon as the task returns (e.g.
                // because the client disconnected and `tx.send` started
                // failing), instead of leaking a `kamal app logs --follow`
                // SSH session per abandoned dashboard tab.
                let _child = child;
                let mut out_lines = BufReader::new(stdout).lines();
                let mut err_lines = BufReader::new(stderr).lines();
                loop {
                    let line = tokio::select! {
                        l = out_lines.next_line() => l,
                        l = err_lines.next_line() => l,
                    };
                    match line {
                        Ok(Some(line)) => {
                            let payload = format!("data: {}\n\n", serde_json::to_string(&line).unwrap_or_default());
                            if tx.send(Bytes::from(payload)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => return,
                        Err(_) => return,
                    }
                }
            });
        }
        Err(e) => {
            let line = format!("[alcazar] failed to run kamal app logs: {e}");
            let payload = format!("data: {}\n\n", serde_json::to_string(&line).unwrap_or_default());
            let _ = tx.try_send(Bytes::from(payload));
        }
    }

    sse_response(rx)
}

fn sse_response(rx: mpsc::Receiver<Bytes>) -> Response<ResBody> {
    let stream = ReceiverStream::new(rx).map(|bytes| Ok::<_, hyper::Error>(Frame::data(bytes)));
    let body = StreamBody::new(stream).boxed();
    let mut resp = Response::new(body);
    resp.headers_mut()
        .insert(hyper::header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    resp.headers_mut()
        .insert(hyper::header::CACHE_CONTROL, "no-cache".parse().unwrap());
    resp
}

// ---- response helpers ----

fn full_body(bytes: Bytes) -> ResBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<ResBody> {
    let bytes = Bytes::from(serde_json::to_vec(value).unwrap_or_default());
    let mut resp = Response::new(full_body(bytes));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
    resp
}

fn ok_json() -> Response<ResBody> {
    json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

fn bad_request(msg: &str) -> Response<ResBody> {
    json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": msg }))
}

fn not_found() -> Response<ResBody> {
    json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": "not found" }))
}

fn html_response(html: &'static str) -> Response<ResBody> {
    let mut resp = Response::new(full_body(Bytes::from_static(html.as_bytes())));
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    resp
}
