use std::time::Instant;

use anyhow::Result;
use minifb::{Key, Window, WindowOptions};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::video_compositor::{ConnectionKind, GalleryFrame, TileFrame};

/// Pixel gap between tiles in the window.
const TILE_GAP: u32 = 20;

/// Padding around the outside of the gallery.
const OUTER_PAD: u32 = 16;

/// Extra pixel rows below each tile for the label.
const LABEL_HEIGHT: u32 = 32;

/// Vertical offset from top of label area to start drawing text.
const LABEL_Y_OFFSET: u32 = 4;

/// Font raster height for labels.
const FONT_HEIGHT: RasterHeight = RasterHeight::Size24;

/// Background colour for gaps (dark grey).
const BG_COLOR: u32 = 0x00181818;

/// Duration in seconds to show the red drop indicator after drops are detected.
const DROP_DOT_DURATION_SECS: f64 = 1.0;

/// Drop dot radius in pixels.
const DROP_DOT_RADIUS: u32 = 5;

/// Per-tile state for tracking video frame drops.
struct TileDropState {
    last_drop_count: u64,
    last_drop_time: Option<Instant>,
}

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
    let mut drop_states: Vec<TileDropState> = Vec::new();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        if let Some(ref win) = window
            && (!win.is_open() || win.is_key_down(Key::Escape))
        {
            cancel.cancel();
            break;
        }

        let gallery = match rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        if gallery.tiles.is_empty() {
            continue;
        }

        {

            let tile_w = gallery.tiles.iter().map(|t| t.frame.width).max().unwrap_or(640);
            let tile_h = gallery.tiles.iter().map(|t| t.frame.height).max().unwrap_or(480);

            let out_w = 2 * OUTER_PAD + gallery.cols * tile_w + (gallery.cols.saturating_sub(1)) * TILE_GAP;
            let out_h = 2 * OUTER_PAD + gallery.rows * (tile_h + LABEL_HEIGHT)
                + (gallery.rows.saturating_sub(1)) * TILE_GAP;

            // Create or resize window if needed.
            if window.is_none() || out_w != cur_w || out_h != cur_h {
                let win_w = (out_w / scale_factor).max(1);
                let win_h = (out_h / scale_factor).max(1);
                let win = Window::new(
                    "meshconf",
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
                pixel_buf = vec![BG_COLOR; (out_w * out_h) as usize];
            }

            // Fill entire buffer with background.
            pixel_buf.fill(BG_COLOR);

            // Ensure drop_states has enough entries.
            while drop_states.len() < gallery.tiles.len() {
                drop_states.push(TileDropState {
                    last_drop_count: 0,
                    last_drop_time: None,
                });
            }

            for (i, tile) in gallery.tiles.iter().enumerate() {
                let x0 = OUTER_PAD + tile.grid_col * (tile_w + TILE_GAP);
                let y0 = OUTER_PAD + tile.grid_row * (tile_h + LABEL_HEIGHT + TILE_GAP);

                // Blit tile pixels using row-based bulk copy.
                blit_tile(&mut pixel_buf, out_w, x0, y0, tile);

                // Update drop state and draw indicator if needed.
                let ds = &mut drop_states[i];
                if tile.video_drops > ds.last_drop_count {
                    ds.last_drop_time = Some(Instant::now());
                    ds.last_drop_count = tile.video_drops;
                }
                let show_dot = ds
                    .last_drop_time
                    .is_some_and(|t| t.elapsed().as_secs_f64() < DROP_DOT_DURATION_SECS);
                if show_dot {
                    draw_filled_circle(
                        &mut pixel_buf,
                        out_w,
                        out_h,
                        x0 + DROP_DOT_RADIUS + 4,
                        y0 + DROP_DOT_RADIUS + 4,
                        DROP_DOT_RADIUS,
                        0x00FF3333,
                    );
                }

                // Draw label below the tile.
                let label_y = y0 + tile.frame.height + LABEL_Y_OFFSET;
                draw_tile_label(
                    &mut pixel_buf,
                    out_w,
                    out_h,
                    x0,
                    label_y,
                    tile_w,
                    tile,
                );
            }

            if let Some(ref mut win) = window
                && let Err(e) =
                    win.update_with_buffer(&pixel_buf, out_w as usize, out_h as usize)
            {
                tracing::warn!("Window update error: {}", e);
                break;
            }
        }
    }
}

/// Blit a packed-u32 tile frame into the window pixel buffer using row copies.
fn blit_tile(buf: &mut [u32], buf_w: u32, x0: u32, y0: u32, tile: &TileFrame) {
    let tw = tile.frame.width as usize;
    let th = tile.frame.height as usize;
    let data = &tile.frame.data;

    for dy in 0..th {
        let src_start = dy * tw;
        let dst_start = ((y0 as usize) + dy) * (buf_w as usize) + (x0 as usize);
        buf[dst_start..dst_start + tw].copy_from_slice(&data[src_start..src_start + tw]);
    }
}

/// Draw name + connection kind label, centered within `tile_w` pixels.
fn draw_tile_label(
    buf: &mut [u32],
    buf_w: u32,
    buf_h: u32,
    x0: u32,
    y0: u32,
    tile_w: u32,
    tile: &TileFrame,
) {
    let name = if tile.name.is_empty() { "?" } else { &tile.name };
    let kind_str = match tile.conn_kind {
        ConnectionKind::Local => "(you)",
        ConnectionKind::Direct => "(direct)",
        ConnectionKind::Relay => "(relay)",
        ConnectionKind::Unknown => "(\u{2026})",
    };

    let name_color = 0x00FFFFFF; // white
    let kind_color = match tile.conn_kind {
        ConnectionKind::Local => 0x00888888,   // dim grey
        ConnectionKind::Direct => 0x0044CC44,  // green
        ConnectionKind::Relay => 0x00CCCC44,   // yellow
        ConnectionKind::Unknown => 0x00888888,  // dim grey
    };

    // Measure total width without allocating a formatted string.
    let space = " ";
    let text_w = measure_text(name) + measure_text(space) + measure_text(kind_str);

    // Center horizontally within tile_w.
    let start_x = x0 + tile_w.saturating_sub(text_w) / 2;

    // Draw name in bold white, then space, then kind in colour.
    let mut cx = start_x;
    cx = draw_text(buf, buf_w, buf_h, cx, y0, name, name_color, FontWeight::Bold);
    cx = draw_text(buf, buf_w, buf_h, cx, y0, space, name_color, FontWeight::Regular);
    let _ = draw_text(buf, buf_w, buf_h, cx, y0, kind_str, kind_color, FontWeight::Regular);
}

/// Measure the pixel width of a string at our chosen font size.
fn measure_text(text: &str) -> u32 {
    let char_w = get_raster_width(FontWeight::Regular, FONT_HEIGHT);
    (text.chars().count() as u32) * (char_w as u32)
}

/// Draw a string into the pixel buffer. Returns the x position after the last character.
fn draw_text(
    buf: &mut [u32],
    buf_w: u32,
    buf_h: u32,
    x: u32,
    y: u32,
    text: &str,
    color: u32,
    weight: FontWeight,
) -> u32 {
    let char_w = get_raster_width(weight, FONT_HEIGHT) as u32;
    let mut cx = x;

    for ch in text.chars() {
        if let Some(raster) = get_raster(ch, weight, FONT_HEIGHT) {
            let raster_data = raster.raster();
            for (row_idx, row) in raster_data.iter().enumerate() {
                let py = y + row_idx as u32;
                if py >= buf_h {
                    break;
                }
                for (col_idx, &intensity) in row.iter().enumerate() {
                    let px = cx + col_idx as u32;
                    if px >= buf_w {
                        break;
                    }
                    if intensity > 0 {
                        let idx = (py * buf_w + px) as usize;
                        if idx < buf.len() {
                            buf[idx] = blend_pixel(buf[idx], color, intensity);
                        }
                    }
                }
            }
        }
        cx += char_w;
    }
    cx
}

/// Alpha-blend a foreground colour onto a background pixel using intensity as alpha.
fn blend_pixel(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let inv_a = 255 - a;
    let r = ((fg >> 16 & 0xFF) * a + (bg >> 16 & 0xFF) * inv_a) / 255;
    let g = ((fg >> 8 & 0xFF) * a + (bg >> 8 & 0xFF) * inv_a) / 255;
    let b = ((fg & 0xFF) * a + (bg & 0xFF) * inv_a) / 255;
    r << 16 | g << 8 | b
}

/// Draw a filled circle at (cx, cy) with given radius and colour.
fn draw_filled_circle(
    buf: &mut [u32],
    buf_w: u32,
    buf_h: u32,
    cx: u32,
    cy: u32,
    radius: u32,
    color: u32,
) {
    let r = radius as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                let px = cx as i32 + dx;
                let py = cy as i32 + dy;
                if px >= 0 && py >= 0 && (px as u32) < buf_w && (py as u32) < buf_h {
                    let idx = (py as u32 * buf_w + px as u32) as usize;
                    buf[idx] = color;
                }
            }
        }
    }
}
