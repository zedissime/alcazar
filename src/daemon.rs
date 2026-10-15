use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;
use tracing::info;

use crate::config::Config;
use crate::dashboard::{self, DashboardState};
use crate::dns;
use crate::ipc;
use crate::proxy;
use crate::proxy::router::Router;
use crate::supervisor::ProcessManager;

/// Runs the supervisor + IPC server until stopped (Ctrl+C, SIGTERM, or a
/// `StopAllAndExit` IPC request), then stops every managed process before
/// returning.
pub async fn run(config: Config, config_path: PathBuf, socket_path: PathBuf) -> Result<()> {
    // Bind every privileged listener (":443", ":53") while still root, then
    // drop to the invoking user before spawning anything else — the process
    // supervisor, the IPC server, all of it. This must happen before
    // `ProcessManager::start_all`: every process it spawns inherits
    // whichever uid the daemon runs as at spawn time.
    let proxy_listener = if !config.routes.is_empty() {
        Some(proxy::bind(&config.proxy.listen).await?)
    } else {
        None
    };
    let http_redirect_listener = if !config.routes.is_empty() {
        Some(proxy::bind_http_redirect(&config.proxy.listen).await?)
    } else {
        None
    };
    let dns_listener = dns::bind(&config.dns.listen).await?;

    crate::privileges::drop_to_invoking_user()?;

    let manager = Arc::new(ProcessManager::start_all(config.processes, config.env.clone()));
    let shutdown = Arc::new(Notify::new());

    {
        let manager = manager.clone();
        let shutdown = shutdown.clone();
        let socket_path = socket_path.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::server::serve(&socket_path, manager, shutdown).await {
                tracing::error!(error = %e, "IPC server stopped unexpectedly");
            }
        });
    }

    if let Some(proxy_listener) = proxy_listener {
        let router = Arc::new(Router::new(&config.routes)?);
        let tls_cert = config
            .proxy
            .tls_cert
            .clone()
            .expect("Config::validate requires tls_cert when routes are set");
        let tls_key = config
            .proxy
            .tls_key
            .clone()
            .expect("Config::validate requires tls_key when routes are set");
        tokio::spawn(async move {
            if let Err(e) = proxy::serve(proxy_listener, &tls_cert, &tls_key, router).await {
                tracing::error!(error = %e, "reverse proxy stopped unexpectedly");
            }
        });
    }

    if let Some(http_redirect_listener) = http_redirect_listener {
        tokio::spawn(async move {
            if let Err(e) = proxy::serve_http_redirect(http_redirect_listener).await {
                tracing::error!(error = %e, "HTTP-to-HTTPS redirect listener stopped unexpectedly");
            }
        });
    }

    {
        let suffix = config.dns.suffix.clone();
        tokio::spawn(async move {
            if let Err(e) = dns::serve(dns_listener, &suffix).await {
                tracing::error!(error = %e, "DNS server stopped unexpectedly");
            }
        });
    }

    if config.dashboard.enabled {
        let listen = config.dashboard.listen.clone();
        let state = Arc::new(DashboardState {
            manager: manager.clone(),
            config_path: config_path.clone(),
            routes: config.routes.clone(),
            dns: config.dns.clone(),
            proxy_listen: config.proxy.listen.clone(),
        });
        tokio::spawn(async move {
            if let Err(e) = dashboard::serve(&listen, state).await {
                tracing::error!(error = %e, "dashboard stopped unexpectedly");
            }
        });
        info!(listen = %config.dashboard.listen, "dashboard enabled");
    }

    info!(count = manager.len().await, "alcazar daemon started");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    tokio::select! {
        _ = &mut ctrl_c => info!("received SIGINT, stopping"),
        _ = sigterm.recv() => info!("received SIGTERM, stopping"),
        _ = shutdown.notified() => info!("stop requested over IPC"),
    }

    manager.stop_all().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
