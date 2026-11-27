// SPDX-License-Identifier: Apache-2.0

//! Turns `alacritty_terminal`'s grid into the wire types: rows as sparse
//! style runs, plus the cursor and modes.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{self, NamedColor};
use coqo_protocol::methods::terminal::{
    Color, Cursor, CursorShape, Frame, Modes, Row, RowUpdate, Screen, Span, Style, TermSize, flags,
};

/// Grid dimensions for `Term::new` / `Term::resize`.
#[derive(Clone, Copy, Debug)]
pub struct Dims(pub TermSize);

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
    fn screen_lines(&self) -> usize {
        usize::from(self.0.rows)
    }
    fn columns(&self) -> usize {
        usize::from(self.0.cols)
    }
}

/// Everything in a frame except the rows, to tell whether anything changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Meta {
    size: Option<TermSize>,
    cursor: Option<Cursor>,
    modes: Modes,
    display_offset: u32,
    history_size: u32,
}

/// What the last frame left the client with, so the next one carries only
/// rows whose content really changed (damage is coarse: the cursor row is
/// always damaged, and a scroll damages everything).
#[derive(Clone, Debug, Default)]
pub struct Sent {
    meta: Meta,
    rows: Vec<Vec<Span>>,
}

pub fn size<T>(term: &Term<T>) -> TermSize {
    TermSize::new(
        u16::try_from(term.columns()).unwrap_or(u16::MAX),
        u16::try_from(term.screen_lines()).unwrap_or(u16::MAX),
    )
}

fn meta<T>(term: &Term<T>) -> Meta {
    let grid = term.grid();
    let offset = grid.display_offset();
    Meta {
        size: Some(size(term)),
        cursor: cursor(term),
        modes: modes(*term.mode()),
        display_offset: u32::try_from(offset).unwrap_or(u32::MAX),
        history_size: u32::try_from(grid.history_size()).unwrap_or(u32::MAX),
    }
}

fn cursor<T>(term: &Term<T>) -> Option<Cursor> {
    if !term.mode().contains(TermMode::SHOW_CURSOR) {
        return None;
    }
    let shape = match term.cursor_style().shape {
        ansi::CursorShape::Hidden => return None,
        ansi::CursorShape::Block => CursorShape::Block,
        ansi::CursorShape::Underline => CursorShape::Underline,
        ansi::CursorShape::Beam => CursorShape::Beam,
        ansi::CursorShape::HollowBlock => CursorShape::HollowBlock,
    };
    let grid = term.grid();
    let point = grid.cursor.point;
    let row = point.line.0 + i32::try_from(grid.display_offset()).ok()?;
    let row = u16::try_from(row).ok()?;
    (usize::from(row) < term.screen_lines()).then_some(Cursor {
        row,
        col: u16::try_from(point.column.0).ok()?,
        shape,
    })
}

fn modes(mode: TermMode) -> Modes {
    Modes {
        app_cursor: mode.contains(TermMode::APP_CURSOR),
        app_keypad: mode.contains(TermMode::APP_KEYPAD),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        mouse: mode.intersects(TermMode::MOUSE_MODE),
    }
}

/// The whole screen, for a snapshot.
pub fn screen<T>(term: &Term<T>) -> Screen {
    let m = meta(term);
    Screen {
        size: size(term),
        rows: (0..term.screen_lines())
            .map(|r| Row {
                spans: row(term, r),
            })
            .collect(),
        cursor: m.cursor,
        modes: m.modes,
        display_offset: m.display_offset,
        history_size: m.history_size,
    }
}

/// Everything in the grid, scrollback first, as plain text: one line per
/// logical line (wrapped rows joined), trailing blanks and the empty rest of
/// the screen dropped. `true` if the scrollback holds `scrollback` lines, so
/// earlier output may be gone.
pub fn text<T>(term: &Term<T>, scrollback: usize) -> (String, bool) {
    let grid = term.grid();
    let history = grid.history_size();
    let cols = grid.columns();
    let top = -i32::try_from(history).unwrap_or(i32::MAX);
    let bottom = i32::try_from(term.screen_lines()).unwrap_or(i32::MAX);
    let mut out = String::new();
    let mut line = String::new();
    for l in top..bottom {
        let cells = &grid[Line(l)];
        for col in 0..cols {
            let cell = &cells[Column(col)];
            if !cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                push_cell(&mut line, cell);
            }
        }
        let wrapped = cols > 0 && cells[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        if !wrapped {
            out.push_str(line.trim_end_matches(' '));
            out.push('\n');
            line.clear();
        }
    }
    out.push_str(line.trim_end_matches(' '));
    out.truncate(out.trim_end_matches('\n').len());
    if !out.is_empty() {
        out.push('\n');
    }
    (out, history >= scrollback)
}

/// The rows that changed since the last frame, or `None` if nothing did.
pub fn take_frame<T: EventListener>(term: &mut Term<T>, last: &mut Sent) -> Option<Frame> {
    let rows = term.screen_lines();
    let mut damaged: Vec<usize> = match term.damage() {
        TermDamage::Full => (0..rows).collect(),
        TermDamage::Partial(lines) => lines.map(|d| d.line).collect(),
    };
    term.reset_damage();
    let m = meta(term);
    // A new size starts the client over from blank rows.
    let full = last.meta.size != m.size;
    if full {
        last.rows = vec![Vec::new(); rows];
        damaged = (0..rows).collect();
    }
    damaged.sort_unstable();
    damaged.dedup();
    let mut updates = Vec::new();
    for r in damaged.into_iter().filter(|&r| r < rows) {
        let spans = row(term, r);
        if last.rows[r] != spans {
            last.rows[r].clone_from(&spans);
            updates.push(RowUpdate {
                row: u16::try_from(r).unwrap_or(u16::MAX),
                spans,
            });
        }
    }
    if !full && updates.is_empty() && m == last.meta {
        return None;
    }
    last.meta = m;
    Some(Frame {
        size: size(term),
        full,
        rows: updates,
        cursor: m.cursor,
        modes: m.modes,
        display_offset: m.display_offset,
        history_size: m.history_size,
    })
}

/// Viewport row `r` as spans.
fn row<T>(term: &Term<T>, r: usize) -> Vec<Span> {
    let grid = term.grid();
    let line = Line(r as i32 - grid.display_offset() as i32);
    let cells = &grid[line];
    let mut spans: Vec<Span> = Vec::new();
    let mut open: Option<(Span, usize)> = None; // (span, next column)
    for col in 0..grid.columns() {
        let cell = &cells[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let style = style(cell);
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        if !wide
            && let Some((span, next)) = open.as_mut()
            && !span.wide
            && span.style == style
            && *next == col
        {
            push_cell(&mut span.text, cell);
            *next = col + 1;
            continue;
        }
        if let Some((span, _)) = open.take() {
            spans.push(span);
        }
        let mut text = String::new();
        push_cell(&mut text, cell);
        open = Some((
            Span {
                col: u16::try_from(col).unwrap_or(u16::MAX),
                text,
                wide,
                style,
            },
            col + if wide { 2 } else { 1 },
        ));
    }
    spans.extend(open.map(|(s, _)| s));
    for span in &mut spans {
        if blank_is_invisible(&span.style) {
            span.text.truncate(span.text.trim_end_matches(' ').len());
        }
    }
    spans.retain(|s| !s.text.is_empty());
    spans
}

fn push_cell(text: &mut String, cell: &Cell) {
    text.push(cell.c);
    if let Some(extra) = cell.zerowidth() {
        text.extend(extra);
    }
}

/// Whether a space in this style looks like an empty cell.
fn blank_is_invisible(style: &Style) -> bool {
    style.bg == Color::Default
        && !style.has(flags::INVERSE | flags::UNDERLINE | flags::STRIKEOUT | flags::FANCY_UNDERLINE)
}

fn style(cell: &Cell) -> Style {
    let f = cell.flags;
    let mut bits = 0;
    for (flag, bit) in [
        (Flags::BOLD, flags::BOLD),
        (Flags::ITALIC, flags::ITALIC),
        (Flags::UNDERLINE, flags::UNDERLINE),
        (Flags::INVERSE, flags::INVERSE),
        (Flags::DIM, flags::DIM),
        (Flags::STRIKEOUT, flags::STRIKEOUT),
        (Flags::HIDDEN, flags::HIDDEN),
    ] {
        if f.contains(flag) {
            bits |= bit;
        }
    }
    if f.intersects(
        Flags::DOUBLE_UNDERLINE
            | Flags::UNDERCURL
            | Flags::DOTTED_UNDERLINE
            | Flags::DASHED_UNDERLINE,
    ) {
        bits |= flags::UNDERLINE | flags::FANCY_UNDERLINE;
    }
    let (fg, dim) = color(cell.fg);
    if dim {
        bits |= flags::DIM;
    }
    Style {
        fg,
        bg: color(cell.bg).0,
        flags: bits,
    }
}

/// The wire colour, and whether it was a dim named colour.
fn color(c: ansi::Color) -> (Color, bool) {
    match c {
        ansi::Color::Spec(rgb) => (Color::Rgb(rgb.r, rgb.g, rgb.b), false),
        ansi::Color::Indexed(i) => (Color::Indexed(i), false),
        ansi::Color::Named(n) => {
            let i = n as usize;
            if i < 16 {
                (Color::Indexed(i as u8), false)
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) {
                (
                    Color::Indexed((i - NamedColor::DimBlack as usize) as u8),
                    true,
                )
            } else {
                (Color::Default, n == NamedColor::DimForeground)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

    fn term(input: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &Dims(TermSize::new(20, 4)), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, input);
        term
    }

    #[test]
    fn rows_become_style_runs() {
        let t = term(b"ab \x1b[1;31mred\x1b[0m  x\r\n\x1b[44m  \x1b[0m\xe7\x95\x8cz");
        let s = screen(&t);
        assert_eq!(s.text(), "ab red  x\n  \u{754c}z");
        let spans = &s.rows[0].spans;
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!((spans[0].col, spans[0].text.as_str()), (0, "ab"));
        assert_eq!(spans[1].style.fg, Color::Indexed(1));
        assert!(spans[1].style.has(flags::BOLD));
        assert_eq!((spans[2].col, spans[2].text.as_str()), (6, "  x"));
        let second = &s.rows[1].spans;
        assert_eq!(second[0].style.bg, Color::Indexed(4));
        assert!(second[1].wide);
        assert_eq!((second[2].col, second[2].text.as_str()), (4, "z"));
        assert_eq!(s.cursor.map(|c| (c.row, c.col)), Some((1, 5)));
    }

    #[test]
    fn frames_carry_only_damaged_rows() {
        let mut t = term(b"one\r\ntwo");
        let mut last = Sent::default();
        let first = take_frame(&mut t, &mut last).unwrap();
        assert!(first.full);
        assert_eq!(first.rows.len(), 2, "blank rows are left out");
        assert!(take_frame(&mut t, &mut last).is_none());

        let mut parser: Processor = Processor::new();
        parser.advance(&mut t, b"\x1b[1;1Hz");
        let frame = take_frame(&mut t, &mut last).unwrap();
        assert!(!frame.full);
        let rows: Vec<u16> = frame.rows.iter().map(|r| r.row).collect();
        assert!(rows.contains(&0), "{rows:?}");
        assert!(rows.len() <= 2, "{rows:?}");

        let mut screen = Screen::blank(first.size);
        screen.apply(&first);
        screen.apply(&frame);
        assert_eq!(screen.text(), "zne\ntwo");
    }
}
