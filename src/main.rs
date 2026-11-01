mod bar;
mod cloudflare;
mod mlab;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client;
use tokio::task::JoinSet;

use bar::{SpeedBar, Spinner};

/// Measure ping, download and upload speed.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Test against the nearest M-Lab NDT7 server, the platform behind Google's speed test (default).
    #[arg(long, conflicts_with = "cloudflare")]
    mlab: bool,
    /// Test against Cloudflare's speed test endpoints instead of M-Lab.
    #[arg(long)]
    cloudflare: bool,
    /// Seconds to run each of the download and upload phases (M-Lab servers stop at 10).
    #[arg(long, default_value_t = 10)]
    duration: u64,
    /// Number of parallel connections per phase (Cloudflare only; NDT7 is a single stream).
    #[arg(long, default_value_t = 4)]
    connections: usize,
    /// Number of latency samples.
    #[arg(long, default_value_t = 10)]
    pings: usize,
}

/// Cloudflare's own speed test runs in the browser, so present the same kind of client it
/// expects; a bespoke user agent is more likely to be rate-limited or blocked. M-Lab doesn't mind.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

enum Backend {
    Mlab(mlab::Server),
    Cloudflare(cloudflare::Server),
}

impl Backend {
    async fn locate(client: &Client, use_cloudflare: bool) -> Result<Self> {
        Ok(if use_cloudflare {
            Self::Cloudflare(cloudflare::locate(client).await?)
        } else {
            Self::Mlab(mlab::locate(client).await?)
        })
    }

    fn describe(&self) -> String {
        match self {
            Self::Mlab(s) => format!("{}, {}", s.city, s.country),
            Self::Cloudflare(s) => format!("Cloudflare {}", s.colo),
        }
    }

    fn ping_url(&self) -> String {
        match self {
            Self::Mlab(s) => s.ping_url.clone(),
            Self::Cloudflare(_) => cloudflare::PING_URL.to_owned(),
        }
    }

    fn max_duration(&self, requested: Duration) -> Duration {
        match self {
            Self::Mlab(_) => requested.min(Duration::from_secs(10)),
            Self::Cloudflare(_) => requested,
        }
    }

    fn download(
        &self,
        client: &Client,
        connections: usize,
        deadline: Instant,
        counter: &Arc<AtomicU64>,
    ) -> JoinSet<Result<()>> {
        match self {
            Self::Mlab(s) => mlab::download(s.download_url.clone(), deadline, counter),
            Self::Cloudflare(_) => cloudflare::download(client, connections, deadline, counter),
        }
    }

    fn upload(
        &self,
        client: &Client,
        connections: usize,
        deadline: Instant,
        counter: &Arc<AtomicU64>,
    ) -> JoinSet<Result<()>> {
        match self {
            Self::Mlab(s) => mlab::upload(s.upload_url.clone(), deadline, counter),
            Self::Cloudflare(_) => cloudflare::upload(client, connections, deadline, counter),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")?;

    let backend = run_locate(&client, args.cloudflare).await?;

    let duration = backend.max_duration(Duration::from_secs(args.duration));
    // Download streams are torn down at the deadline; every byte counted has arrived.
    run_phase("Download", duration, false, |deadline, counter| {
        backend.download(&client, args.connections, deadline, counter)
    })
    .await?;
    // Upload stops sending at the deadline, then waits for the server to finish the exchange
    // so the average reflects what was accepted rather than what was queued locally.
    run_phase("Upload", duration, true, |deadline, counter| {
        backend.upload(&client, args.connections, deadline, counter)
    })
    .await?;

    run_ping(&client, backend.ping_url(), args.pings).await?;
    Ok(())
}

/// Finds the server behind a spinner, then prints where it is.
async fn run_locate(client: &Client, use_cloudflare: bool) -> Result<Backend> {
    let mut spinner = Spinner::new("Server");
    spinner.tick("locating nearest server");
    let mut task = tokio::spawn({
        let client = client.clone();
        async move { Backend::locate(&client, use_cloudflare).await }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let backend = loop {
        tokio::select! {
            _ = tick.tick() => spinner.tick("locating nearest server"),
            res = &mut task => break res?,
        }
    };
    spinner.clear();
    let backend = backend?;
    println!("{:<9} {}", "Server", backend.describe());
    Ok(backend)
}

/// After this many samples, stop early if sampling has already taken longer than
/// `SLOW_PING_BUDGET`: a slow link doesn't need ten round trips to characterise it.
const SLOW_PING_CUTOFF: usize = 5;
const SLOW_PING_BUDGET: Duration = Duration::from_secs(4);

/// One warm-up request (discarded) then up to `count` timed GETs on the reused connection.
/// `done` is incremented after each sample so the spinner can show progress.
async fn http_ping(
    client: Client,
    url: String,
    count: usize,
    done: Arc<AtomicUsize>,
) -> Result<Vec<Duration>> {
    let probe = || client.get(&url).send();
    probe().await.context("warm-up request")?;

    let sampling_started = Instant::now();
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let resp = probe().await.context("ping request")?;
        resp.bytes().await.context("ping response body")?;
        samples.push(start.elapsed());
        done.fetch_add(1, Ordering::Relaxed);
        if samples.len() == SLOW_PING_CUTOFF && sampling_started.elapsed() > SLOW_PING_BUDGET {
            break;
        }
    }
    Ok(samples)
}

/// Runs the latency phase behind a spinner that shows immediately, then prints min/mean/max.
async fn run_ping(client: &Client, url: String, count: usize) -> Result<()> {
    let done = Arc::new(AtomicUsize::new(0));
    let mut spinner = Spinner::new("Ping");
    spinner.tick("0");

    let mut task = tokio::spawn(http_ping(client.clone(), url, count, done.clone()));
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let samples = loop {
        tokio::select! {
            _ = tick.tick() => spinner.tick(&done.load(Ordering::Relaxed).to_string()),
            res = &mut task => break res?,
        }
    };
    spinner.clear();
    let samples = samples?;

    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    let min = samples.iter().min().map_or(0.0, ms);
    let max = samples.iter().max().map_or(0.0, ms);
    let mean = samples.iter().map(ms).sum::<f64>() / samples.len().max(1) as f64;
    println!(
        "{:<9} min {min:.1} ms   mean {mean:.1} ms   max {max:.1} ms   ({} samples)",
        "Ping",
        samples.len()
    );
    Ok(())
}

/// Runs a throughput phase: spawns the workers, drives the live bar with a 1 s sliding-window
/// speed until the deadline or until the workers finish (an NDT7 server ends the test itself),
/// then freezes the bar at the overall average. With `drain` the workers may finish their
/// final exchange (bounded) before the average is taken.
async fn run_phase<F>(label: &'static str, duration: Duration, drain: bool, spawn: F) -> Result<()>
where
    F: FnOnce(Instant, &Arc<AtomicU64>) -> JoinSet<Result<()>>,
{
    let counter = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let deadline = start + duration;
    let mut set = spawn(deadline, &counter);
    let mut bar = SpeedBar::new(label);
    bar.update(0.0);

    // (instant, cumulative bytes) samples for the sliding window.
    let mut history: Vec<(Instant, u64)> = vec![(start, 0)];
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = Instant::now();
                let total = counter.load(Ordering::Relaxed);
                history.push((now, total));
                history.retain(|(t, _)| now.duration_since(*t) <= Duration::from_secs(1));
                let (t0, b0) = history[0];
                let window = now.duration_since(t0).as_secs_f64();
                if window > 0.0 {
                    bar.update(mbps(total - b0, window));
                }
                if now >= deadline {
                    break;
                }
            }
            res = set.join_next() => match res {
                Some(Ok(Err(e))) => { set.abort_all(); bar.finish(0.0); return Err(e); }
                Some(Err(e)) if !e.is_cancelled() => { set.abort_all(); bar.finish(0.0); return Err(e.into()); }
                Some(_) => {}
                None => break,
            }
        }
    }
    if drain {
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while set.join_next().await.is_some() {}
        })
        .await;
    }
    set.abort_all();
    let elapsed = start.elapsed().as_secs_f64();
    bar.finish(mbps(counter.load(Ordering::Relaxed), elapsed));
    Ok(())
}

fn mbps(bytes: u64, secs: f64) -> f64 {
    bytes as f64 * 8.0 / secs / 1_000_000.0
}
