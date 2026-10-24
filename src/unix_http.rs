//! HTTP over a unix socket, for telling a lease's mockbmc how the DPU's power changed.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Method, Request, header::HOST};
use hyper_util::rt::TokioIo;
use tokio::{net::UnixStream, time::timeout};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends a request without a body and fails unless the answer is a success.
pub async fn send(socket: &Path, method: Method, path: &str) -> Result<()> {
    timeout(
        REQUEST_TIMEOUT,
        send_unbounded(socket, method.clone(), path),
    )
    .await
    .with_context(|| format!("waiting {}s for an answer", REQUEST_TIMEOUT.as_secs()))?
    .with_context(|| format!("{method} {path} on {}", socket.display()))
}

async fn send_unbounded(socket: &Path, method: Method, path: &str) -> Result<()> {
    let stream = UnixStream::connect(socket).await.context("connecting")?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("starting HTTP")?;
    tokio::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, "localhost")
        .body(Empty::<Bytes>::new())
        .context("building the request")?;
    let response = sender
        .send_request(request)
        .await
        .context("sending the request")?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response
        .into_body()
        .collect()
        .await
        .context("reading the answer")?
        .to_bytes();
    bail!("{status}: {}", String::from_utf8_lossy(&body).trim())
}
