//! Simple TUI drawing helpers using ANSI escape sequences.

use std::io::{self, BufWriter, Write};
use std::sync::{Arc, Mutex};

use crate::term;

/// Clear the entire screen and hide the cursor.
pub fn init(out: &mut impl Write) -> io::Result<()> {
    // Switch to alternate screen buffer, clear, hide cursor
    write!(out, "\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l")?;
    out.flush()
}

/// Show cursor and restore main screen buffer on exit.
pub fn cleanup(out: &mut impl Write) -> io::Result<()> {
    write!(out, "\x1b[?25h\x1b[?1049l")?;
    out.flush()
}

/// Word-wrap a long string into lines of at most `width` characters.
fn wrap_line(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut remaining = s;
    while remaining.len() > width {
        lines.push(remaining[..width].to_string());
        remaining = &remaining[width..];
    }
    lines.push(remaining.to_string());
    lines
}

/// Draw a box with a title centered on screen, containing lines of text.
/// Long lines are wrapped to fit within the terminal width.
pub fn draw_centered_box(out: &mut impl Write, title: &str, lines: &[&str]) -> io::Result<()> {
    let ws = term::window_size()?;
    let term_w = ws.cols as usize;
    let term_h = ws.rows as usize;

    // Max content width: leave room for borders + some margin
    let max_content_w = term_w.saturating_sub(8); // 4 for box chrome + 4 margin

    // Wrap lines that are too long
    let mut wrapped: Vec<String> = Vec::new();
    for line in lines {
        if line.len() > max_content_w {
            wrapped.extend(wrap_line(line, max_content_w));
        } else {
            wrapped.push(line.to_string());
        }
    }

    let max_line_len = wrapped.iter().map(|l| l.len()).max().unwrap_or(0);
    let title_len = title.len();
    let content_w = max_line_len.max(title_len).max(20).min(max_content_w);
    let box_w = content_w + 4; // 2 border + 2 padding
    let box_h = wrapped.len() + 4; // top border + title + separator + lines + bottom border

    let start_col = term_w.saturating_sub(box_w) / 2;
    let start_row = term_h.saturating_sub(box_h) / 2;

    let mut buf = BufWriter::new(out);

    // Top border
    term::move_to(&mut buf, start_col as u16, start_row as u16)?;
    write!(buf, "╭{}╮", "─".repeat(box_w - 2))?;

    // Title line
    let title_pad = (box_w - 2 - title_len) / 2;
    term::move_to(&mut buf, start_col as u16, (start_row + 1) as u16)?;
    write!(
        buf,
        "│{}{}{:>pad$}│",
        " ".repeat(title_pad),
        title,
        "",
        pad = box_w - 2 - title_pad - title_len,
    )?;

    // Separator
    term::move_to(&mut buf, start_col as u16, (start_row + 2) as u16)?;
    write!(buf, "├{}┤", "─".repeat(box_w - 2))?;

    // Content lines
    for (i, line) in wrapped.iter().enumerate() {
        term::move_to(&mut buf, start_col as u16, (start_row + 3 + i) as u16)?;
        write!(buf, "│ {:<width$} │", line, width = content_w)?;
    }

    // Bottom border
    term::move_to(
        &mut buf,
        start_col as u16,
        (start_row + 3 + wrapped.len()) as u16,
    )?;
    write!(buf, "╰{}╯", "─".repeat(box_w - 2))?;

    buf.flush()
}

/// Draw a centered box with a copyable text block below it.
/// The copyable text is printed starting at column 0 with no padding,
/// so terminal text selection copies it cleanly.
pub fn draw_centered_box_with_copyable(
    out: &mut impl Write,
    title: &str,
    lines: &[&str],
    copyable: &str,
) -> io::Result<()> {
    let ws = term::window_size()?;
    let term_w = ws.cols as usize;
    let term_h = ws.rows as usize;

    let max_content_w = term_w.saturating_sub(8);

    let max_line_len = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    let title_len = title.len();
    let content_w = max_line_len.max(title_len).max(20).min(max_content_w);
    let box_w = content_w + 4;
    // box rows: top + title + separator + content lines + bottom
    // then 1 blank + copyable text below
    let box_content_h = lines.len();
    let box_h = box_content_h + 4;
    let total_h = box_h + 2; // box + blank line + copyable line

    let start_col = term_w.saturating_sub(box_w) / 2;
    let start_row = term_h.saturating_sub(total_h) / 2;

    let mut buf = BufWriter::new(out);

    // Top border
    term::move_to(&mut buf, start_col as u16, start_row as u16)?;
    write!(buf, "╭{}╮", "─".repeat(box_w - 2))?;

    // Title line
    let title_pad = (box_w - 2 - title_len) / 2;
    term::move_to(&mut buf, start_col as u16, (start_row + 1) as u16)?;
    write!(
        buf,
        "│{}{}{:>pad$}│",
        " ".repeat(title_pad),
        title,
        "",
        pad = box_w - 2 - title_pad - title_len,
    )?;

    // Separator
    term::move_to(&mut buf, start_col as u16, (start_row + 2) as u16)?;
    write!(buf, "├{}┤", "─".repeat(box_w - 2))?;

    // Content lines
    for (i, line) in lines.iter().enumerate() {
        term::move_to(&mut buf, start_col as u16, (start_row + 3 + i) as u16)?;
        write!(buf, "│ {:<width$} │", line, width = content_w)?;
    }

    // Bottom border
    let bottom_row = start_row + 3 + box_content_h;
    term::move_to(&mut buf, start_col as u16, bottom_row as u16)?;
    write!(buf, "╰{}╯", "─".repeat(box_w - 2))?;

    // Copyable text: starts at column 0, no padding, no box chrome.
    // This ensures terminal text selection grabs only the ticket.
    let copy_row = bottom_row + 2;
    term::move_to(&mut buf, 0, copy_row as u16)?;
    write!(buf, "{}", copyable)?;

    buf.flush()
}

/// Draw a status line at the bottom of the terminal.
pub fn draw_status(out: &mut impl Write, text: &str) -> io::Result<()> {
    let ws = term::window_size()?;
    let mut buf = BufWriter::new(out);
    term::move_to(&mut buf, 0, ws.rows.saturating_sub(1))?;
    write!(buf, "\x1b[2K\x1b[2m{}\x1b[0m", text)?;
    buf.flush()
}

/// Draw an error banner at the top of the terminal, wrapping across multiple lines.
pub fn draw_error_banner(out: &mut impl Write, message: &str) -> io::Result<()> {
    let ws = term::window_size()?;
    let term_w = ws.cols as usize;

    // Max usable width (leave some padding)
    let max_w = term_w.saturating_sub(4);
    let prefix = "⚠ ";
    let prefix_len = 4; // "⚠ " is 4 columns (⚠ = 1 wide char + space, but using UnicodeWidthStr is overkill; 2 bytes display + space ≈ 3-4 cols)

    // Word-wrap the message to fit within max_w (accounting for prefix on first line)
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let first_line_max = max_w.saturating_sub(prefix_len);

    for word in message.split_whitespace() {
        let line_max = if lines.is_empty() { first_line_max } else { max_w };
        if current_line.is_empty() {
            if word.len() > line_max {
                // Long word: hard-break it
                let mut remaining = word;
                while !remaining.is_empty() {
                    let lmax = if lines.is_empty() && current_line.is_empty() { first_line_max } else { max_w };
                    let take: String = remaining.chars().take(lmax).collect();
                    let taken = take.len();
                    lines.push(take);
                    remaining = &remaining[taken..];
                }
            } else {
                current_line.push_str(word);
            }
        } else if current_line.len() + 1 + word.len() <= line_max {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current_line));
            current_line.push_str(word);
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    let mut buf = BufWriter::new(out);
    for (i, line) in lines.iter().enumerate() {
        let full_line = if i == 0 {
            format!("{}{}", prefix, line)
        } else {
            format!("{:>width$}{}", "", line, width = prefix_len)
        };
        let col = term_w.saturating_sub(full_line.len()) / 2;
        term::move_to(&mut buf, col as u16, (1 + i) as u16)?;
        write!(buf, "\x1b[1;31m{}\x1b[0m", full_line)?;
    }
    buf.flush()
}

/// Clear the screen (content only, no cursor state change).
pub fn clear(out: &mut impl Write) -> io::Result<()> {
    write!(out, "\x1b[2J\x1b[H")?;
    out.flush()
}

// --- Log pane: captures tracing output and displays it in the TUI ---

/// Shared ring buffer of log lines for display in the TUI.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<LogBufferInner>>,
}

struct LogBufferInner {
    lines: Vec<String>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LogBufferInner {
                lines: Vec::with_capacity(capacity),
                capacity,
            })),
        }
    }

    pub fn push(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.lines.len() >= inner.capacity {
            inner.lines.remove(0);
        }
        inner.lines.push(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.inner.lock().unwrap().lines.clone()
    }
}

/// A tracing layer that writes formatted log lines into a `LogBuffer`.
pub struct TuiLogLayer {
    buf: LogBuffer,
}

impl TuiLogLayer {
    pub fn new(buf: LogBuffer) -> Self {
        Self { buf }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TuiLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write as FmtWrite;
        let mut msg = String::new();
        let meta = event.metadata();
        let _ = write!(msg, "{} ", meta.level());
        // Extract the message field
        struct Visitor<'a>(&'a mut String);
        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                if field.name() == "message" {
                    let _ = write!(self.0, "{:?}", value);
                } else {
                    let _ = write!(self.0, " {}={:?}", field.name(), value);
                }
            }
        }
        event.record(&mut Visitor(&mut msg));
        self.buf.push(msg);
    }
}

/// Draw the log pane at the bottom of the terminal (above the status line).
pub fn draw_log_pane(out: &mut impl Write, log_buf: &LogBuffer, max_lines: usize) -> io::Result<()> {
    let ws = term::window_size()?;
    let term_w = ws.cols as usize;
    let pane_start_row = (ws.rows as usize).saturating_sub(max_lines + 1); // +1 for status

    let lines = log_buf.lines();
    let display_lines = if lines.len() > max_lines {
        &lines[lines.len() - max_lines..]
    } else {
        &lines
    };

    let mut buf = BufWriter::new(out);
    for (i, line) in display_lines.iter().enumerate() {
        let row = pane_start_row + i;
        term::move_to(&mut buf, 0, row as u16)?;
        write!(buf, "\x1b[2K")?; // clear line
        // Truncate to terminal width, dim
        let truncated: String = line.chars().take(term_w).collect();
        write!(buf, "\x1b[2m{}\x1b[0m", truncated)?;
    }
    buf.flush()
}
