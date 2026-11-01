//! Latency and throughput measurement against Cloudflare's public speed-test endpoints.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

/// Cloudflare's own speed test runs in the browser, so present the same kind of client it
/// expects; a bespoke user agent is more likely to be rate-limited or blocked.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

pub fn client() -> Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")
}

/// One warm-up request (discarded) then `count` timed zero-byte GETs on the reused connection.
pub async fn ping(client: &Client, count: usize) -> Result<Vec<Duration>> {
    let probe = || client.get(DOWN_URL).query(&[("bytes", "0")]).send();
    probe().await.context("warm-up request to speed.cloudflare.com")?;

    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let resp = probe().await.context("ping request")?;
        resp.bytes().await.context("ping response body")?;
        samples.push(start.elapsed());
    }
    Ok(samples)
}

/// Spawns `connections` download loops that run until `deadline`, adding received bytes to `counter`.
/// Tasks are not aborted here; the caller aborts the set at the deadline.
pub fn download(client: &Client, connections: usize, deadline: Instant, counter: Arc<AtomicU64>) -> JoinSet<Result<()>> {
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
pub fn upload(client: &Client, connections: usize, deadline: Instant, counter: Arc<AtomicU64>) -> JoinSet<Result<()>> {
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
                        Some((Ok::<Bytes, std::io::Error>(zeros.clone()), sent + zeros.len() as u64))
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

#[cfg(test)]
mod tests {
    #[test]
    fn user_agent_is_a_single_line_browser_string() {
        let ua = super::USER_AGENT;
        assert!(ua.starts_with("Mozilla/5.0 ("), "{ua}");
        assert!(ua.contains("AppleWebKit/537.36 (KHTML, like Gecko) Chrome/"), "{ua}");
        assert!(!ua.contains('\n') && !ua.contains("  "), "{ua}");
    }
}
