pub mod router;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1 as client_http1;
use hyper::header::{HOST, LOCATION, UPGRADE};
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use router::Router;

type ResponseBody = BoxBody<Bytes, hyper::Error>;

/// Binds the (typically privileged, `:443`) listener. Split out from
/// `serve` so the daemon can do this while still root and drop privileges
/// before running the accept loop — everything after the bind (TLS config
/// loading, request handling) then runs as the invoking user, not root.
pub async fn bind(listen: &str) -> Result<TcpListener> {
    TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding reverse proxy listener on {listen}"))
}

pub async fn serve(
    listener: TcpListener,
    tls_cert: &Path,
    tls_key: &Path,
    router: Arc<Router>,
) -> Result<()> {
    // rustls 0.23 needs a process-wide default crypto provider installed
    // before building any ServerConfig; installing twice is harmless.
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let tls_config = load_tls_config(tls_cert, tls_key)?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    info!("reverse proxy listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "proxy accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, %peer, "TLS handshake failed");
                    return;
                }
            };
            let io = TokioIo::new(tls_stream);
            let service = service_fn(move |req| {
                let router = router.clone();
                async move { Ok::<_, hyper::Error>(handle_request(req, router).await) }
            });
            if let Err(e) = server_http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                warn!(error = %e, %peer, "proxy connection error");
            }
        });
    }
}

/// Derives the plain-HTTP counterpart of the (typically privileged, TLS)
/// `[proxy].listen` address by swapping its port for `80` — same host, so
/// the redirect listener binds alongside the HTTPS one on every interface
/// it's reachable from.
fn http_redirect_addr(tls_listen: &str) -> Result<String> {
    let addr: SocketAddr = tls_listen
        .parse()
        .with_context(|| format!("parsing proxy listen address {tls_listen}"))?;
    Ok(SocketAddr::new(addr.ip(), 80).to_string())
}

/// Binds the plain-HTTP redirect listener (`:80` alongside the TLS `:443`).
/// Split out from `serve_http_redirect` for the same reason as `bind`: the
/// daemon binds every privileged port while still root, before dropping to
/// the invoking user.
pub async fn bind_http_redirect(tls_listen: &str) -> Result<TcpListener> {
    let addr = http_redirect_addr(tls_listen)?;
    TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding HTTP-to-HTTPS redirect listener on {addr}"))
}

/// Answers every plain-HTTP request with a `301` to the same host and path
/// over HTTPS — so a stray `http://` link (copy-pasted, hardcoded by an
/// upstream service that doesn't know it's behind TLS, etc.) still reaches
/// the real proxy instead of hitting a bare connection-refused, since
/// `[proxy].listen` only ever binds TLS on `:443`, never plaintext `:80`.
pub async fn serve_http_redirect(listener: TcpListener) -> Result<()> {
    info!("HTTP-to-HTTPS redirect listener listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "HTTP redirect accept failed");
                continue;
            }
        };
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(|req| async move { Ok::<_, hyper::Error>(redirect_to_https(req)) });
            if let Err(e) = server_http1::Builder::new().serve_connection(io, service).await {
                warn!(error = %e, %peer, "HTTP redirect connection error");
            }
        });
    }
}

fn https_location(host: &str, path_and_query: &str) -> String {
    format!("https://{host}{path_and_query}")
}

fn redirect_to_https(req: Request<Incoming>) -> Response<ResponseBody> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let location = https_location(host, path_and_query);

    let mut resp = text_response(StatusCode::MOVED_PERMANENTLY, "redirecting to https");
    if let Ok(value) = location.parse() {
        resp.headers_mut().insert(LOCATION, value);
    }
    resp
}

fn load_tls_config(cert_path: &Path, key_path: &Path) -> Result<ServerConfig> {
    let cert_bytes = std::fs::read(cert_path)
        .with_context(|| format!("reading TLS cert {}", cert_path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing TLS cert {}", cert_path.display()))?;

    let key_bytes = std::fs::read(key_path)
        .with_context(|| format!("reading TLS key {}", key_path.display()))?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .with_context(|| format!("parsing TLS key {}", key_path.display()))?
        .context("no private key found in TLS key file")?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS server config")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

async fn handle_request(req: Request<Incoming>, router: Arc<Router>) -> Response<ResponseBody> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    let Some(target) = router.resolve(&host).map(str::to_string) else {
        warn!(host, "no route matched");
        return text_response(StatusCode::NOT_FOUND, "no route matched Host header");
    };

    match proxy_to(&target, req).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, host, target, "upstream error");
            text_response(StatusCode::BAD_GATEWAY, "upstream request failed")
        }
    }
}

async fn proxy_to(target: &str, mut req: Request<Incoming>) -> Result<Response<ResponseBody>> {
    let uri: Uri = target.parse().context("invalid route target")?;
    let host = uri.host().context("route target missing host")?;
    let port = uri.port_u16().unwrap_or(80);
    let addr = format!("{host}:{port}");

    let is_upgrade = req.headers().get(UPGRADE).is_some();
    let client_upgrade = is_upgrade.then(|| hyper::upgrade::on(&mut req));

    let (mut parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    parts.uri = path_and_query.parse().context("rewriting request URI")?;
    let outbound_req = Request::from_parts(parts, body);

    let stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connecting to upstream {addr}"))?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = client_http1::Builder::new()
        .preserve_header_case(true)
        .handshake(io)
        .await
        .with_context(|| format!("HTTP handshake with upstream {addr}"))?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            warn!(error = %e, "upstream connection closed with error");
        }
    });

    let mut response = sender
        .send_request(outbound_req)
        .await
        .with_context(|| format!("sending request to upstream {addr}"))?;

    if is_upgrade && response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let client_upgrade = client_upgrade.expect("is_upgrade implies client_upgrade is Some");
        let upstream_upgrade = hyper::upgrade::on(&mut response);

        let status = response.status();
        let headers = response.headers().clone();

        tokio::spawn(async move {
            match (client_upgrade.await, upstream_upgrade.await) {
                (Ok(client), Ok(upstream)) => {
                    let mut client = TokioIo::new(client);
                    let mut upstream = TokioIo::new(upstream);
                    if let Err(e) =
                        tokio::io::copy_bidirectional(&mut client, &mut upstream).await
                    {
                        warn!(error = %e, "websocket proxy copy error");
                    }
                }
                _ => warn!("failed to complete WebSocket upgrade on one side"),
            }
        });

        let mut resp = Response::new(empty_body());
        *resp.status_mut() = status;
        *resp.headers_mut() = headers;
        return Ok(resp);
    }

    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(parts, body.boxed()))
}

fn empty_body() -> ResponseBody {
    Full::new(Bytes::new()).map_err(|never| match never {}).boxed()
}

fn text_response(status: StatusCode, message: &str) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(message.to_string()))
        .map_err(|never| match never {})
        .boxed();
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_redirect_addr_swaps_port_for_80() {
        assert_eq!(http_redirect_addr("0.0.0.0:443").unwrap(), "0.0.0.0:80");
        assert_eq!(http_redirect_addr("127.0.0.1:8443").unwrap(), "127.0.0.1:80");
    }

    #[test]
    fn http_redirect_addr_rejects_invalid_listen() {
        assert!(http_redirect_addr("not-an-address").is_err());
    }

    #[test]
    fn https_location_preserves_host_and_path() {
        assert_eq!(
            https_location("api.myapp.test", "/rails/active_storage/blobs/redirect/abc/file.xlsx"),
            "https://api.myapp.test/rails/active_storage/blobs/redirect/abc/file.xlsx"
        );
    }
}
