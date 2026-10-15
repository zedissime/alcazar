mod api;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::config::{DnsConfig, RouteConfig};
use crate::supervisor::ProcessManager;

/// Everything the dashboard's HTTP handlers need. Routes/dns/proxy are a
/// read-only snapshot taken once when the daemon starts — only the process
/// list is hot-reloadable in this version (see `ProcessManager::add`).
pub struct DashboardState {
    pub manager: Arc<ProcessManager>,
    pub config_path: PathBuf,
    pub routes: Vec<RouteConfig>,
    pub dns: DnsConfig,
    pub proxy_listen: String,
}

pub async fn serve(listen: &str, state: Arc<DashboardState>) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding dashboard listener on {listen}"))?;
    info!(listen, "dashboard listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "dashboard accept failed");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { api::handle(req, state).await }
            });
            if let Err(e) = server_http1::Builder::new().serve_connection(io, service).await {
                warn!(error = %e, %peer, "dashboard connection error");
            }
        });
    }
}
