//! Minimal terminal utilities — replaces crossterm for our needs.
//!
//! Raw mode, cursor movement, reading raw input bytes, and terminal size queries.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

/// Original termios state, restored on drop or via `disable_raw_mode`.
static ORIG_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

/// Enable raw mode on stdin. Saves the original termios for later restoration.
pub fn enable_raw_mode() -> io::Result<()> {
    let fd = io::stdin().as_raw_fd();
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };

    if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
        return Err(io::Error::last_os_error());
    }

    *ORIG_TERMIOS.lock().unwrap() = Some(orig);

    let mut raw = orig;
    // Input: no break, no CR-to-NL, no parity, no strip, no flow control
    raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
    // Output: disable post-processing
    raw.c_oflag &= !libc::OPOST;
    // Control: 8-bit chars
    raw.c_cflag |= libc::CS8;
    // Local: no echo, no canonical, no signals, no extended
    raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
    // Return immediately from read with whatever is available
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;

    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // Hide cursor
    print!("\x1b[?25l");
    io::stdout().flush()?;

    Ok(())
}

/// Restore the original terminal mode.
pub fn disable_raw_mode() -> io::Result<()> {
    if let Some(orig) = ORIG_TERMIOS.lock().unwrap().take() {
        let fd = io::stdin().as_raw_fd();
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &orig) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // Show cursor
    print!("\x1b[?25h");
    io::stdout().flush()?;
    Ok(())
}

/// RAII guard that restores terminal state on drop.
pub struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Move cursor to (col, row) — 0-indexed. Emits CSI sequence directly.
pub fn move_to(out: &mut impl Write, col: u16, row: u16) -> io::Result<()> {
    // CSI row;col H — 1-indexed
    write!(out, "\x1b[{};{}H", row + 1, col + 1)
}

/// Terminal size in cells and pixels.
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub x_pixel: u16,
    pub y_pixel: u16,
}

/// Query the terminal size via TIOCGWINSZ.
pub fn window_size() -> io::Result<WinSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = io::stdout().as_raw_fd();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(WinSize {
        cols: ws.ws_col,
        rows: ws.ws_row,
        x_pixel: ws.ws_xpixel,
        y_pixel: ws.ws_ypixel,
    })
}

/// Wait for stdin to become readable, up to `timeout`.
/// Returns `true` if data is available.
pub fn poll_stdin(timeout: Duration) -> bool {
    let fd = io::stdin().as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
    ret > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// Read whatever bytes are currently available from stdin (non-blocking).
/// Returns the number of bytes read.
pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    io::stdin().read(buf)
}

/// Drain any pending bytes from stdin.
pub fn drain_stdin() {
    let mut buf = [0u8; 1024];
    while poll_stdin(Duration::from_millis(10)) {
        if io::stdin().read(&mut buf).unwrap_or(0) == 0 {
            break;
        }
    }
}

/// Read a single key press. Returns the raw byte(s).
/// Blocks up to `timeout`. Returns `None` on timeout.
pub fn read_key(timeout: Duration) -> Option<KeyPress> {
    if !poll_stdin(timeout) {
        return None;
    }
    let mut buf = [0u8; 8];
    let n = read_stdin(&mut buf).unwrap_or(0);
    if n == 0 {
        return None;
    }

    match buf[0] {
        3 => Some(KeyPress::CtrlC),
        b'q' | b'Q' => Some(KeyPress::Char(buf[0] as char)),
        0x1b => {
            // Escape or escape sequence — don't interpret further
            Some(KeyPress::Escape)
        }
        _ => Some(KeyPress::Char(buf[0] as char)),
    }
}

#[derive(Debug)]
pub enum KeyPress {
    Char(char),
    CtrlC,
    Escape,
}
