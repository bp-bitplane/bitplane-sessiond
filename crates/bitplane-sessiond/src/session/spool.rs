//! A session's terminal state (ADR 0025), behind a trait in the shape of
//! shpool's `SessionSpool`.

use bitplane_protocol::session::Size;
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::{Terminal, TerminalOptions};

/// Scrollback budget per session. ghostty's `max_scrollback` is bytes, not
/// lines: about 1 MiB holds 1k rows at 120 columns.
pub const SCROLLBACK_BYTES: usize = 10 * 1024 * 1024;

/// Every byte a session's PTY writes goes through `process`, attached or not.
pub trait Spool {
    fn process(&mut self, data: &[u8]);
    fn resize(&mut self, size: Size);
    /// VT bytes that redraw the terminal as it is now on a fresh one.
    fn snapshot(&mut self) -> Vec<u8>;
}

/// `!Send`, like everything libghostty-vt hands out: create it on the
/// thread that uses it.
pub struct GhosttySpool {
    terminal: Terminal<'static, 'static>,
}

impl GhosttySpool {
    pub fn new(size: Size) -> Self {
        let terminal = Terminal::new(TerminalOptions {
            cols: size.cols.max(1),
            rows: size.rows.max(1),
            max_scrollback: SCROLLBACK_BYTES,
        })
        .expect("libghostty-vt creates a terminal");
        Self { terminal }
    }
}

impl Spool for GhosttySpool {
    fn process(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    fn resize(&mut self, size: Size) {
        let _ = self
            .terminal
            .resize(size.cols.max(1), size.rows.max(1), 0, 0);
    }

    fn snapshot(&mut self) -> Vec<u8> {
        let options = FormatterOptions::new()
            .with_format(Format::Vt)
            .with_palette(false)
            .with_modes(true)
            .with_scrolling_region(true)
            .with_tabstops(true)
            .with_pwd(true)
            .with_keyboard(true)
            .with_cursor(true)
            .with_style(true)
            .with_hyperlink(true)
            .with_protection(true)
            .with_kitty_keyboard(true)
            .with_charsets(true);
        let mut snapshot = Formatter::new(&self.terminal, options)
            .and_then(|mut formatter| formatter.format_alloc(None).map(|bytes| bytes.to_vec()))
            .unwrap_or_default();
        // The formatter moves the cursor after placing it (tabstops, DECSTBM).
        if let (Ok(x), Ok(y)) = (self.terminal.cursor_x(), self.terminal.cursor_y()) {
            snapshot.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        }
        snapshot
    }
}
