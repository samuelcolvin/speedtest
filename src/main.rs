mod bar;
mod cloudflare;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use tokio::task::JoinSet;

use bar::{SpeedBar, Spinner};

/// Measure ping, download and upload speed against Cloudflare.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Seconds to run each of the download and upload phases.
    #[arg(long, default_value_t = 10)]
    duration: u64,
    /// Number of parallel connections per phase.
    #[arg(long, default_value_t = 4)]
    connections: usize,
    /// Number of latency samples.
    #[arg(long, default_value_t = 10)]
    pings: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = cloudflare::client()?;

    run_ping(&client, args.pings).await?;

    let duration = Duration::from_secs(args.duration);
    // Download streams are torn down at the deadline; every byte counted has arrived.
    run_phase("Download", duration, false, |deadline, counter| {
        cloudflare::download(&client, args.connections, deadline, counter)
    })
    .await?;
    // Upload bodies end themselves at the deadline, so wait for the server to acknowledge
    // the buffered tail rather than counting bytes that never left the machine.
    run_phase("Upload", duration, true, |deadline, counter| {
        cloudflare::upload(&client, args.connections, deadline, counter)
    })
    .await?;
    Ok(())
}

/// Runs the latency phase behind a spinner that shows immediately, then prints min/mean/max.
async fn run_ping(client: &reqwest::Client, count: usize) -> Result<()> {
    let done = Arc::new(AtomicUsize::new(0));
    let mut spinner = Spinner::new("Ping");
    spinner.tick(&format!("0/{count}"));

    let mut task = tokio::spawn(cloudflare::ping(client.clone(), count, done.clone()));
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let samples = loop {
        tokio::select! {
            _ = tick.tick() => spinner.tick(&format!("{}/{count}", done.load(Ordering::Relaxed))),
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
/// speed until the deadline, then freezes the bar at the overall average. With `drain` the
/// workers are allowed to finish their final request (bounded) before the average is taken.
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
