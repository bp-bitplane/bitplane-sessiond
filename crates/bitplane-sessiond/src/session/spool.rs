//! A session's terminal state (ADR 0025), behind a trait in the shape of
//! shpool's `SessionSpool`.

use bitplane_protocol::session::Size;
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::CursorVisualStyle;
use libghostty_vt::terminal::{Point, PointCoordinate};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

/// Scrollback budget per session, in bytes. ghostty's `max_scrollback` is a
/// byte budget despite its name (PageList `max_size`), never a line count:
/// about 1 MiB holds 1k rows at 120 columns, so this keeps about 10k.
pub const SCROLLBACK_BYTES: usize = 10 * 1024 * 1024;

/// Every byte a session's PTY writes goes through `process`, attached or not.
pub trait Spool {
    fn process(&mut self, data: &[u8]);
    fn resize(&mut self, size: Size);
    /// The title the program set (OSC 0/2), without control characters.
    fn title(&self) -> Option<String>;
    /// VT bytes that redraw the terminal as it is now on a fresh one.
    fn snapshot(&mut self) -> Vec<u8>;
}

/// `!Send`, like everything libghostty-vt hands out: create it on the
/// thread that uses it.
pub struct GhosttySpool {
    terminal: Terminal<'static, 'static>,
    render: RenderState<'static>,
}

impl GhosttySpool {
    pub fn new(size: Size) -> Self {
        let terminal = Terminal::new(TerminalOptions {
            cols: size.cols.max(1),
            rows: size.rows.max(1),
            max_scrollback: SCROLLBACK_BYTES,
        })
        .expect("libghostty-vt creates a terminal");
        let render = RenderState::new().expect("libghostty-vt creates a render state");
        Self { terminal, render }
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

    fn title(&self) -> Option<String> {
        let title = self.terminal.title().ok()?;
        Some(title.chars().filter(|c| !c.is_control()).collect()).filter(|t: &String| !t.is_empty())
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
        let (Ok(x), Ok(y)) = (self.terminal.cursor_x(), self.terminal.cursor_y()) else {
            return snapshot;
        };
        let cup = format!("\x1b[{};{}H", y + 1, x + 1).into_bytes();
        // The formatter drops trailing blank rows. Its CUP precedes DECSTBM, so
        // the region is still full-screen there.
        let padding = self.blank_row_padding();
        let at = rfind(&snapshot, &cup).unwrap_or(snapshot.len());
        snapshot.splice(at..at, padding);
        if let Some(title) = self.title() {
            snapshot.extend_from_slice(format!("\x1b]2;{title}\x1b\\").as_bytes());
        }
        if let Some(decscusr) = self.decscusr() {
            snapshot.extend_from_slice(format!("\x1b[{decscusr} q").as_bytes());
        }
        // The formatter moves the cursor after placing it (tabstops, DECSTBM).
        snapshot.extend_from_slice(&cup);
        snapshot
    }
}

impl GhosttySpool {
    /// LFs at the bottom row that scroll the formatter's output until the
    /// restored terminal holds as many rows as this one, under a saved pen so
    /// the new rows stay unstyled.
    fn blank_row_padding(&self) -> Vec<u8> {
        let (Ok(rows), Ok(scrollback)) = (self.terminal.rows(), self.terminal.scrollback_rows())
        else {
            return Vec::new();
        };
        let rows = rows as usize;
        let emitted = self.last_row_with_text().map_or(0, |row| row + 1);
        let missing = (scrollback + rows).saturating_sub(emitted.max(rows));
        if missing == 0 {
            return Vec::new();
        }
        let mut padding = format!("\x1b7\x1b[m\x1b[{rows};1H").into_bytes();
        padding.resize(padding.len() + missing, b'\n');
        padding.extend_from_slice(b"\x1b8");
        padding
    }

    /// The last row, counted over scrollback and screen, with any text.
    fn last_row_with_text(&self) -> Option<usize> {
        let cols = self.terminal.cols().ok()?;
        let total = self.terminal.total_rows().ok()?;
        (0..total).rev().find(|&row| {
            (0..cols).any(|x| {
                let point = Point::Screen(PointCoordinate { x, y: row as u32 });
                self.terminal
                    .grid_ref(point)
                    .and_then(|cell| cell.cell()?.has_text())
                    .unwrap_or(false)
            })
        })
    }

    /// DECSCUSR for the cursor as it renders; a hollow block is a block that
    /// lost focus, which DECSCUSR cannot express.
    fn decscusr(&mut self) -> Option<u8> {
        let state = self.render.update(&self.terminal).ok()?;
        let steady = !state.cursor_blinking().ok()?;
        let blinking = match state.cursor_visual_style().ok()? {
            CursorVisualStyle::Block | CursorVisualStyle::BlockHollow => 1,
            CursorVisualStyle::Underline => 3,
            CursorVisualStyle::Bar => 5,
            _ => return None,
        };
        Some(blinking + u8::from(steady))
    }
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}
