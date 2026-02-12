use std::io::{BufWriter, Write};
use std::time::Duration;

use crate::term;

/// Detect whether the terminal supports the Kitty graphics protocol.
///
/// Sends a 1×1 RGB image query and checks for an OK response.
/// Manages its own raw mode session — call before any other raw mode usage.
pub fn detect_kitty_support() -> anyhow::Result<bool> {
    term::enable_raw_mode()?;

    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")?;
    stdout.flush()?;

    let supported = read_kitty_response(Duration::from_millis(500))
        .map(|r| r.contains("OK"))
        .unwrap_or(false);

    term::disable_raw_mode()?;
    Ok(supported)
}

/// Read a Kitty graphics response from stdin.
/// Response format: \x1b_G...;\x1b\
fn read_kitty_response(timeout: Duration) -> Option<String> {
    let mut collected = Vec::new();
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        if !term::poll_stdin(remaining) {
            break;
        }

        let mut buf = [0u8; 256];
        if let Ok(n) = term::read_stdin(&mut buf) {
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..n]);
            if collected.len() >= 2
                && collected[collected.len() - 2] == b'\x1b'
                && collected[collected.len() - 1] == b'\\'
            {
                break;
            }
        }
    }

    if collected.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&collected).into_owned())
    }
}

/// Transmit an RGB24 image as PNG via inline base64 without displaying it (`a=t`).
/// Call `display_image` afterwards to show it.
pub fn transmit_image(
    stdout: &mut impl Write,
    id: u32,
    width: u32,
    height: u32,
    rgb_data: &[u8],
) -> anyhow::Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::ImageEncoder;

    let mut png_buf = Vec::with_capacity(rgb_data.len() / 2);
    let encoder =
        PngEncoder::new_with_quality(&mut png_buf, CompressionType::Fast, FilterType::Sub);
    encoder.write_image(rgb_data, width, height, image::ExtendedColorType::Rgb8)?;

    let b64 = STANDARD.encode(&png_buf);
    let mut buf = BufWriter::new(stdout);

    let chunk_size = 4096;
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == chunks.len() - 1;
        let more = if is_last { 0 } else { 1 };

        if is_first {
            // a=t: transmit only, don't display. q=2: suppress responses.
            write!(
                buf,
                "\x1b_Gi={},s={},v={},a=t,f=100,t=d,q=2,m={};{}\x1b\\",
                id, width, height, more, chunk,
            )?;
        } else if is_last {
            write!(buf, "\x1b_Gm=0;{}\x1b\\", chunk)?;
        } else {
            write!(buf, "\x1b_Gm=1;{}\x1b\\", chunk)?;
        }
    }

    buf.flush()?;
    Ok(())
}

/// Display a previously transmitted image centered in the terminal (`a=p`).
/// Clears all existing placements of this image first to avoid duplicates on resize.
pub fn display_image(stdout: &mut impl Write, id: u32) -> anyhow::Result<()> {
    let (col, row) = centered_position(crate::video_capture::ENCODE_WIDTH, crate::video_capture::ENCODE_HEIGHT);
    let mut buf = BufWriter::new(stdout);
    // Delete all placements (but keep the image data) before re-placing
    write!(buf, "\x1b_Ga=d,d=i,i={},q=2\x1b\\", id)?;
    term::move_to(&mut buf, col, row)?;
    write!(buf, "\x1b_Ga=p,i={},q=2\x1b\\", id)?;
    buf.flush()?;
    Ok(())
}

/// Display a previously transmitted image centered in the terminal, using the
/// actual image dimensions for centering.
pub fn display_image_sized(stdout: &mut impl Write, id: u32, img_w: u32, img_h: u32) -> anyhow::Result<()> {
    let (col, row) = centered_position(img_w, img_h);
    let mut buf = BufWriter::new(stdout);
    write!(buf, "\x1b_Ga=d,d=i,i={},q=2\x1b\\", id)?;
    term::move_to(&mut buf, col, row)?;
    write!(buf, "\x1b_Ga=p,i={},q=2\x1b\\", id)?;
    buf.flush()?;
    Ok(())
}

/// Display a previously transmitted image at a specific cell position (`a=p`).
pub fn display_image_at(stdout: &mut impl Write, id: u32, col: u16, row: u16) -> anyhow::Result<()> {
    let mut buf = BufWriter::new(stdout);
    term::move_to(&mut buf, col, row)?;
    write!(buf, "\x1b_Ga=p,i={},q=2\x1b\\", id)?;
    buf.flush()?;
    Ok(())
}

/// Compute the top-left cell position to center an image of the given pixel size.
fn centered_position(img_w: u32, img_h: u32) -> (u16, u16) {
    let ws = match term::window_size() {
        Ok(ws) => ws,
        Err(_) => return (0, 0),
    };

    if ws.x_pixel == 0 || ws.y_pixel == 0 || ws.cols == 0 || ws.rows == 0 {
        return (0, 0);
    }

    let cell_w = ws.x_pixel as u32 / ws.cols as u32;
    let cell_h = ws.y_pixel as u32 / ws.rows as u32;

    if cell_w == 0 || cell_h == 0 {
        return (0, 0);
    }

    let img_cols = img_w.div_ceil(cell_w);
    let img_rows = img_h.div_ceil(cell_h);

    let col = (ws.cols as u32).saturating_sub(img_cols) / 2;
    let row = (ws.rows as u32).saturating_sub(img_rows) / 2;

    (col as u16, row as u16)
}

/// Delete an image by ID from the terminal.
pub fn delete_image(stdout: &mut impl Write, id: u32) -> anyhow::Result<()> {
    write!(stdout, "\x1b_Ga=d,d=I,i={}\x1b\\", id)?;
    stdout.flush()?;
    Ok(())
}
