use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use std::sync::{Arc, Mutex};

use vc::audio::AudioEngine;
use vc::room::{MediaResources, Room};
use vc::video_compositor::{GalleryFrame, StatusInfo};
use vc::video_decode::RgbFrame;
use vc::{bandwidth, net, term, tui, video_capture, video_encode};
#[cfg(feature = "kitty")]
use vc::{kitty, video_display_kitty};
#[cfg(feature = "minifb")]
use vc::video_display_window;

#[derive(Parser)]
#[command(name = "vc", about = "Peer-to-peer video call in the terminal")]
struct Cli {
    /// Ticket to join an existing call (omit to start a new call)
    ticket: Option<String>,

    /// Use a generated test pattern instead of the real camera
    #[arg(long)]
    test_pattern: bool,

    /// Display video in the terminal using the Kitty graphics protocol
    /// instead of a native window
    #[cfg(feature = "kitty")]
    #[arg(long)]
    kitty: bool,

    /// Custom relay server URL (e.g. http://localhost:3340 for local dev).
    /// Omit to use iroh's default production relays.
    #[arg(long, conflicts_with = "no_relay")]
    relay: Option<String>,

    /// Disable relay servers entirely (direct connections only).
    #[arg(long, conflicts_with = "relay")]
    no_relay: bool,
}

fn main() -> Result<()> {
    // Ensure the terminal is restored if we panic or receive a signal.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = term::disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = tui::cleanup(&mut stdout);
        default_panic(info);
    }));

    // Restore terminal on SIGTERM / SIGHUP (SIGINT is handled via the
    // key-reader thread or tokio signal handler, but add it here too as
    // a safety net since we disable ISIG in raw mode).
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, restore_terminal_on_signal as *const () as libc::sighandler_t);
        }
    }

    let cli = Cli::parse();

    #[cfg(feature = "kitty")]
    let use_kitty = cli.kitty;
    #[cfg(not(feature = "kitty"))]
    let use_kitty = false;

    // Default to native window unless --kitty is requested.
    #[cfg(feature = "minifb")]
    let use_window = !use_kitty;
    #[cfg(not(feature = "minifb"))]
    let use_window = false;

    if use_window {
        #[cfg(feature = "minifb")]
        {
            init_tracing_file();

            let cancel = CancellationToken::new();
            let (display_tx, display_rx) =
                tokio::sync::mpsc::channel::<GalleryFrame>(2);

            let cancel_clone = cancel.clone();
            let rt_handle = std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
                rt.block_on(run_app(cli, cancel_clone, Some(display_tx), false))
            });

            video_display_window::run_main_thread(display_rx, cancel.clone());
            cancel.cancel();

            return rt_handle
                .join()
                .map_err(|_| anyhow::anyhow!("Runtime thread panicked"))?;
        }
    }

    {
        init_tracing_file();

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_app(cli, CancellationToken::new(), None, use_kitty))
    }
}

extern "C" fn restore_terminal_on_signal(sig: libc::c_int) {
    let _ = term::disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = tui::cleanup(&mut stdout);
    // Re-raise with default handler so the process exits with the right status.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn init_tracing_file() {
    use tracing_subscriber::EnvFilter;

    let log_dir = dirs::state_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("vc")
        .join("logs");
    std::fs::create_dir_all(&log_dir).ok();

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let pid = std::process::id();
    let log_path = log_dir.join(format!("vc-{}-{}.log", timestamp, pid));

    eprintln!("Logging to {}", log_path.display());

    let file = std::fs::File::create(&log_path).expect("failed to create log file");

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(file)
        .with_ansi(false)
        .init();
}

async fn run_app(
    cli: Cli,
    cancel: CancellationToken,
    window_display_tx: Option<mpsc::Sender<GalleryFrame>>,
    use_kitty_flag: bool,
) -> Result<()> {
    let joining = cli.ticket.is_some();
    let ticket_str = cli.ticket.clone();

    let use_window = window_display_tx.is_some();
    let use_tui = !use_window;

    let kitty_ok = if use_tui && use_kitty_flag {
        #[cfg(feature = "kitty")]
        { kitty::detect_kitty_support().unwrap_or(false) }
        #[cfg(not(feature = "kitty"))]
        { false }
    } else {
        false
    };

    let _raw_guard = if use_tui {
        term::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        tui::init(&mut stdout)?;
        Some(term::RawModeGuard)
    } else {
        None
    };

    // Key input handler
    if use_tui {
        let cancel_clone = cancel.clone();
        std::thread::spawn(move || loop {
            if cancel_clone.is_cancelled() {
                break;
            }
            if let Some(
                term::KeyPress::Char('q')
                | term::KeyPress::Char('Q')
                | term::KeyPress::CtrlC,
            ) = term::read_key(std::time::Duration::from_millis(100))
            {
                cancel_clone.cancel();
                break;
            }
        });
    } else {
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel_clone.cancel();
        });
    }

    // --- Audio engine (playback side starts immediately) ---
    let (_audio_engine, capture_rx, playback_tx) = match AudioEngine::start() {
        Ok((engine, rx, tx)) => (Some(engine), Some(rx), Some(tx)),
        Err(e) => {
            tracing::warn!("Could not start audio engine: {}", e);
            (None, None, None)
        }
    };

    // Shared camera error for display in the TUI.
    let camera_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Shared status line rendered by the video display loop at the bottom
    // of the terminal. Updated from the main task as the app progresses
    // through setup stages.
    let status_line: Arc<Mutex<Option<StatusInfo>>> = Arc::new(Mutex::new(None));

    // --- Display (persists for lifetime of app) ---
    #[allow(unused_mut)]
    let mut display_handle: Option<tokio::task::JoinHandle<()>> = None;
    let display_tx: Option<mpsc::Sender<GalleryFrame>> = if use_window {
        window_display_tx
    } else if kitty_ok {
        #[cfg(feature = "kitty")]
        {
            let (tx, rx) = mpsc::channel::<GalleryFrame>(2);
            let cancel_clone = cancel.clone();
            let camera_error_clone = camera_error.clone();
            let status_line_clone = status_line.clone();
            display_handle = Some(tokio::spawn(async move {
                if let Err(e) = video_display_kitty::run_video_display(rx, cancel_clone.clone(), status_line_clone, camera_error_clone).await
                    && !cancel_clone.is_cancelled() {
                        tracing::warn!("Video display error: {}", e);
                    }
            }));
            Some(tx)
        }
        #[cfg(not(feature = "kitty"))]
        { None }
    } else {
        None
    };

    // --- Create compositor channel up-front so we can share it with
    //     both the Room (for remote peer frames) and the local camera
    //     preview tee task.
    let (compositor_tx, compositor_rx) =
        mpsc::channel::<(vc::room::PeerId, RgbFrame)>(10);

    // --- Bind endpoint (instant — no network wait) ---
    tracing::info!("Binding iroh endpoint...");
    let endpoint = net::bind_endpoint(cli.relay.as_deref(), cli.no_relay).await?;
    tracing::info!(id = %endpoint.id(), "Endpoint bound, local addr: {:?}", endpoint.bound_sockets());

    // --- Start video capture immediately (no relay needed) ---
    let encoder_config = bandwidth::EncoderConfig::new();
    let video_encoder = {
        let use_test_pattern = cli.test_pattern;
        let camera_error = camera_error.clone();
        let cancel = cancel.clone();

        let video_frame_rx = if use_test_pattern {
            Some(video_capture::start_test_pattern(cancel.clone(), Some(15)))
        } else {
            match video_capture::VideoCapture::start(cancel.clone()) {
                Ok((capture, rx)) => {
                    std::mem::forget(capture);
                    Some(rx)
                }
                Err(e) => {
                    tracing::warn!("Could not start camera: {}", e);
                    *camera_error.lock().unwrap() = Some(format!("Camera error: {}", e));
                    None
                }
            }
        };

        match video_frame_rx {
            Some(rx) => {
                // Tee capture frames: one copy goes to the compositor as
                // a local preview tile, the other feeds the encoder.
                let our_peer_id = endpoint.id();
                let encoder_rx = video_capture::tee_capture(
                    rx,
                    our_peer_id,
                    compositor_tx.clone(),
                    cancel.clone(),
                );

                match video_encode::VideoEncoder::start(encoder_rx, Some(encoder_config.clone()), cancel.clone()) {
                    Ok(enc) => Some(enc),
                    Err(e) => {
                        tracing::warn!("Failed to start video encoder: {}", e);
                        *camera_error.lock().unwrap() =
                            Some(format!("Video encoder error: {}", e));
                        None
                    }
                }
            }
            None => None,
        }
    };

    let our_name = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();

    let enc_config_for_room = if video_encoder.is_some() { Some(encoder_config) } else { None };
    let media = MediaResources {
        audio_capture_rx: capture_rx,
        video_encoder,
        playback_tx,
        display_tx,
        compositor_channel: Some((compositor_tx, compositor_rx)),
        encoder_config: enc_config_for_room,
        our_name,
    };

    let mut room = Room::new(endpoint.clone(), cancel.clone(), media);

    // --- Wait for relay discovery (with timeout + cancel) ---
    // Camera is already running and showing self-preview while we wait.
    // Skip when relays are disabled — there's nothing to discover.
    if !cli.no_relay {
        *status_line.lock().unwrap() = Some(StatusInfo {
            message: "Setting up network connection…".into(),
            copyable: None,
        });
        if !use_tui {
            eprintln!("Setting up network connection...");
        }
        tracing::info!("Waiting for endpoint to come online (relay discovery)...");
        let addr_before = endpoint.addr();
        tracing::info!(
            "Endpoint addr before online: relay_urls={:?}, direct_addrs={:?}",
            addr_before.relay_urls().collect::<Vec<_>>(),
            addr_before.ip_addrs().collect::<Vec<_>>(),
        );
        tokio::select! {
            _ = endpoint.online() => {
                tracing::info!("endpoint.online() returned");
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                tracing::warn!("Timed out waiting for relay discovery after 10s, proceeding without relay");
            }
            _ = cancel.cancelled() => {
                anyhow::bail!("Cancelled during network setup");
            }
        }
    }

    let addr_after = endpoint.addr();
    tracing::info!(
        "Endpoint addr after online: relay_urls={:?}, direct_addrs={:?}",
        addr_after.relay_urls().collect::<Vec<_>>(),
        addr_after.ip_addrs().collect::<Vec<_>>(),
    );

    let our_ticket = match net::endpoint_ticket(&endpoint) {
        Ok(ticket) => {
            let has_relay = addr_after.relay_urls().next().is_some();
            let msg = if has_relay {
                "Share this ticket with your peer. Waiting for connection…"
            } else {
                "⚠ No relay server (local connections only). Share this ticket:"
            };
            if !joining {
                *status_line.lock().unwrap() = Some(StatusInfo {
                    message: msg.into(),
                    copyable: Some(ticket.clone()),
                });
                if !use_tui {
                    eprintln!("{}", msg);
                    eprintln!("{}", ticket);
                }
            }
            ticket
        }
        Err(e) => {
            tracing::warn!("Failed to generate ticket: {}", e);
            String::new()
        }
    };

    // --- Resolve initial peer address (if joining) ---
    let initial_peer_addr = match ticket_str {
        Some(ref t) => Some(net::ticket_to_addr(t)?),
        None => None,
    };

    if joining && use_tui {
        let mut stdout = std::io::stdout();
        tui::clear(&mut stdout)?;
        if !kitty_ok {
            tui::draw_centered_box(
                &mut stdout,
                "vc",
                &[
                    "Terminal does not support Kitty graphics.",
                    "Audio only — video will not be displayed.",
                ],
            )?;
        }
    }

    // Show our ticket so additional peers can join via us, regardless of
    // whether we started the call or joined it.
    *status_line.lock().unwrap() = Some(StatusInfo {
        message: "Share this ticket to invite others.".into(),
        copyable: Some(our_ticket),
    });

    // --- Run ---
    room.run(initial_peer_addr).await?;

    // Wait for the display loop to finish before restoring the terminal.
    // Once cancel fires the compositor task exits, dropping its display
    // sender, which causes the display loop to see a disconnected channel
    // and break out — cleaning up Kitty images on the way.  If we don't
    // wait here, tui::cleanup may restore the main screen buffer while
    // the display loop is still writing base64-encoded Kitty image data.
    drop(room);
    if let Some(handle) = display_handle {
        let _ = handle.await;
    }

    // Clean up TUI
    if use_tui {
        let mut stdout = std::io::stdout();
        let _ = tui::cleanup(&mut stdout);
    }

    Ok(())
}
