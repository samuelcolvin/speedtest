mod bar;
mod cloudflare;
mod mlab;

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client;
use tokio::task::JoinSet;

use bar::{Finished, HiddenCursor, LiveLine, SpeedBar, Spinner};

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
/// expects; a bespoke user agent is more likely to be rate-limited or blocked.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                                  (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// M-Lab asks clients to identify themselves, so there we say who we are.
const HONEST_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

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
    // Restored when this guard drops, which covers normal exit and errors; Ctrl-C bypasses
    // drop, so a signal handler restores it explicitly before exiting.
    let _cursor = HiddenCursor::new();
    // Drawn before building the HTTP client or any networking, so output appears the moment
    // the CLI starts.
    let mut spinner = Spinner::new("Server");
    spinner.tick(LOCATING);
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            if std::io::stdout().is_terminal() {
                // The cursor is at the start of a live line: step past it and the blank line below.
                print!("\n\n{}", bar::SHOW_CURSOR);
                let _ = std::io::stdout().flush();
            }
            std::process::exit(130);
        }
    });

    let user_agent = if args.cloudflare {
        BROWSER_USER_AGENT
    } else {
        HONEST_USER_AGENT
    };
    let client = Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")?;

    let backend = run_locate(spinner, &client, args.cloudflare).await?;

    let duration = backend.max_duration(Duration::from_secs(args.duration));
    // Download streams are torn down at the deadline; every byte counted has arrived.
    let download = run_phase("Download", None, duration, false, |deadline, counter| {
        backend.download(&client, args.connections, deadline, counter)
    })
    .await?;
    // Upload stops sending at the deadline, then waits for the server to finish the exchange
    // so the average reflects what was accepted rather than what was queued locally.
    run_phase(
        "Upload",
        Some(download),
        duration,
        true,
        |deadline, counter| backend.upload(&client, args.connections, deadline, counter),
    )
    .await?;

    run_ping(&client, backend.ping_url(), args.pings).await?;
    Ok(())
}

const LOCATING: &str = "locating nearest server";

/// Finds the server behind a spinner, then prints where it is.
async fn run_locate(
    mut spinner: Spinner,
    client: &Client,
    use_cloudflare: bool,
) -> Result<Backend> {
    let mut task = tokio::spawn({
        let client = client.clone();
        async move { Backend::locate(&client, use_cloudflare).await }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let backend = loop {
        tokio::select! {
            _ = tick.tick() => spinner.tick(LOCATING),
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
/// Each sample is pushed to `samples` as it completes so the caller can show running stats.
async fn http_ping(
    client: Client,
    url: String,
    count: usize,
    samples: Arc<Mutex<Vec<Duration>>>,
) -> Result<()> {
    let probe = || client.get(&url).send();
    probe().await.context("warm-up request")?;

    let sampling_started = Instant::now();
    for _ in 0..count {
        let start = Instant::now();
        let resp = probe().await.context("ping request")?;
        resp.bytes().await.context("ping response body")?;
        let taken = {
            let mut samples = samples.lock().expect("ping samples lock");
            samples.push(start.elapsed());
            samples.len()
        };
        if taken == SLOW_PING_CUTOFF && sampling_started.elapsed() > SLOW_PING_BUDGET {
            break;
        }
    }
    Ok(())
}

/// The Ping result line for the samples so far.
fn ping_line(samples: &[Duration]) -> String {
    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    let min = samples.iter().min().map_or(0.0, ms);
    let max = samples.iter().max().map_or(0.0, ms);
    let mean = samples.iter().map(ms).sum::<f64>() / samples.len().max(1) as f64;
    let plural = if samples.len() == 1 { "" } else { "s" };
    format!(
        "{:<9} min {min:.1} ms   mean {mean:.1} ms   max {max:.1} ms   ({} sample{plural})",
        "Ping",
        samples.len()
    )
}

/// Runs the latency phase: a spinner until the first sample, then the min/mean/max line
/// redrawn as each further sample arrives.
async fn run_ping(client: &Client, url: String, count: usize) -> Result<()> {
    let samples = Arc::new(Mutex::new(Vec::with_capacity(count)));
    let mut spinner = Spinner::new("Ping");
    let line = LiveLine::new();
    spinner.tick("");

    let mut task = tokio::spawn(http_ping(client.clone(), url, count, samples.clone()));
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let result = loop {
        tokio::select! {
            _ = tick.tick() => {
                let samples = samples.lock().expect("ping samples lock");
                if samples.is_empty() {
                    spinner.tick("");
                } else {
                    line.update(&ping_line(&samples));
                }
            }
            res = &mut task => break res?,
        }
    };
    let samples = samples.lock().expect("ping samples lock");
    if samples.is_empty() {
        spinner.clear();
        return result;
    }
    line.finish(&ping_line(&samples));
    result
}

/// Runs a throughput phase: spawns the workers and drives the live bar with the average speed
/// so far until the deadline or until the workers finish (an NDT7 server ends the test itself).
/// The bar ends on the same average, so it doesn't jump when the phase finishes. With `drain`
/// the workers may finish their final exchange (bounded) before the average is taken.
async fn run_phase<F>(
    label: &'static str,
    above: Option<Finished>,
    duration: Duration,
    drain: bool,
    spawn: F,
) -> Result<Finished>
where
    F: FnOnce(Instant, &Arc<AtomicU64>) -> JoinSet<Result<()>>,
{
    let counter = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let deadline = start + duration;
    let mut set = spawn(deadline, &counter);
    let mut bar = SpeedBar::new(label, above);
    bar.update(0.0);

    // The average is timed from the first byte, so connection setup (several seconds on a
    // high-latency link) doesn't count as time spent transferring.
    let mut first_byte: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = Instant::now();
                let total = counter.load(Ordering::Relaxed);
                if first_byte.is_none() && total > 0 {
                    first_byte = Some(now);
                }
                bar.update(average_mbps(total, first_byte, now));
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
        // Keep redrawing while the last exchange finishes: the average falls as time passes
        // without new bytes, and showing that avoids a jump when the bar is frozen.
        let drain_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let now = Instant::now();
                    bar.update(average_mbps(counter.load(Ordering::Relaxed), first_byte, now));
                    if now >= drain_deadline {
                        break;
                    }
                }
                res = set.join_next() => if res.is_none() { break },
            }
        }
    }
    set.abort_all();
    Ok(bar.finish(average_mbps(
        counter.load(Ordering::Relaxed),
        first_byte,
        Instant::now(),
    )))
}

/// Average speed in Mbps over the time since the first byte; zero until there is one.
fn average_mbps(bytes: u64, first_byte: Option<Instant>, now: Instant) -> f64 {
    let secs = first_byte.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
    if secs > 0.0 {
        bytes as f64 * 8.0 / secs / 1_000_000.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_is_timed_from_first_byte() {
        let t0 = Instant::now();
        assert_eq!(average_mbps(0, None, t0), 0.0);
        assert_eq!(average_mbps(1_000, Some(t0), t0), 0.0);
        // 2.5 MB over 2 s is 10 Mbps
        let mbps = average_mbps(2_500_000, Some(t0), t0 + Duration::from_secs(2));
        assert!((mbps - 10.0).abs() < 1e-9, "{mbps}");
    }

    #[test]
    fn ping_line_counts_samples() {
        let one = [Duration::from_millis(12)];
        assert_eq!(
            ping_line(&one),
            "Ping      min 12.0 ms   mean 12.0 ms   max 12.0 ms   (1 sample)"
        );
        let three = [
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
        ];
        assert_eq!(
            ping_line(&three),
            "Ping      min 10.0 ms   mean 20.0 ms   max 30.0 ms   (3 samples)"
        );
    }
}
