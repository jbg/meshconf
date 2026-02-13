use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use iroh::Endpoint;
use iroh::endpoint::Connection;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use meshconf::broadcaster::Broadcaster;
use meshconf::protocol::MediaObject;
use meshconf::{dispatch, net, video_capture, video_decode, video_encode};

#[derive(Parser)]
#[command(
    name = "vc-bench",
    about = "Video pipeline benchmark for vc.\n\n\
             Runs the full encode→QUIC→decode pipeline over a local loopback\n\
             connection using a test pattern, and reports throughput and latency."
)]
struct Cli {
    /// Benchmark duration in seconds (after warmup)
    #[arg(long, default_value = "10")]
    duration: u64,

    /// Warmup duration in seconds (lets encoder/decoder stabilise)
    #[arg(long, default_value = "2")]
    warmup: u64,

    /// Remove the 15fps cap and measure maximum pipeline throughput
    #[arg(long)]
    uncapped: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_bench(cli))
}

/// Set up a loopback QUIC connection via iroh: two endpoints, one connects to the other.
/// Returns (sender_conn, receiver_conn).
async fn setup_loopback() -> Result<(Connection, Connection)> {
    let ep_a = Endpoint::builder()
        .alpns(vec![net::ALPN.to_vec()])
        .bind()
        .await?;
    let ep_b = Endpoint::builder()
        .alpns(vec![net::ALPN.to_vec()])
        .bind()
        .await?;

    ep_a.online().await;
    let addr_a = ep_a.addr();

    let (conn_b, conn_a) = tokio::try_join!(
        async { ep_b.connect(addr_a, net::ALPN).await.map_err(anyhow::Error::from) },
        async {
            ep_a.accept()
                .await
                .ok_or_else(|| anyhow::anyhow!("No incoming connection"))?
                .await
                .map_err(anyhow::Error::from)
        }
    )?;

    // Keep endpoints alive for the lifetime of the connections.
    std::mem::forget(ep_a);
    std::mem::forget(ep_b);

    Ok((conn_b, conn_a))
}

async fn run_bench(cli: Cli) -> Result<()> {
    let cancel = CancellationToken::new();
    let warmup = Duration::from_secs(cli.warmup);
    let duration = Duration::from_secs(cli.duration);

    eprintln!("Setting up loopback connection...");
    let (conn_send, conn_recv) = setup_loopback().await?;
    eprintln!("Loopback connected.");

    // --- Sender side: test pattern → encode → QUIC ---

    let max_fps = if cli.uncapped { None } else { Some(15) };
    let frame_rx = video_capture::start_test_pattern(cancel.clone(), max_fps);

    // Encoder — sends encoded frames over conn_send.
    // The encoder assigns monotonic group_ids starting from 0.
    {
        let cancel = cancel.clone();
        let conn = conn_send.clone();
        tokio::spawn(async move {
            match video_encode::VideoEncoder::start(frame_rx, None, cancel.clone()) {
                Ok(enc) => {
                    let bc = Broadcaster::new_with_keyframe(enc.force_keyframe_flag());
                    // Use a dummy peer ID — just need the connection.
                    bc.add_peer(conn.remote_id(), conn);
                    enc.send_loop(bc, None, cancel).await;
                }
                Err(e) => tracing::warn!("Encoder error: {}", e),
            }
        });
    }

    // --- Receiver side: QUIC → dispatch → decode → measure ---

    // Dispatcher channels (larger capacity to avoid measurement artifacts)
    let (audio_tx, _audio_rx) = mpsc::channel::<MediaObject>(64);
    let (video_tx, video_payload_rx) = mpsc::channel::<MediaObject>(32);

    // Dispatcher
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            dispatch::run_dispatcher(conn_recv, audio_tx, video_tx, meshconf::video_compositor::VideoDropCounter::new(), None, None, cancel).await;
        });
    }

    // Intercept video payloads to record send-side timestamps keyed by group_id,
    // and measure encoded frame sizes.
    let send_times: Arc<Mutex<HashMap<u64, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let encoded_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let measuring = Arc::new(AtomicBool::new(false));
    let sent_count = Arc::new(AtomicU64::new(0));
    let (video_payload_tx2, video_payload_rx2) = mpsc::channel::<MediaObject>(32);
    {
        let send_times = send_times.clone();
        let encoded_sizes = encoded_sizes.clone();
        let measuring = measuring.clone();
        let sent_count = sent_count.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut rx = video_payload_rx;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    obj = rx.recv() => {
                        match obj {
                            Some(obj) => {
                                if measuring.load(Ordering::Relaxed) {
                                    send_times.lock().unwrap().insert(obj.group_id, Instant::now());
                                    encoded_sizes.lock().unwrap().push(obj.payload.len());
                                    sent_count.fetch_add(1, Ordering::Relaxed);
                                }
                                if video_payload_tx2.send(obj).await.is_err() { break; }
                            }
                            None => break,
                        }
                    }
                }
            }
        });
    }

    // Decoder
    let (tagged_tx, mut tagged_rx) = mpsc::channel::<(iroh::PublicKey, video_decode::RgbFrame)>(8);
    let (display_tx_internal, mut display_rx) = mpsc::channel::<video_decode::RgbFrame>(8);
    {
        // Adapter: strip PeerId tag for bench compatibility
        tokio::spawn(async move {
            while let Some((_pid, frame)) = tagged_rx.recv().await {
                let _ = display_tx_internal.send(frame).await;
            }
        });
    }
    {
        let cancel = cancel.clone();
        let dummy_peer_id = conn_send.remote_id();
        tokio::spawn(async move {
            if let Err(e) =
                video_decode::run_video_decoder(dummy_peer_id, video_payload_rx2, tagged_tx, meshconf::video_compositor::VideoDropCounter::new(), cancel)
                    .await
            {
                tracing::warn!("Decoder error: {}", e);
            }
        });
    }

    // --- Measurement loop ---

    eprintln!(
        "Pipeline running. Warming up for {}s, then measuring for {}s...",
        cli.warmup, cli.duration
    );

    let start = Instant::now();
    let mut frame_count: u64 = 0;
    let mut latencies: Vec<Duration> = Vec::new();
    let mut inter_frame_times: Vec<Duration> = Vec::new();
    let mut last_frame_time: Option<Instant> = None;
    let mut fps_windows: Vec<f64> = Vec::new();
    let mut window_start = start + warmup;
    let mut window_frames: u64 = 0;
    let mut warmup_done = false;

    loop {
        tokio::select! {
            frame = display_rx.recv() => {
                match frame {
                    Some(rgb_frame) => {
                        let now = Instant::now();
                        let elapsed = now.duration_since(start);

                        if elapsed < warmup {
                            // Drain send_times during warmup
                            send_times.lock().unwrap().remove(&rgb_frame.group_id);
                            continue;
                        }

                        if !warmup_done {
                            warmup_done = true;
                            window_start = now;
                            window_frames = 0;
                            // Start tracking on the intercept side
                            measuring.store(true, Ordering::Relaxed);
                            eprintln!("Warmup complete. Measuring...");
                        }

                        if elapsed > warmup + duration {
                            // Record final window
                            let window_dur = now.duration_since(window_start).as_secs_f64();
                            if window_dur > 0.1 && window_frames > 0 {
                                fps_windows.push(window_frames as f64 / window_dur);
                            }
                            break;
                        }

                        frame_count += 1;
                        window_frames += 1;

                        // Record FPS windows every 1 second
                        if now.duration_since(window_start) >= Duration::from_secs(1) {
                            let window_dur = now.duration_since(window_start).as_secs_f64();
                            fps_windows.push(window_frames as f64 / window_dur);
                            window_start = now;
                            window_frames = 0;
                        }

                        // Latency: look up send timestamp by group_id
                        if let Some(send_time) = send_times.lock().unwrap().remove(&rgb_frame.group_id) {
                            latencies.push(now.duration_since(send_time));
                        }

                        // Inter-frame time
                        if let Some(last) = last_frame_time {
                            inter_frame_times.push(now.duration_since(last));
                        }
                        last_frame_time = Some(now);
                    }
                    None => {
                        eprintln!("Pipeline closed unexpectedly.");
                        break;
                    }
                }
            }
            _ = cancel.cancelled() => break,
        }
    }

    // Stop counting before tearing down the pipeline.
    measuring.store(false, Ordering::Relaxed);
    let total_sent = sent_count.load(Ordering::Relaxed);
    let dropped = total_sent.saturating_sub(frame_count);

    cancel.cancel();

    let sizes = encoded_sizes.lock().unwrap();

    print_results(
        frame_count,
        dropped,
        duration,
        &latencies,
        &inter_frame_times,
        &fps_windows,
        &sizes,
        cli.uncapped,
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_results(
    frame_count: u64,
    dropped: u64,
    duration: Duration,
    latencies: &[Duration],
    inter_frame_times: &[Duration],
    fps_windows: &[f64],
    encoded_sizes: &[usize],
    uncapped: bool,
) {
    println!();
    println!("=== vc video pipeline benchmark ===");
    if uncapped {
        println!(
            "Resolution: {}×{}, UNCAPPED (max throughput)",
            video_capture::ENCODE_WIDTH,
            video_capture::ENCODE_HEIGHT
        );
    } else {
        println!(
            "Resolution: {}×{}, Target: 15 fps",
            video_capture::ENCODE_WIDTH,
            video_capture::ENCODE_HEIGHT
        );
    }
    println!("Duration:  {:.1}s", duration.as_secs_f64());
    println!();

    // --- FPS ---
    let total = frame_count + dropped;
    let avg_fps = frame_count as f64 / duration.as_secs_f64();
    println!("Decoded frames:  {} / {} sent ({} dropped)", frame_count, total, dropped);
    if !fps_windows.is_empty() {
        let min_fps = fps_windows.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_fps = fps_windows
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        println!(
            "Decoded FPS:     {:.1} avg  ({:.1} min, {:.1} max)  [1s windows]",
            avg_fps, min_fps, max_fps
        );
    } else {
        println!("Decoded FPS:     {:.1} avg", avg_fps);
    }

    // --- Inter-frame timing (jitter) ---
    if !inter_frame_times.is_empty() {
        let avg_ift =
            inter_frame_times.iter().sum::<Duration>() / inter_frame_times.len() as u32;
        let avg_ms = avg_ift.as_secs_f64() * 1000.0;
        let variance = inter_frame_times
            .iter()
            .map(|d| {
                let diff = d.as_secs_f64() * 1000.0 - avg_ms;
                diff * diff
            })
            .sum::<f64>()
            / inter_frame_times.len() as f64;
        let stddev = variance.sqrt();
        println!(
            "Inter-frame:     {:.1}ms avg, {:.1}ms stddev",
            avg_ms, stddev
        );
    }

    // --- Pipeline latency ---
    if !latencies.is_empty() {
        let mut sorted: Vec<f64> = latencies
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let min = sorted[0];
        let p50 = sorted[sorted.len() / 2];
        let p95 = sorted[((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1)];
        let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)];
        let max = sorted[sorted.len() - 1];
        println!(
            "Latency:         {:.1}ms avg, {:.1}ms p50, {:.1}ms p95, {:.1}ms p99  ({:.1}–{:.1}ms)",
            avg, p50, p95, p99, min, max
        );
    }

    // --- Encoded frame sizes ---
    if !encoded_sizes.is_empty() {
        let avg_size = encoded_sizes.iter().sum::<usize>() as f64 / encoded_sizes.len() as f64;
        let min_size = *encoded_sizes.iter().min().unwrap();
        let max_size = *encoded_sizes.iter().max().unwrap();
        println!(
            "Encoded size:    {:.1} KB avg  ({:.1} KB min, {:.1} KB max)",
            avg_size / 1024.0,
            min_size as f64 / 1024.0,
            max_size as f64 / 1024.0
        );
    }

    println!();
}
