//! Snapshot fidelity (ADR 0025): the raw stream fed into one terminal and its
//! snapshot written into a fresh one must leave both terminals the same.

use std::time::{Duration, Instant};

use bitplane_protocol::session::Size;
use bitplane_sessiond::session::{GhosttySpool, SCROLLBACK_BYTES, Spool};
use libghostty_vt::render::{CellIteration, CellIterator, RowIterator};
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::terminal::Mode;
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

const SIZE: Size = Size {
    cols: 120,
    rows: 40,
};

#[test]
fn claude_code_restores_exactly() {
    assert_restores(include_bytes!("fixtures/claude-code.vt"));
}

#[test]
fn vim_on_the_alt_screen_restores_exactly() {
    assert_restores(include_bytes!("fixtures/vim-alt-screen.vt"));
}

#[test]
fn a_synthetic_stream_with_every_mode_restores_exactly() {
    assert_restores(&synthetic());
}

#[test]
fn every_cursor_shape_restores() {
    for decscusr in 0..=6 {
        let mut stream = format!("prompt $ \x1b[{decscusr} q").into_bytes();
        assert_restores(&stream);
        stream.extend_from_slice(b"\x1b[?12l");
        assert_restores(&stream);
    }
}

#[test]
fn every_mouse_mode_and_encoding_restores() {
    for mode in [9, 1000, 1002, 1003] {
        for encoding in [1005, 1006, 1015, 1016] {
            assert_restores(format!("\x1b[?{mode}h\x1b[?{encoding}h$ ").as_bytes());
        }
    }
}

#[test]
fn a_hidden_cursor_restores() {
    assert_restores(b"\x1b[2J\x1b[5;7Hhidden\x1b[?25l");
}

#[test]
fn blank_rows_under_a_coloured_pen_restore_blank() {
    let mut stream = history(60);
    stream.extend_from_slice(b"\x1b[10;1Hcursor here\x1b[44;31m");
    assert_restores(&stream);
}

#[test]
fn a_cleared_screen_over_history_restores() {
    let mut stream = history(60);
    stream.extend_from_slice(b"\x1b[H\x1b[2J$ ");
    assert_restores(&stream);
    stream.extend_from_slice(b"\x1b[3J");
    assert_restores(&stream);
}

#[test]
fn snapshots_carry_no_palette() {
    for stream in [
        include_bytes!("fixtures/claude-code.vt").to_vec(),
        include_bytes!("fixtures/vim-alt-screen.vt").to_vec(),
        synthetic(),
        b"\x1b]4;1;rgb:12/34/56\x07red".to_vec(),
    ] {
        let snapshot = snapshot_of(&stream);
        assert!(
            !contains(&snapshot, b"\x1b]4;"),
            "a snapshot overrides the client's palette"
        );
    }
}

#[test]
fn a_ten_thousand_row_history_snapshots_well_under_100_ms() {
    let mut spool = GhosttySpool::new(SIZE);
    for line in 0..10_000 {
        spool.process(
            format!(
                "\x1b[3{}m{line:>6}\x1b[0m {}\r\n",
                line % 7 + 1,
                "history ".repeat(13)
            )
            .as_bytes(),
        );
    }

    let fastest = (0..3)
        .map(|_| {
            let started = Instant::now();
            let snapshot = spool.snapshot();
            assert!(snapshot.len() > 500_000, "the history is in the snapshot");
            started.elapsed()
        })
        .min()
        .unwrap();

    assert!(
        fastest < Duration::from_millis(50),
        "snapshot took {fastest:?}"
    );
}

/// 300 lines of history, then every mode the criterion lists, a scroll
/// region, custom tabstops, styles, a title and a blinking bar cursor that
/// sits above blank rows.
fn synthetic() -> Vec<u8> {
    let mut stream = Vec::new();
    for line in 1..=300 {
        stream.extend_from_slice(
            format!(
                "\x1b[1;3{}mhistory {line}\x1b[0m plain \x1b[4munder\x1b[0m\r\n",
                line % 7
            )
            .as_bytes(),
        );
    }
    stream.extend_from_slice(
        concat!(
            "\x1b]2;synthetic: every mode\x07",
            "\x1b[?1h",    // DECCKM
            "\x1b=",       // DECKPAM
            "\x1b[?2004h", // bracketed paste
            "\x1b[?1002h", // button-event mouse
            "\x1b[?1006h", // SGR encoding
            "\x1b[?1004h", // focus events
            "\x1b[>13u",   // kitty: disambiguate, alternates, all as escapes
            "\x1b[3g\x1b[1;5H\x1bH\x1b[1;17H\x1bH",
            "\x1b[2J\x1b[H",
            "\x1b[48;5;24m\x1b[38;2;250;200;10m header with colours \x1b[K\x1b[0m\r\n",
            "\tcol5\tcol17\r\n",
            "\x1b[7mreverse\x1b[0m \x1b[3mitalic\x1b[0m \x1b[9mstrike\x1b[0m wide: 日本語\r\n",
            "\x1b[5;20r",
            "\x1b[20;1Hscrolled 1\r\nscrolled 2\r\nscrolled 3",
            "\x1b[r",
            "\x1b[5 q",
            "\x1b[12;34H",
        )
        .as_bytes(),
    );
    stream
}

fn history(lines: usize) -> Vec<u8> {
    (1..=lines)
        .flat_map(|line| format!("history {line}\r\n").into_bytes())
        .collect()
}

fn assert_restores(stream: &[u8]) {
    let mut reference = terminal();
    reference.vt_write(stream);
    let mut restored = terminal();
    restored.vt_write(&snapshot_of(stream));

    let (expected, actual) = (state(&reference), state(&restored));
    assert_eq!(expected.scalars, actual.scalars);
    for (row, (expected, actual)) in expected.rows.iter().zip(&actual.rows).enumerate() {
        if let Some(col) = (0..expected.len()).find(|&col| !restores(expected, actual, col)) {
            panic!(
                "viewport row {row} differs at column {col}\n  {:?}\n  {:?}\nraw:      {}\nrestored: {}",
                expected[col],
                actual[col],
                text(expected),
                text(actual)
            );
        }
    }
}

fn snapshot_of(stream: &[u8]) -> Vec<u8> {
    let mut spool = GhosttySpool::new(SIZE);
    spool.process(stream);
    spool.snapshot()
}

fn terminal() -> Terminal<'static, 'static> {
    Terminal::new(TerminalOptions {
        cols: SIZE.cols,
        rows: SIZE.rows,
        max_scrollback: SCROLLBACK_BYTES,
    })
    .unwrap()
}

#[derive(Debug, PartialEq)]
struct Scalars {
    cursor: (u16, u16),
    cursor_visible: bool,
    cursor_shape: String,
    cursor_blinking: bool,
    screen: String,
    modes: Vec<(u16, bool)>,
    kitty_flags: String,
    title: String,
    scrollback_rows: usize,
}

struct State {
    scalars: Scalars,
    rows: Vec<Vec<Cell>>,
}

#[derive(Debug)]
struct Cell {
    text: String,
    look: String,
    bg: Option<RgbColor>,
    unwritten: bool,
}

const MODES: [Mode; 14] = [
    Mode::DECCKM,
    Mode::BRACKETED_PASTE,
    Mode::X10_MOUSE,
    Mode::NORMAL_MOUSE,
    Mode::BUTTON_MOUSE,
    Mode::ANY_MOUSE,
    Mode::UTF8_MOUSE,
    Mode::SGR_MOUSE,
    Mode::URXVT_MOUSE,
    Mode::SGR_PIXELS_MOUSE,
    Mode::FOCUS_EVENT,
    Mode::KEYPAD_KEYS,
    Mode::CURSOR_VISIBLE,
    Mode::CURSOR_BLINKING,
];

fn state(terminal: &Terminal<'static, 'static>) -> State {
    let mut render = RenderState::new().unwrap();
    let snapshot = render.update(terminal).unwrap();
    let scalars = Scalars {
        cursor: (terminal.cursor_x().unwrap(), terminal.cursor_y().unwrap()),
        cursor_visible: terminal.is_cursor_visible().unwrap(),
        cursor_shape: format!("{:?}", snapshot.cursor_visual_style().unwrap()),
        cursor_blinking: snapshot.cursor_blinking().unwrap(),
        screen: format!("{:?}", terminal.active_screen().unwrap()),
        modes: MODES
            .iter()
            .map(|&mode| (mode.value(), terminal.mode(mode).unwrap()))
            .collect(),
        kitty_flags: format!("{:?}", terminal.kitty_keyboard_flags().unwrap()),
        title: terminal.title().unwrap().to_owned(),
        scrollback_rows: terminal.scrollback_rows().unwrap(),
    };

    let mut rows = Vec::new();
    let mut row_iterator = RowIterator::new().unwrap();
    let mut cell_iterator = CellIterator::new().unwrap();
    let mut row_iteration = row_iterator.update(&snapshot).unwrap();
    while let Some(row) = row_iteration.next() {
        let mut line = Vec::new();
        let mut cells = cell_iterator.update(row).unwrap();
        while let Some(cell) = cells.next() {
            line.push(cell_of(cell));
        }
        rows.push(line);
    }
    assert_eq!(rows.len(), SIZE.rows as usize);
    State { scalars, rows }
}

/// What a cell renders as. The formatter writes never-written cells as
/// spaces in the current pen, so a blank only keeps the pen where it shows.
fn cell_of(cell: &CellIteration) -> Cell {
    let text: String = cell.graphemes().unwrap().into_iter().collect();
    let text = if text.is_empty() {
        " ".to_owned()
    } else {
        text
    };
    let style = cell.style().unwrap();
    let bg = cell.bg_color().unwrap();
    let pen_shows = style.inverse
        || style.strikethrough
        || style.overline
        || style.underline != Underline::None;
    let look = if text == " " && !pen_shows {
        format!("blank on {bg:?}")
    } else {
        format!("{style:?} {:?} {bg:?}", cell.fg_color().unwrap())
    };
    Cell {
        text,
        look,
        bg,
        unwritten: cell.graphemes_len().unwrap() == 0 && !cell.has_styling().unwrap(),
    }
}

/// The formatter flushes never-written cells as spaces before switching
/// pens, so they take the previous cell's background (a formatter gap).
fn restores(expected: &[Cell], actual: &[Cell], col: usize) -> bool {
    let (want, got) = (&expected[col], &actual[col]);
    if want.text == got.text && want.look == got.look {
        return true;
    }
    want.unwritten
        && col > 0
        && got.text == " "
        && got.look == format!("blank on {:?}", actual[col - 1].bg)
}

fn text(row: &[Cell]) -> String {
    row.iter().map(|cell| cell.text.as_str()).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
