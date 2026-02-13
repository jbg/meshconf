use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::kitty;
use crate::term;
use crate::tui;
use crate::video_compositor::{ConnectionKind, GalleryFrame, CELL_GAP_COLS, CELL_GAP_ROWS};

/// Status information displayed at the bottom of the terminal.
///
/// `message` is centered and rendered dim (e.g. "Waiting for connection…").
pub use crate::video_compositor::StatusInfo;

/// Flag set by the SIGWINCH handler to indicate the terminal was resized.
static RESIZED: AtomicBool = AtomicBool::new(false);

/// Install a SIGWINCH handler that sets the RESIZED flag.
fn install_sigwinch_handler() {
    unsafe {
        libc::signal(libc::SIGWINCH, sigwinch_handler as *const () as libc::sighandler_t);
    }
}

extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

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
    status_line: Arc<Mutex<Option<StatusInfo>>>,
    camera_error: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let handle = tokio::task::spawn_blocking(move || {
        display_loop(rx, cancel, status_line, camera_error);
    });

    handle.await?;
    Ok(())
}

/// Image IDs: we reserve a pair (back/front) per tile slot.
/// Slot i uses IDs (i*2 + 1) and (i*2 + 2).
const MAX_TILES: usize = 9;

fn clear_all_images(stdout: &mut impl Write) {
    let _ = write!(stdout, "\x1b[2J"); // clear all text
    let _ = stdout.flush();
    for i in 0..(MAX_TILES * 2 + 2) {
        let _ = kitty::delete_image(stdout, (i + 1) as u32);
    }
}

/// Per-tile label state used to avoid redrawing unchanged labels.
#[derive(Clone, PartialEq, Eq)]
struct DrawnLabel {
    name: Arc<str>,
    conn_kind_tag: u8, // cheap stand-in for ConnectionKind comparison
}

impl Default for DrawnLabel {
    fn default() -> Self {
        Self {
            name: Arc::from(""),
            conn_kind_tag: 0,
        }
    }
}

/// Per-tile state for tracking video drops and the red indicator dot.
#[derive(Clone)]
struct TileDropState {
    /// Last observed cumulative drop count for this tile.
    last_drop_count: u64,
    /// When drops were last detected (count increased).
    last_drop_time: Option<Instant>,
    /// Whether the red dot is currently drawn.
    dot_drawn: bool,
}

fn conn_kind_tag(kind: ConnectionKind) -> u8 {
    match kind {
        ConnectionKind::Unknown => 0,
        ConnectionKind::Direct  => 1,
        ConnectionKind::Relay   => 2,
        ConnectionKind::Local   => 3,
    }
}

fn display_loop(mut rx: mpsc::Receiver<GalleryFrame>, cancel: CancellationToken, status_line: Arc<Mutex<Option<StatusInfo>>>, camera_error: Arc<Mutex<Option<String>>>) {
    install_sigwinch_handler();

    let mut stdout = std::io::stdout();

    // Detect shared memory support (must happen while raw mode is active).
    let transfer_mode = if kitty::detect_shm_support() {
        tracing::info!("Kitty shm transfer supported — using shared memory");
        kitty::TransferMode::Shm
    } else {
        tracing::info!("Kitty shm transfer not supported — using PTY inline transfer");
        kitty::TransferMode::Pty
    };

    // Track which back-buffer ID to use per slot.
    let mut back_toggle: [bool; MAX_TILES] = [false; MAX_TILES];
    // Track last grid layout to clear stale images on layout change.
    let mut last_tile_count: usize = 0;
    let mut last_status_drawn: Option<StatusInfo> = None;
    let mut error_drawn = false;
    // Keep the last gallery frame so we can re-render on resize.
    let mut last_gallery: Option<GalleryFrame> = None;
    // Track drawn labels so we only redraw when they change.
    let mut drawn_labels: Vec<DrawnLabel> = Vec::new();
    // Track per-tile video drop state for the red dot indicator.
    let mut tile_drop_states: Vec<TileDropState> = Vec::new();
    // Reusable scratch buffers for Kitty image transmission.
    let mut transmit_scratch = kitty::TransmitScratch::new();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Check for terminal resize.
        let resized = RESIZED.swap(false, Ordering::SeqCst);
        if resized {
            clear_all_images(&mut stdout);
            last_status_drawn = None;
            error_drawn = false;
            drawn_labels.clear();
            tile_drop_states.clear();
            // Force re-render of the last frame at the new size.
            if let Some(ref gallery) = last_gallery {
                let status = status_line.lock().unwrap().clone();
                render_gallery(&mut stdout, gallery, &mut back_toggle, &status, &mut last_status_drawn, &mut drawn_labels, &mut tile_drop_states, &mut transmit_scratch, transfer_mode);
            }
        }

        // Draw camera error banner if present and not yet drawn.
        if !error_drawn {
            if let Some(ref err) = *camera_error.lock().unwrap() {
                let _ = tui::draw_error_banner(&mut stdout, err);
                error_drawn = true;
            }
        }

        let gallery = match rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        // If tile count changed, clear everything (text + images).
        if gallery.tiles.len() != last_tile_count {
            clear_all_images(&mut stdout);
            last_tile_count = gallery.tiles.len();
            last_status_drawn = None; // redraw status after layout change
            error_drawn = false; // redraw error after layout change
            drawn_labels.clear(); // force label redraw
            tile_drop_states.clear();
        }

        // Read the current status text (may change at any time from another task).
        let status = status_line.lock().unwrap().clone();

        // Redraw status if it changed since last time we drew it.
        let status_changed = status != last_status_drawn;

        // Nothing to draw — but re-draw the status if needed.
        if gallery.tiles.is_empty() {
            last_gallery = None;
            if status_changed {
                if let Ok(ws) = term::window_size() {
                    let anchor_row = ws.rows.saturating_sub(3);
                    draw_status_text(&mut stdout, &status, &last_status_drawn, ws.cols as usize, anchor_row);
                    let _ = stdout.flush();
                }
                last_status_drawn = status;
            }
            continue;
        }

        render_gallery(&mut stdout, &gallery, &mut back_toggle, &status, &mut last_status_drawn, &mut drawn_labels, &mut tile_drop_states, &mut transmit_scratch, transfer_mode);
        last_gallery = Some(gallery);
    }

    // Clean up all image IDs.
    for i in 0..(MAX_TILES * 2 + 2) {
        let _ = kitty::delete_image(&mut stdout, (i + 1) as u32);
    }
}

fn render_gallery(
    stdout: &mut impl std::io::Write,
    gallery: &GalleryFrame,
    back_toggle: &mut [bool; MAX_TILES],
    status: &Option<StatusInfo>,
    last_status_drawn: &mut Option<StatusInfo>,
    drawn_labels: &mut Vec<DrawnLabel>,
    tile_drop_states: &mut Vec<TileDropState>,
    transmit_scratch: &mut kitty::TransmitScratch,
    transfer_mode: kitty::TransferMode,
) {
    // Compute grid cell positions in terminal cells.
    let ws = match term::window_size() {
        Ok(ws) => ws,
        Err(_) => return,
    };

    if ws.x_pixel == 0 || ws.y_pixel == 0 || ws.cols == 0 || ws.rows == 0 {
        return;
    }

    let term_cell_w = ws.x_pixel as u32 / ws.cols as u32;
    let term_cell_h = ws.y_pixel as u32 / ws.rows as u32;
    if term_cell_w == 0 || term_cell_h == 0 {
        return;
    }

    // Use the maximum tile dimensions so every tile occupies the same
    // grid area. When ABR scales down a peer's resolution, the terminal
    // scales the image up via the Kitty `c=/r=` parameters.
    let tile_px_w = gallery.tiles.iter().map(|t| t.frame.width).max().unwrap_or(640);
    let tile_px_h = gallery.tiles.iter().map(|t| t.frame.height).max().unwrap_or(480);

    // Terminal cells occupied by one tile.
    let tile_cols = tile_px_w.div_ceil(term_cell_w);
    let tile_rows = tile_px_h.div_ceil(term_cell_h);

    // One extra row per grid row for the name label below each tile.
    let label_rows: u32 = 1;

    // Total gallery size in terminal cells.
    let gap_c = CELL_GAP_COLS as u32;
    let gap_r = CELL_GAP_ROWS as u32;
    let total_cols = gallery.cols * tile_cols + (gallery.cols - 1) * gap_c;
    let total_rows = gallery.rows * (tile_rows + label_rows) + (gallery.rows - 1) * gap_r;

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
            stdout,
            back_id,
            tile.frame.width,
            tile.frame.height,
            &tile.frame.data,
            transmit_scratch,
            transfer_mode,
        ) {
            tracing::warn!("Kitty transmit error for tile {}: {}", i, e);
            continue;
        }

        // Compute this tile's terminal cell position.
        let col = origin_col + tile.grid_col * (tile_cols + gap_c);
        let row = origin_row + tile.grid_row * (tile_rows + label_rows + gap_r);

        if let Err(e) =
            kitty::display_image_at(stdout, back_id, col as u16, row as u16, Some(tile_cols), Some(tile_rows))
        {
            tracing::warn!("Kitty display error for tile {}: {}", i, e);
        }

        // Draw name + connection kind label below the tile — only when it changes.
        let current_label = DrawnLabel {
            name: tile.name.clone(),
            conn_kind_tag: conn_kind_tag(tile.conn_kind),
        };
        let need_label_draw = drawn_labels.get(i) != Some(&current_label);
        if need_label_draw {
            // Ensure vec is large enough.
            if drawn_labels.len() <= i {
                drawn_labels.resize(i + 1, DrawnLabel::default());
            }
            drawn_labels[i] = current_label;

            let label_row = row + tile_rows;
            let display_name = if tile.name.is_empty() { "?" } else { &tile.name };
            let (kind_str, kind_visible_len) = match tile.conn_kind {
                ConnectionKind::Local   => ("\x1b[2m(you)\x1b[0m",    5),
                ConnectionKind::Direct  => ("\x1b[32m(direct)\x1b[0m", 8),
                ConnectionKind::Relay   => ("\x1b[33m(relay)\x1b[0m",  7),
                ConnectionKind::Unknown => ("\x1b[2m(…)\x1b[0m",      3),
            };
            // Visible (non-escape) character count for centering.
            let visible_len = display_name.len() + 1 + kind_visible_len;
            let label_col = col + (tile_cols.saturating_sub(visible_len as u32)) / 2;
            // Pad with spaces to overwrite any previous longer label
            // without using \x1b[2K which clears the whole line.
            let pad = (tile_cols as usize).saturating_sub(visible_len);
            let _ = term::move_to(stdout, label_col as u16, label_row as u16);
            let _ = write!(stdout, "\x1b[1m{}\x1b[0m {} {:pad$}", display_name, kind_str, "", pad = pad);
        }

        // --- Red drop indicator dot above the top-left corner of the tile ---
        // Ensure drop state vec is large enough.
        if tile_drop_states.len() <= i {
            tile_drop_states.resize(i + 1, TileDropState {
                last_drop_count: 0,
                last_drop_time: None,
                dot_drawn: false,
            });
        }
        let ds = &mut tile_drop_states[i];
        let drops = tile.video_drops;
        if drops > ds.last_drop_count {
            ds.last_drop_time = Some(Instant::now());
            ds.last_drop_count = drops;
        }
        let should_show_dot = ds.last_drop_time
            .map(|t| t.elapsed().as_secs_f64() < 1.0)
            .unwrap_or(false);
        if should_show_dot != ds.dot_drawn {
            // Draw or clear the dot one row above the tile's top-left.
            if row > 0 {
                let dot_row = row - 1;
                let _ = term::move_to(stdout, col as u16, dot_row as u16);
                if should_show_dot {
                    let _ = write!(stdout, "\x1b[31m●\x1b[0m");
                } else {
                    let _ = write!(stdout, " ");
                }
            }
            ds.dot_drawn = should_show_dot;
        }
    }
    let _ = stdout.flush();

    // Draw status line near the bottom of the terminal (only when changed).
    if *status != *last_status_drawn {
        let term_cols = ws.cols as usize;
        let anchor_row = ws.rows.saturating_sub(3);
        draw_status_text(stdout, status, last_status_drawn, term_cols, anchor_row);
        let _ = stdout.flush();
        *last_status_drawn = status.clone();
    }
}

/// Build a styled label string for a tile: "name (kind)".
///
/// Uses ANSI escape codes for colour:
/// - name in bold white
/// - "(you)" in dim
/// - "(direct)" in green
/// Draw (or clear) the status near the bottom of the terminal.
///
/// The message line is centered and dimmed.  The optional copyable text
/// (e.g. a ticket) is printed starting at column 0 so that terminal text
/// selection grabs only the ticket with no embedded whitespace.
///
/// Layout rendered upward from `anchor_row`:
///   [copyable lines … at col 0]   ← bottom, wraps naturally
///   [blank line]
///   [centered message]             ← anchor_row - copyable_lines - 1
fn draw_status_text(
    stdout: &mut impl Write,
    new_status: &Option<StatusInfo>,
    old_status: &Option<StatusInfo>,
    term_cols: usize,
    anchor_row: u16,
) {
    // Clear all rows the old status occupied.
    let old_row_count = status_row_count(old_status, term_cols);
    let old_top = anchor_row.saturating_sub(old_row_count.saturating_sub(1) as u16);
    for i in 0..old_row_count {
        let _ = term::move_to(stdout, 0, old_top + i as u16);
        let _ = write!(stdout, "\x1b[2K");
    }

    if let Some(info) = new_status {
        let copyable_lines = info.copyable.as_ref()
            .map(|t| wrapped_line_count(t, term_cols))
            .unwrap_or(0);
        // Total rows: message + blank + copyable (or just message if no copyable).
        let total = if copyable_lines > 0 { 1 + 1 + copyable_lines } else { 1 };
        let top_row = anchor_row.saturating_sub(total.saturating_sub(1) as u16);

        // Centered message line.
        let msg_len = info.message.chars().count();
        let col = term_cols.saturating_sub(msg_len) / 2;
        let _ = term::move_to(stdout, col as u16, top_row);
        let _ = write!(stdout, "\x1b[2K\x1b[2m{}\x1b[0m", info.message);

        // Copyable text at column 0 (blank line separates it from the message).
        if let Some(ref ticket) = info.copyable {
            let copy_start = top_row + 2; // +1 for message row, +1 for blank
            let _ = term::move_to(stdout, 0, copy_start);
            let _ = write!(stdout, "{}", ticket);
        }
    }
}

/// How many terminal rows a status occupies.
fn status_row_count(status: &Option<StatusInfo>, term_cols: usize) -> usize {
    match status {
        None => 0,
        Some(info) => {
            let copyable_lines = info.copyable.as_ref()
                .map(|t| wrapped_line_count(t, term_cols))
                .unwrap_or(0);
            if copyable_lines > 0 { 1 + 1 + copyable_lines } else { 1 }
        }
    }
}

/// Count how many rows `text` would occupy when wrapped to `width` columns.
fn wrapped_line_count(text: &str, width: usize) -> usize {
    if width == 0 || text.is_empty() {
        return 1;
    }
    let char_count = text.chars().count();
    (char_count + width - 1) / width
}


