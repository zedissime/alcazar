use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tracing::{info, warn};

use super::protocol::{ProcessStatusDto, Request, Response};
use crate::supervisor::ProcessManager;

pub async fn serve(
    socket_path: &Path,
    manager: Arc<ProcessManager>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket_path.exists() {
        std::fs::remove_file(socket_path).ok();
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding IPC socket at {}", socket_path.display()))?;
    info!(path = %socket_path.display(), "IPC socket listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let manager = manager.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, manager, shutdown).await {
                warn!(error = %e, "IPC connection error");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    manager: Arc<ProcessManager>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::Error {
                    message: format!("bad request: {e}"),
                };
                send(&mut writer, &resp).await?;
                continue;
            }
        };
        let response = handle_request(request, &manager, &shutdown).await;
        send(&mut writer, &response).await?;
    }
    Ok(())
}

async fn handle_request(
    request: Request,
    manager: &Arc<ProcessManager>,
    shutdown: &Arc<Notify>,
) -> Response {
    match request {
        Request::Ping => Response::Ok,
        Request::List => {
            let statuses = manager.statuses().await;
            let dtos = statuses
                .into_iter()
                .map(|s| ProcessStatusDto {
                    name: s.name,
                    state: s.state.to_string(),
                    pid: s.pid,
                    restarts: s.restarts,
                    uptime_secs: s.started_at.map(|t| t.elapsed().as_secs()),
                    port: s.port,
                })
                .collect();
            Response::Statuses(dtos)
        }
        Request::Start { name } => {
            if manager.start(&name).await {
                Response::Ok
            } else {
                Response::Error {
                    message: format!("no such process: {name}"),
                }
            }
        }
        Request::Restart { name } => {
            if manager.restart(&name).await {
                Response::Ok
            } else {
                Response::Error {
                    message: format!("no such process: {name}"),
                }
            }
        }
        Request::Stop { name } => {
            if manager.stop(&name).await {
                Response::Ok
            } else {
                Response::Error {
                    message: format!("no such process: {name}"),
                }
            }
        }
        Request::StopAllAndExit => {
            manager.stop_all().await;
            shutdown.notify_one();
            Response::Ok
        }
        Request::LogPath { name } => match manager.log_path(&name).await {
            Some(path) => Response::Path(path.display().to_string()),
            None => Response::Error {
                message: format!("no such process: {name}"),
            },
        },
    }
}

async fn send(writer: &mut (impl AsyncWriteExt + Unpin), response: &Response) -> Result<()> {
    let mut json = serde_json::to_string(response)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    Ok(())
}
