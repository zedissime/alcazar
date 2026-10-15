use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::protocol::{Request, Response};

pub async fn send_request(socket_path: &Path, request: Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "connecting to alcazar daemon at {} (is it running? try `alcazar start`)",
            socket_path.display()
        )
    })?;
    let (reader, mut writer) = stream.into_split();

    let mut json = serde_json::to_string(&request)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;

    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) => Ok(serde_json::from_str(&line)?),
        None => bail!("daemon closed the connection without responding"),
    }
}

pub async fn is_daemon_running(socket_path: &Path) -> bool {
    matches!(
        send_request(socket_path, Request::Ping).await,
        Ok(Response::Ok)
    )
}
