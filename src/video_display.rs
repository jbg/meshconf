use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::kitty;
use crate::term;
use crate::video_compositor::{GalleryFrame, CELL_GAP_COLS, CELL_GAP_ROWS};

/// Receives gallery frames and renders each tile to the terminal via Kitty graphics.
///
/// Each tile is transmitted and placed at the correct grid position so that
/// the terminal background shows through the gaps between tiles.
///
/// If `ticket` is provided it is shown at the bottom of the terminal so the
/// host can share it with additional participants while the call is running.
pub async fn run_video_display(
    rx: mpsc::Receiver<GalleryFrame>,
    cancel: CancellationToken,
    ticket: Option<String>,
) -> Result<()> {
    let handle = tokio::task::spawn_blocking(move || {
        display_loop(rx, cancel, ticket);
    });

    handle.await?;
    Ok(())
}

/// Image IDs: we reserve a pair (back/front) per tile slot.
/// Slot i uses IDs (i*2 + 1) and (i*2 + 2).
const MAX_TILES: usize = 9;

fn display_loop(mut rx: mpsc::Receiver<GalleryFrame>, cancel: CancellationToken, ticket: Option<String>) {
    let mut stdout = std::io::stdout();
    // Track which back-buffer ID to use per slot.
    let mut back_toggle: [bool; MAX_TILES] = [false; MAX_TILES];
    // Track last grid layout to clear stale images on layout change.
    let mut last_tile_count: usize = 0;
    let mut ticket_drawn = false;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let gallery = match rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        // If tile count changed, clear everything (text + images).
        if gallery.tiles.len() != last_tile_count {
            use std::io::Write;
            let _ = write!(stdout, "\x1b[2J"); // clear all text
            let _ = stdout.flush();
            for i in 0..(MAX_TILES * 2 + 2) {
                let _ = kitty::delete_image(&mut stdout, (i + 1) as u32);
            }
            last_tile_count = gallery.tiles.len();
            ticket_drawn = false; // redraw ticket after layout change
        }

        // Nothing to draw — screen was just cleared above.
        if gallery.tiles.is_empty() {
            continue;
        }

        // Compute grid cell positions in terminal cells.
        let ws = match term::window_size() {
            Ok(ws) => ws,
            Err(_) => continue,
        };

        if ws.x_pixel == 0 || ws.y_pixel == 0 || ws.cols == 0 || ws.rows == 0 {
            continue;
        }

        let term_cell_w = ws.x_pixel as u32 / ws.cols as u32;
        let term_cell_h = ws.y_pixel as u32 / ws.rows as u32;
        if term_cell_w == 0 || term_cell_h == 0 {
            continue;
        }

        // All tiles are the same pixel size (first tile's dimensions).
        let tile_px_w = gallery.tiles.first().map(|t| t.frame.width).unwrap_or(640);
        let tile_px_h = gallery.tiles.first().map(|t| t.frame.height).unwrap_or(480);

        // Terminal cells occupied by one tile.
        let tile_cols = tile_px_w.div_ceil(term_cell_w);
        let tile_rows = tile_px_h.div_ceil(term_cell_h);

        // Total gallery size in terminal cells.
        let gap_c = CELL_GAP_COLS as u32;
        let gap_r = CELL_GAP_ROWS as u32;
        let total_cols = gallery.cols * tile_cols + (gallery.cols - 1) * gap_c;
        let total_rows = gallery.rows * tile_rows + (gallery.rows - 1) * gap_r;

        // Top-left offset to center the gallery in the terminal.
        let origin_col = (ws.cols as u32).saturating_sub(total_cols) / 2;
        let origin_row = (ws.rows as u32).saturating_sub(total_rows) / 2;

        for (i, tile) in gallery.tiles.iter().enumerate() {
            if i >= MAX_TILES {
                break;
            }

            let slot = i;
            let (id_a, id_b) = ((slot * 2 + 1) as u32, (slot * 2 + 2) as u32);
            let back_id = if back_toggle[slot] { id_b } else { id_a };
            back_toggle[slot] = !back_toggle[slot];

            // Transmit tile image to back buffer.
            if let Err(e) = kitty::transmit_image(
                &mut stdout,
                back_id,
                tile.frame.width,
                tile.frame.height,
                &tile.frame.data,
            ) {
                tracing::warn!("Kitty transmit error for tile {}: {}", i, e);
                continue;
            }

            // Compute this tile's terminal cell position.
            let col = origin_col + tile.grid_col * (tile_cols + gap_c);
            let row = origin_row + tile.grid_row * (tile_rows + gap_r);

            if let Err(e) =
                kitty::display_image_at(&mut stdout, back_id, col as u16, row as u16)
            {
                tracing::warn!("Kitty display error for tile {}: {}", i, e);
            }
        }

        // Draw ticket once at the bottom of the terminal.
        if !ticket_drawn
            && let Some(ref ticket) = ticket {
                use std::io::Write;
                // Start high enough to fit 3-4 lines of wrapped ticket text.
                let row = ws.rows.saturating_sub(5);
                let label = "ticket: ";
                let full_len = label.len() + ticket.len();
                let text_col = (ws.cols as usize).saturating_sub(full_len) / 2;
                let _ = term::move_to(&mut stdout, text_col as u16, row);
                let _ = write!(
                    stdout,
                    "\x1b[2K\x1b[2m{}\x1b[0m{}",
                    label, ticket,
                );
                let _ = stdout.flush();
                ticket_drawn = true;
            }
    }

    // Clean up all image IDs.
    for i in 0..(MAX_TILES * 2 + 2) {
        let _ = kitty::delete_image(&mut stdout, (i + 1) as u32);
    }
}
