use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use vc::audio::AudioEngine;
use vc::room::{MediaResources, Room};
use vc::video_compositor::GalleryFrame;
use vc::{
    kitty, net, term, tui, video_capture, video_display, video_display_window,
    video_encode,
};

#[derive(Parser)]
#[command(name = "vc", about = "Peer-to-peer video call in the terminal")]
struct Cli {
    /// Ticket to join an existing call (omit to host)
    ticket: Option<String>,

    /// Use a generated test pattern instead of the real camera
    #[arg(long)]
    test_pattern: bool,

    /// Display received video in a native window instead of the terminal
    #[arg(long)]
    window: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let use_window = cli.window;

    if use_window {
        init_tracing_stderr();

        let cancel = CancellationToken::new();
        let (display_tx, display_rx) =
            tokio::sync::mpsc::channel::<GalleryFrame>(2);

        let cancel_clone = cancel.clone();
        let rt_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(run_app(cli, cancel_clone, Some(display_tx)))
        });

        video_display_window::run_main_thread(display_rx, cancel.clone());
        cancel.cancel();

        rt_handle
            .join()
            .map_err(|_| anyhow::anyhow!("Runtime thread panicked"))?
    } else {
        if std::env::var("RUST_LOG").is_ok() {
            init_tracing_stderr();
        }

        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_app(cli, CancellationToken::new(), None))
    }
}

fn init_tracing_stderr() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
}

async fn run_app(
    cli: Cli,
    cancel: CancellationToken,
    window_display_tx: Option<mpsc::Sender<GalleryFrame>>,
) -> Result<()> {
    let use_window = window_display_tx.is_some();
    let use_tui = !use_window;

    let kitty_ok = if use_tui {
        kitty::detect_kitty_support().unwrap_or(false)
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

    // --- Audio engine (playback side starts immediately; capture may be deferred) ---
    let (_audio_engine, capture_rx, playback_tx) = match AudioEngine::start() {
        Ok((engine, rx, tx)) => (Some(engine), Some(rx), Some(tx)),
        Err(e) => {
            tracing::warn!("Could not start audio engine: {}", e);
            (None, None, None)
        }
    };

    // --- Create endpoint ---
    let endpoint = net::create_endpoint().await?;
    let is_host = cli.ticket.is_none();
    let ticket_str = cli.ticket.clone();

    // Generate ticket (host only) — needed both for the UI and the display overlay.
    let host_ticket: Option<String> = if is_host {
        Some(net::endpoint_ticket(&endpoint)?)
    } else {
        None
    };

    // --- Show ticket / connecting UI ---
    if let Some(ref ticket) = host_ticket {
        if use_tui {
            let mut stdout = std::io::stdout();
            let _ = tui::draw_centered_box_with_copyable(
                &mut stdout,
                "vc",
                &[
                    "Share this ticket with your peer:",
                    "Select and copy the text below.",
                    "",
                    "Waiting for connection...",
                ],
                ticket,
            );
        } else {
            eprintln!("Share this ticket with your peer:");
            eprintln!("{}", ticket);
            eprintln!("Waiting for connection...");
        }
    } else if use_tui {
        let mut stdout = std::io::stdout();
        let _ = tui::draw_centered_box(&mut stdout, "vc", &["Connecting to host..."]);
    }

    // --- Display (persists for lifetime of app) ---
    let display_tx: Option<mpsc::Sender<GalleryFrame>> = if use_window {
        window_display_tx
    } else if kitty_ok {
        let (tx, rx) = mpsc::channel::<GalleryFrame>(2);
        let cancel_clone = cancel.clone();
        let ticket_for_display = host_ticket.clone();
        tokio::spawn(async move {
            if let Err(e) = video_display::run_video_display(rx, cancel_clone.clone(), ticket_for_display).await
                && !cancel_clone.is_cancelled() {
                    tracing::warn!("Video display error: {}", e);
                }
        });
        Some(tx)
    } else {
        None
    };

    // --- Create Room with media resources ---
    // For the host, defer camera/encoder startup until the first peer joins.
    // For joiners, start immediately.
    let start_video_capture = {
        let use_test_pattern = cli.test_pattern;
        let cancel = cancel.clone();
        move || -> Option<video_encode::VideoEncoder> {
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
                        None
                    }
                }
            };
            match video_frame_rx {
                Some(rx) => match video_encode::VideoEncoder::start(rx, cancel.clone()) {
                    Ok(enc) => Some(enc),
                    Err(e) => {
                        tracing::warn!("Failed to start video encoder: {}", e);
                        None
                    }
                },
                None => None,
            }
        }
    };

    let media = if is_host {
        MediaResources {
            audio_capture_rx: capture_rx,
            video_encoder: None,
            playback_tx,
            display_tx,
            on_first_peer: Some(Box::new(move || {
                vc::room::CaptureResources {
                    audio_capture_rx: None,
                    video_encoder: start_video_capture(),
                }
            })),
        }
    } else {
        let video_encoder = start_video_capture();
        MediaResources {
            audio_capture_rx: capture_rx,
            video_encoder,
            playback_tx,
            display_tx,
            on_first_peer: None,
        }
    };

    let mut room = Room::new(endpoint.clone(), is_host, cancel.clone(), media);

    // --- Run ---
    if is_host {
        // Clear UI once first peer connects (handled by room events — for now just clear)
        room.run_host().await?;
    } else {
        let host_addr = net::ticket_to_addr(ticket_str.as_ref().unwrap())?;
        if use_tui {
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
        room.run_joiner(host_addr).await?;
    }

    // Clean up TUI
    if use_tui {
        let mut stdout = std::io::stdout();
        let _ = tui::cleanup(&mut stdout);
    }

    Ok(())
}
