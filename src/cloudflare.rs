//! Latency and throughput measurement against Cloudflare's public speed-test endpoints.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Body, Client};
use tokio::task::JoinSet;

const DOWN_URL: &str = "https://speed.cloudflare.com/__down";
const UP_URL: &str = "https://speed.cloudflare.com/__up";

/// Bytes requested per download GET; a new request starts when one finishes.
/// Cloudflare returns 403 for 100 MB, so stay at the size its own test uses.
const DOWN_CHUNK: u64 = 25_000_000;
/// Upper bound per upload POST so individual requests stay bounded.
const UP_REQUEST_MAX: u64 = 25_000_000;
const UP_CHUNK: usize = 64 * 1024;

/// Zero-byte download used for latency round trips.
pub const PING_URL: &str = "https://speed.cloudflare.com/__down?bytes=0";

/// The Cloudflare point of presence serving us, identified by the airport code in `CF-RAY`.
pub struct Server {
    pub colo: String,
}

pub async fn locate(client: &Client) -> Result<Server> {
    let resp = client
        .get(PING_URL)
        .send()
        .await
        .context("contacting speed.cloudflare.com")?
        .error_for_status()
        .context("speed.cloudflare.com response")?;
    let colo = resp
        .headers()
        .get("cf-ray")
        .and_then(|v| v.to_str().ok())
        .and_then(|ray| ray.rsplit_once('-'))
        .map_or_else(|| "unknown".to_owned(), |(_, colo)| colo.to_owned());
    Ok(Server { colo })
}

/// Spawns `connections` download loops that run until `deadline`, adding received bytes to `counter`.
/// Tasks are not aborted here; the caller aborts the set at the deadline.
pub fn download(
    client: &Client,
    connections: usize,
    deadline: Instant,
    counter: &Arc<AtomicU64>,
) -> JoinSet<Result<()>> {
    let mut set = JoinSet::new();
    for _ in 0..connections {
        let client = client.clone();
        let counter = counter.clone();
        set.spawn(async move {
            while Instant::now() < deadline {
                let resp = client
                    .get(DOWN_URL)
                    .query(&[("bytes", DOWN_CHUNK.to_string())])
                    .send()
                    .await
                    .context("download request")?
                    .error_for_status()
                    .context("download response")?;
                let mut body = resp.bytes_stream();
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.context("download stream")?;
                    counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    if Instant::now() >= deadline {
                        return Ok(());
                    }
                }
            }
            Ok(())
        });
    }
    set
}

/// Spawns `connections` upload loops. Each POST streams zero-filled chunks, counting them as they are
/// handed to the HTTP stack, and ends its body once the deadline passes so the request finishes cleanly.
///
/// The counter runs slightly ahead of the wire (socket buffering); fine for a speed estimate.
pub fn upload(
    client: &Client,
    connections: usize,
    deadline: Instant,
    counter: &Arc<AtomicU64>,
) -> JoinSet<Result<()>> {
    let mut set = JoinSet::new();
    let zeros = Bytes::from(vec![0u8; UP_CHUNK]);
    for _ in 0..connections {
        let client = client.clone();
        let counter = counter.clone();
        let zeros = zeros.clone();
        set.spawn(async move {
            while Instant::now() < deadline {
                let counter = counter.clone();
                let zeros = zeros.clone();
                let stream = futures_util::stream::unfold(0u64, move |sent| {
                    let counter = counter.clone();
                    let zeros = zeros.clone();
                    async move {
                        if sent >= UP_REQUEST_MAX || Instant::now() >= deadline {
                            return None;
                        }
                        counter.fetch_add(zeros.len() as u64, Ordering::Relaxed);
                        Some((
                            Ok::<Bytes, std::io::Error>(zeros.clone()),
                            sent + zeros.len() as u64,
                        ))
                    }
                });
                client
                    .post(UP_URL)
                    .header("content-type", "application/octet-stream")
                    .body(Body::wrap_stream(stream))
                    .send()
                    .await
                    .context("upload request")?
                    .error_for_status()
                    .context("upload response")?;
            }
            Ok(())
        });
    }
    set
}
