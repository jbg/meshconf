use anyhow::Result;
use minifb::{Key, Window, WindowOptions};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::video_compositor::GalleryFrame;

/// Pixel gap between tiles in the window (in pixels).
const TILE_GAP: u32 = 4;

/// Run the window display loop on the **main thread**.
///
/// macOS requires all AppKit/window operations on the main thread.
pub fn run_main_thread(
    rx: mpsc::Receiver<GalleryFrame>,
    cancel: CancellationToken,
) {
    display_loop(rx, cancel);
}

/// Stub for the async interface.
pub async fn run_video_display_window(
    mut rx: mpsc::Receiver<GalleryFrame>,
    cancel: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            frame = rx.recv() => {
                if frame.is_none() { break; }
            }
        }
    }
    Ok(())
}

/// Query the main display's backing scale factor (e.g. 2.0 on Retina).
#[cfg(target_os = "macos")]
fn display_scale_factor() -> u32 {
    use objc2_app_kit::NSScreen;
    use objc2::MainThreadMarker;
    let Some(mtm) = MainThreadMarker::new() else {
        return 1;
    };
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        return 1;
    };
    let factor = screen.backingScaleFactor();
    (factor as u32).max(1)
}

#[cfg(not(target_os = "macos"))]
fn display_scale_factor() -> u32 {
    1
}

fn display_loop(mut rx: mpsc::Receiver<GalleryFrame>, cancel: CancellationToken) {
    let mut window: Option<Window> = None;
    let mut pixel_buf: Vec<u32> = Vec::new();
    let mut cur_w: u32 = 0;
    let mut cur_h: u32 = 0;
    let scale_factor = display_scale_factor();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        if let Some(ref win) = window
            && (!win.is_open() || win.is_key_down(Key::Escape)) {
                cancel.cancel();
                break;
            }

        let gallery = match rx.try_recv() {
            Ok(f) => Some(f),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        };

        if let Some(gallery) = gallery {
            if gallery.tiles.is_empty() {
                continue;
            }

            let tile_w = gallery.tiles[0].frame.width;
            let tile_h = gallery.tiles[0].frame.height;

            let out_w = gallery.cols * tile_w + (gallery.cols.saturating_sub(1)) * TILE_GAP;
            let out_h = gallery.rows * tile_h + (gallery.rows.saturating_sub(1)) * TILE_GAP;

            // Create or resize window if needed
            if window.is_none() || out_w != cur_w || out_h != cur_h {
                let win_w = (out_w / scale_factor).max(1);
                let win_h = (out_h / scale_factor).max(1);
                let win = Window::new(
                    "vc",
                    win_w as usize,
                    win_h as usize,
                    WindowOptions {
                        resize: false,
                        scale_mode: minifb::ScaleMode::AspectRatioStretch,
                        ..WindowOptions::default()
                    },
                );
                match win {
                    Ok(w) => {
                        window = Some(w);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create window: {}", e);
                        break;
                    }
                }
                cur_w = out_w;
                cur_h = out_h;
            }

            // Composite all tiles into a single pixel buffer
            pixel_buf.resize((out_w * out_h) as usize, 0x00202020); // dark bg for gaps

            for tile in &gallery.tiles {
                let x0 = tile.grid_col * (tile_w + TILE_GAP);
                let y0 = tile.grid_row * (tile_h + TILE_GAP);
                let data = &tile.frame.data;

                for dy in 0..tile.frame.height {
                    for dx in 0..tile.frame.width {
                        let src_idx = ((dy * tile.frame.width + dx) * 3) as usize;
                        let dst_x = x0 + dx;
                        let dst_y = y0 + dy;
                        if dst_x < out_w && dst_y < out_h && src_idx + 3 <= data.len() {
                            let dst_idx = (dst_y * out_w + dst_x) as usize;
                            pixel_buf[dst_idx] = (data[src_idx] as u32) << 16
                                | (data[src_idx + 1] as u32) << 8
                                | data[src_idx + 2] as u32;
                        }
                    }
                }
            }

            if let Some(ref mut win) = window
                && let Err(e) =
                    win.update_with_buffer(&pixel_buf, out_w as usize, out_h as usize)
            {
                    tracing::warn!("Window update error: {}", e);
                    break;
                }
        } else if let Some(ref mut win) = window {
            win.update();
            std::thread::sleep(std::time::Duration::from_millis(1));
        } else {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
