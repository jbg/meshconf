use std::io::Write;
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

/// Detect whether the terminal supports shared memory image transfer (`t=shm`).
///
/// Must be called while raw mode is **active** (the caller is responsible for
/// raw mode lifecycle).
pub fn detect_shm_support() -> bool {
    // Create a tiny 1-byte shm, send a query, check for OK.
    let name = b"/vc_shm_probe\0";
    let name_cstr = name.as_ptr() as *const libc::c_char;

    let ok = unsafe {
        let fd = libc::shm_open(name_cstr, libc::O_CREAT | libc::O_RDWR, 0o600);
        if fd < 0 {
            return false;
        }
        if libc::ftruncate(fd, 3) != 0 {
            libc::close(fd);
            libc::shm_unlink(name_cstr);
            return false;
        }
        // Write 1 pixel of RGB data.
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            3,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            libc::close(fd);
            libc::shm_unlink(name_cstr);
            return false;
        }
        std::ptr::write_bytes(ptr as *mut u8, 0, 3);
        libc::munmap(ptr, 3);
        libc::close(fd);
        true
    };

    if !ok {
        return false;
    }

    let mut stdout = std::io::stdout();
    // a=q: query. t=shm. f=24. 1×1 pixel.
    let _ = write!(
        stdout,
        "\x1b_Gi=32,s=1,v=1,a=q,f=24,t=shm;/vc_shm_probe\x1b\\"
    );
    let _ = stdout.flush();

    let supported = read_kitty_response(Duration::from_millis(500))
        .map(|r| r.contains("OK"))
        .unwrap_or(false);

    // Clean up in case terminal didn't unlink.
    unsafe {
        libc::shm_unlink(name_cstr);
    }

    supported
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

// ---------------------------------------------------------------------------
// Image transmission
// ---------------------------------------------------------------------------

/// Reusable state for [`transmit_image`] and [`transmit_image_shm`].
///
/// Create once in the display loop and pass to every transmit call.
pub struct TransmitScratch {
    /// Reusable buffer for u32→RGB24 conversion.
    pub rgb_data: Vec<u8>,
    /// Monotonic counter for unique shm names (shm path only).
    shm_counter: u64,
    /// Reusable buffer for building escape sequences / output data.
    cmd_buf: Vec<u8>,
    /// Reusable buffer for zlib-compressed pixel data (PTY path only).
    zlib_buf: Vec<u8>,
}

impl TransmitScratch {
    pub fn new() -> Self {
        Self {
            rgb_data: Vec::new(),
            shm_counter: 0,
            cmd_buf: Vec::new(),
            zlib_buf: Vec::new(),
        }
    }
}

/// Convert packed u32 pixel data to RGB24 into `scratch.rgb_data`.
fn convert_to_rgb24(pixel_data: &[u32], pixel_count: usize, scratch: &mut TransmitScratch) {
    scratch.rgb_data.clear();
    scratch.rgb_data.reserve(pixel_count * 3);
    for &px in &pixel_data[..pixel_count] {
        scratch.rgb_data.push(((px >> 16) & 0xFF) as u8); // R
        scratch.rgb_data.push(((px >> 8) & 0xFF) as u8);  // G
        scratch.rgb_data.push((px & 0xFF) as u8);          // B
    }
}

// ---------------------------------------------------------------------------
// Shared memory transport (t=shm) — preferred when supported
// ---------------------------------------------------------------------------

/// Transmit an image via POSIX shared memory (`t=shm`).
///
/// Creates a shm object, copies RGB24 pixels into it, and sends ~80 bytes
/// through the PTY. The terminal reads pixels directly from shared memory
/// and unlinks the shm name.
pub fn transmit_image_shm(
    stdout: &mut impl Write,
    id: u32,
    width: u32,
    height: u32,
    pixel_data: &[u32],
    scratch: &mut TransmitScratch,
) -> anyhow::Result<()> {
    let pixel_count = (width * height) as usize;
    let rgb_len = pixel_count * 3;

    convert_to_rgb24(pixel_data, pixel_count, scratch);

    // Generate a unique shm name into a stack buffer.
    // Format: /vc_<pid>_<counter>  (leading '/' required by POSIX)
    scratch.shm_counter += 1;
    let mut name_buf = [0u8; 48];
    let name_len = {
        let mut pos = 0;
        let mut push = |bytes: &[u8]| {
            name_buf[pos..pos + bytes.len()].copy_from_slice(bytes);
            pos += bytes.len();
        };
        let mut itoa_tmp = itoa::Buffer::new();
        push(b"/vc_");
        push(itoa_tmp.format(std::process::id()).as_bytes());
        push(b"_");
        push(itoa_tmp.format(scratch.shm_counter).as_bytes());
        name_buf[pos] = 0; // NUL terminator for C API
        pos
    };
    let name_cstr = name_buf.as_ptr() as *const libc::c_char;
    // The escape sequence payload is the shm name.
    // Kitty's docs say "without leading /" but Ghostty expects the full name.
    // POSIX shm_open requires the leading '/', so we pass the full name and
    // let the terminal handle it — both Kitty and Ghostty open it with shm_open
    // which will prepend '/' if needed.
    let shm_payload = &name_buf[..name_len];

    unsafe {
        let fd = libc::shm_open(name_cstr, libc::O_CREAT | libc::O_RDWR, 0o600);
        if fd < 0 {
            return Err(anyhow::anyhow!(
                "shm_open failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        if libc::ftruncate(fd, rgb_len as libc::off_t) != 0 {
            libc::close(fd);
            libc::shm_unlink(name_cstr);
            return Err(anyhow::anyhow!(
                "ftruncate failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let ptr = libc::mmap(
            std::ptr::null_mut(),
            rgb_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            libc::close(fd);
            libc::shm_unlink(name_cstr);
            return Err(anyhow::anyhow!(
                "mmap failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        std::ptr::copy_nonoverlapping(scratch.rgb_data.as_ptr(), ptr as *mut u8, rgb_len);

        libc::munmap(ptr, rgb_len);
        libc::close(fd);
    }

    // Build escape sequence: ~80 bytes.
    let mut itoa_buf = itoa::Buffer::new();
    scratch.cmd_buf.clear();
    scratch.cmd_buf.extend_from_slice(b"\x1b_Gi=");
    scratch.cmd_buf.extend_from_slice(itoa_buf.format(id).as_bytes());
    scratch.cmd_buf.extend_from_slice(b",s=");
    scratch.cmd_buf.extend_from_slice(itoa_buf.format(width).as_bytes());
    scratch.cmd_buf.extend_from_slice(b",v=");
    scratch.cmd_buf.extend_from_slice(itoa_buf.format(height).as_bytes());
    scratch.cmd_buf.extend_from_slice(b",a=t,f=24,t=shm,q=2;");
    scratch.cmd_buf.extend_from_slice(shm_payload);
    scratch.cmd_buf.extend_from_slice(b"\x1b\\");

    stdout.write_all(&scratch.cmd_buf)?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Direct PTY transport (t=d) — fallback for terminals without shm support
// ---------------------------------------------------------------------------

/// Transmit an image as zlib-compressed RGB24 via inline base64 over the PTY.
///
/// Uses `o=z` to tell the terminal the payload is zlib-compressed, and `f=24`
/// for raw RGB. Compression level 1 (fastest) typically reduces video frames
/// to 30-50% of their raw size with minimal CPU cost.
///
/// Builds the entire Kitty escape sequence stream into a single buffer and
/// writes it with one `write_all` call.
pub fn transmit_image_pty(
    stdout: &mut impl Write,
    id: u32,
    width: u32,
    height: u32,
    pixel_data: &[u32],
    scratch: &mut TransmitScratch,
) -> anyhow::Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use flate2::{Compression, write::ZlibEncoder};

    let pixel_count = (width * height) as usize;
    convert_to_rgb24(pixel_data, pixel_count, scratch);

    // Zlib-compress the RGB data at level 1 (fastest).
    scratch.zlib_buf.clear();
    {
        let mut encoder = ZlibEncoder::new(&mut scratch.zlib_buf, Compression::fast());
        encoder.write_all(&scratch.rgb_data)?;
        encoder.finish()?;
    }

    // Base64 encode the compressed data into cmd_buf.
    let b64_len = scratch.zlib_buf.len() * 4 / 3 + 4;
    scratch.cmd_buf.clear();
    scratch.cmd_buf.resize(b64_len, 0);
    let b64_len = STANDARD
        .encode_slice(&scratch.zlib_buf, &mut scratch.cmd_buf)
        .map_err(|e| anyhow::anyhow!("base64 encode failed: {e}"))?;

    // We need the b64 data and the output buffer simultaneously, so copy
    // the b64 data into rgb_data (which we're done with) to free cmd_buf.
    scratch.rgb_data.clear();
    scratch.rgb_data.extend_from_slice(&scratch.cmd_buf[..b64_len]);
    let b64 = &scratch.rgb_data[..b64_len];

    // Build escape sequence stream into cmd_buf.
    let chunk_size = 4096;
    let total_chunks = (b64_len + chunk_size - 1) / chunk_size;
    scratch.cmd_buf.clear();
    scratch.cmd_buf.reserve(b64_len + total_chunks * 32);

    let mut itoa_buf = itoa::Buffer::new();

    for (i, chunk) in b64.chunks(chunk_size).enumerate() {
        let is_first = i == 0;
        let is_last = i == total_chunks - 1;
        let more = if is_last { b"0" as &[u8] } else { b"1" };

        scratch.cmd_buf.extend_from_slice(b"\x1b_G");
        if is_first {
            scratch.cmd_buf.extend_from_slice(b"i=");
            scratch.cmd_buf.extend_from_slice(itoa_buf.format(id).as_bytes());
            scratch.cmd_buf.extend_from_slice(b",s=");
            scratch.cmd_buf.extend_from_slice(itoa_buf.format(width).as_bytes());
            scratch.cmd_buf.extend_from_slice(b",v=");
            scratch.cmd_buf.extend_from_slice(itoa_buf.format(height).as_bytes());
            scratch.cmd_buf.extend_from_slice(b",a=t,f=24,o=z,t=d,q=2,m=");
        } else {
            scratch.cmd_buf.extend_from_slice(b"m=");
        }
        scratch.cmd_buf.extend_from_slice(more);
        scratch.cmd_buf.push(b';');
        scratch.cmd_buf.extend_from_slice(chunk);
        scratch.cmd_buf.extend_from_slice(b"\x1b\\");
    }

    stdout.write_all(&scratch.cmd_buf)?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unified entry point (chosen at init time by the display loop)
// ---------------------------------------------------------------------------

/// Transfer mode, detected once at startup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    /// POSIX shared memory — minimal PTY traffic.
    Shm,
    /// Inline base64 over the PTY — universal fallback.
    Pty,
}

/// Transmit an image using the given transfer mode.
pub fn transmit_image(
    stdout: &mut impl Write,
    id: u32,
    width: u32,
    height: u32,
    pixel_data: &[u32],
    scratch: &mut TransmitScratch,
    mode: TransferMode,
) -> anyhow::Result<()> {
    match mode {
        TransferMode::Shm => transmit_image_shm(stdout, id, width, height, pixel_data, scratch),
        TransferMode::Pty => transmit_image_pty(stdout, id, width, height, pixel_data, scratch),
    }
}

// ---------------------------------------------------------------------------
// Display / delete helpers (unchanged — these are tiny escape sequences)
// ---------------------------------------------------------------------------

/// Display a previously transmitted image centered in the terminal (`a=p`).
pub fn display_image(stdout: &mut impl Write, id: u32) -> anyhow::Result<()> {
    let (col, row) = centered_position(crate::video_capture::ENCODE_WIDTH, crate::video_capture::ENCODE_HEIGHT);
    let mut buf = std::io::BufWriter::new(stdout);
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
    let mut buf = std::io::BufWriter::new(stdout);
    write!(buf, "\x1b_Ga=d,d=i,i={},q=2\x1b\\", id)?;
    term::move_to(&mut buf, col, row)?;
    write!(buf, "\x1b_Ga=p,i={},q=2\x1b\\", id)?;
    buf.flush()?;
    Ok(())
}

/// Display a previously transmitted image at a specific cell position (`a=p`).
pub fn display_image_at(
    stdout: &mut impl Write,
    id: u32,
    col: u16,
    row: u16,
    fit_cols: Option<u32>,
    fit_rows: Option<u32>,
) -> anyhow::Result<()> {
    let mut buf = std::io::BufWriter::new(stdout);
    term::move_to(&mut buf, col, row)?;
    match (fit_cols, fit_rows) {
        (Some(c), Some(r)) => {
            write!(buf, "\x1b_Ga=p,i={},c={},r={},q=2\x1b\\", id, c, r)?;
        }
        _ => {
            write!(buf, "\x1b_Ga=p,i={},q=2\x1b\\", id)?;
        }
    }
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
