// SPDX-License-Identifier: Apache-2.0

//! Terminals (`terminal/*`): PTYs owned by the daemon, so they survive the UI
//! closing (PLAN §7.3: Claude terminal mode runs in one).
//!
//! The daemon keeps the emulator state (grid, scrollback, modes). A client
//! shows a terminal by mirroring its [`Screen`], the rows currently in view:
//!
//! 1. [`Snapshot`] returns the screen and the `seq` it reflects.
//! 2. Subscribe to [`Output`] with `since: seq`. Each [`TerminalEvent::Frame`]
//!    carries only the rows that changed since the previous frame (frames are
//!    batched at about 60 Hz), plus the cursor and modes; apply it with
//!    [`Screen::apply`].
//! 3. On `RESUME_GAP`, take a new snapshot and resubscribe.
//!
//! Rows hold absolute content, never diffs within a row, so applying a frame
//! twice is harmless. Scrollback stays in the daemon: [`Scroll`] moves the
//! terminal's viewport (shared by every attached client), and the next frame
//! shows the scrolled rows.
//!
//! Requests on one connection run concurrently, so a client sends the next
//! [`Input`] only after the previous one's response (coalescing keystrokes
//! typed in the meantime) to keep them in order.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::method::{Method, Topic};

pub use super::codes::{NO_SUCH_TERMINAL, SPAWN_FAILED};

/// Identifies a terminal for the lifetime of the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct TerminalId(pub u64);

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A terminal's size in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

impl TermSize {
    /// A size, at least one cell each way.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

impl Default for TermSize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// Params of [`Spawn`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SpawnParams {
    /// The program to run. Omit for the user's login shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Arguments to `program`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Working directory. Omit for the user's home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment variables, on top of the daemon's environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Initial size.
    #[serde(default)]
    pub size: TermSize,
    /// Tab title until the program sets one. Defaults to the program name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Whether a terminal's process is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum TerminalStatus {
    Running,
    /// The process exited. `code` is `None` when a signal ended it.
    Exited {
        #[serde(default)]
        code: Option<i32>,
    },
}

/// What the daemon knows about a terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TerminalInfo {
    pub id: TerminalId,
    /// The title: set by the program (OSC 0/2), else the spawn title.
    pub title: String,
    /// The program that was started.
    pub program: String,
    /// The working directory: as reported by the shell (OSC 7), else the
    /// process's own (where the OS can tell), else the spawn directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub size: TermSize,
    /// Process id of the program (also its process group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub status: TerminalStatus,
    /// Exit code of the last shell command, if the shell reports it (OSC 133;D).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_exit: Option<i32>,
}

/// Names one terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TerminalRef {
    pub id: TerminalId,
}

/// Params of [`Input`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct InputParams {
    pub id: TerminalId,
    /// Bytes for the PTY, as text: typed characters, escape sequences for
    /// keys, or pasted text (already wrapped for bracketed paste).
    pub data: String,
}

/// Params of [`Resize`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ResizeParams {
    pub id: TerminalId,
    pub size: TermSize,
}

/// How to move a terminal's viewport through its scrollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ScrollTo {
    /// Up into the scrollback by this many lines (negative: down).
    Lines(i32),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// Params of [`Scroll`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ScrollParams {
    pub id: TerminalId,
    pub scroll: ScrollTo,
}

/// A cell colour. Indexed colours are resolved by the client's theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum Color {
    /// The default foreground or background.
    #[default]
    Default,
    /// The 256-colour palette: 0–15 are the ANSI colours.
    Indexed(u8),
    /// True colour.
    Rgb(u8, u8, u8),
}

/// Style flags of a [`Style`], as bits.
pub mod flags {
    pub const BOLD: u16 = 1;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    /// Swap foreground and background.
    pub const INVERSE: u16 = 1 << 3;
    pub const DIM: u16 = 1 << 4;
    pub const STRIKEOUT: u16 = 1 << 5;
    /// Drawn as blank.
    pub const HIDDEN: u16 = 1 << 6;
    /// Curly, double, dotted or dashed underline.
    pub const FANCY_UNDERLINE: u16 = 1 << 7;
}

/// How a run of cells is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Style {
    #[serde(default, skip_serializing_if = "is_default")]
    pub fg: Color,
    #[serde(default, skip_serializing_if = "is_default")]
    pub bg: Color,
    /// Bits from [`flags`].
    #[serde(default, skip_serializing_if = "is_default")]
    pub flags: u16,
}

impl Style {
    /// Whether `flag` (from [`flags`]) is set.
    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    /// Whether this is the plain default style.
    pub fn is_default(&self) -> bool {
        *self == Style::default()
    }
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// A run of cells with one style, starting at column `col`. Each character
/// of `text` takes one cell, or two in a `wide` span (CJK, emoji). Blank
/// default-styled cells are left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Span {
    pub col: u16,
    pub text: String,
    /// Every character is two cells wide.
    #[serde(default, skip_serializing_if = "is_default")]
    pub wide: bool,
    #[serde(default, skip_serializing_if = "Style::is_default")]
    pub style: Style,
}

/// One row of the screen, as sparse spans in column order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Row {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
}

impl Row {
    /// The row as plain text, with blanks filled in and trailing blanks trimmed.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let mut col = 0usize;
        for span in &self.spans {
            let start = usize::from(span.col);
            while col < start {
                out.push(' ');
                col += 1;
            }
            out.push_str(&span.text);
            col += span_width(span);
        }
        out.truncate(out.trim_end().len());
        out
    }

    /// Text of the cells in `cols` (half open), for copying a selection.
    pub fn text_in(&self, cols: std::ops::Range<u16>) -> String {
        let mut out = String::new();
        let mut col = cols.start;
        for span in &self.spans {
            let mut c = span.col;
            for ch in span.text.chars() {
                let width = if span.wide { 2 } else { 1 };
                if c >= cols.start && c < cols.end {
                    while col < c {
                        out.push(' ');
                        col += 1;
                    }
                    out.push(ch);
                    col = c + width;
                }
                c += width;
            }
        }
        out.truncate(out.trim_end().len());
        out
    }
}

/// Cells taken by a span.
pub fn span_width(span: &Span) -> usize {
    span.text.chars().count() * if span.wide { 2 } else { 1 }
}

/// Cursor shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
}

/// The cursor, in screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    #[serde(default, skip_serializing_if = "is_default")]
    pub shape: CursorShape,
}

/// Terminal modes that change how a client encodes input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Modes {
    /// DECCKM: arrow keys send `ESC O A` instead of `ESC [ A`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub app_cursor: bool,
    /// DECKPAM: application keypad.
    #[serde(default, skip_serializing_if = "is_default")]
    pub app_keypad: bool,
    /// Wrap pasted text in `ESC [200~` … `ESC [201~`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub bracketed_paste: bool,
    /// The alternate screen is active (full-screen programs such as `top`);
    /// it has no scrollback, so a client turns wheel scrolling into arrow keys.
    #[serde(default, skip_serializing_if = "is_default")]
    pub alt_screen: bool,
    /// The program asked for mouse reports.
    #[serde(default, skip_serializing_if = "is_default")]
    pub mouse: bool,
}

/// The rows in view, the cursor and the modes: what a client draws.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Screen {
    pub size: TermSize,
    /// Exactly `size.rows` rows, top to bottom.
    pub rows: Vec<Row>,
    /// `None` when hidden or scrolled out of view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(default)]
    pub modes: Modes,
    /// How many lines the viewport is scrolled up into the scrollback.
    #[serde(default)]
    pub display_offset: u32,
    /// Lines of scrollback above the screen.
    #[serde(default)]
    pub history_size: u32,
}

impl Screen {
    /// A blank screen of `size`.
    pub fn blank(size: TermSize) -> Self {
        Self {
            size,
            rows: vec![Row::default(); usize::from(size.rows)],
            ..Self::default()
        }
    }

    /// Apply a frame from [`Output`].
    pub fn apply(&mut self, frame: &Frame) {
        if frame.full || frame.size != self.size {
            self.rows = vec![Row::default(); usize::from(frame.size.rows)];
        }
        self.size = frame.size;
        self.rows
            .resize(usize::from(frame.size.rows), Row::default());
        for update in &frame.rows {
            if let Some(row) = self.rows.get_mut(usize::from(update.row)) {
                row.spans.clone_from(&update.spans);
            }
        }
        self.cursor = frame.cursor;
        self.modes = frame.modes;
        self.display_offset = frame.display_offset;
        self.history_size = frame.history_size;
    }

    /// The rows as plain text, one line each, trailing blank lines trimmed.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = self.rows.iter().map(Row::text).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// One row of a [`Frame`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RowUpdate {
    /// Row index, 0 at the top of the viewport.
    pub row: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
}

/// What changed on screen since the previous frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Frame {
    pub size: TermSize,
    /// The screen starts over (after a resize): rows left out are blank.
    #[serde(default, skip_serializing_if = "is_default")]
    pub full: bool,
    /// The changed rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<RowUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(default)]
    pub modes: Modes,
    #[serde(default)]
    pub display_offset: u32,
    #[serde(default)]
    pub history_size: u32,
}

/// An event of [`Output`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TerminalEvent {
    /// The screen changed.
    Frame(Frame),
    /// The program set the title (OSC 0/2), or reset it.
    Title { title: String },
    /// The shell reported a new working directory (OSC 7).
    Cwd { cwd: String },
    /// The shell reported a finished command (OSC 133;D).
    CommandFinished {
        #[serde(default)]
        code: Option<i32>,
    },
    /// The bell rang.
    Bell,
    /// The process exited. The terminal stays until [`Close`] so its last
    /// output can still be read.
    Exited {
        #[serde(default)]
        code: Option<i32>,
    },
}

/// The result of [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TerminalSnapshot {
    /// The [`Output`] seq this snapshot reflects: subscribe with `since: seq`.
    pub seq: u64,
    pub info: TerminalInfo,
    pub screen: Screen,
}

/// `terminal/spawn`: start a program (or the login shell) in a new PTY.
#[derive(Debug)]
pub enum Spawn {}
impl Method for Spawn {
    const NAME: &'static str = "terminal/spawn";
    type Params = SpawnParams;
    type Result = TerminalInfo;
}

/// `terminal/list`: every terminal the daemon holds, running or exited.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "terminal/list";
    type Params = ();
    type Result = Vec<TerminalInfo>;
}

/// `terminal/snapshot`: the current screen and the seq it reflects.
#[derive(Debug)]
pub enum Snapshot {}
impl Method for Snapshot {
    const NAME: &'static str = "terminal/snapshot";
    type Params = TerminalRef;
    type Result = TerminalSnapshot;
}

/// `terminal/input`: write to the PTY. Scrolls the viewport to the bottom.
#[derive(Debug)]
pub enum Input {}
impl Method for Input {
    const NAME: &'static str = "terminal/input";
    type Params = InputParams;
    type Result = ();
}

/// `terminal/resize`: change the size in cells.
#[derive(Debug)]
pub enum Resize {}
impl Method for Resize {
    const NAME: &'static str = "terminal/resize";
    type Params = ResizeParams;
    type Result = ();
}

/// `terminal/scroll`: move the viewport through the scrollback.
#[derive(Debug)]
pub enum Scroll {}
impl Method for Scroll {
    const NAME: &'static str = "terminal/scroll";
    type Params = ScrollParams;
    type Result = ();
}

/// `terminal/close`: end the process (its whole process group) and forget
/// the terminal. Result is `true` if it existed.
#[derive(Debug)]
pub enum Close {}
impl Method for Close {
    const NAME: &'static str = "terminal/close";
    type Params = TerminalRef;
    type Result = bool;
}

/// `terminal/output`: frames and other events of one terminal.
#[derive(Debug)]
pub enum Output {}
impl Topic for Output {
    const NAME: &'static str = "terminal/output";
    type Params = TerminalRef;
    type Event = TerminalEvent;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(col: u16, text: &str) -> Span {
        Span {
            col,
            text: text.into(),
            wide: false,
            style: Style::default(),
        }
    }

    #[test]
    fn rows_render_as_text_with_gaps_filled() {
        let row = Row {
            spans: vec![
                span(0, "ls"),
                span(4, "-la"),
                Span {
                    wide: true,
                    ..span(10, "界")
                },
            ],
        };
        assert_eq!(row.text(), "ls  -la   界");
        assert_eq!(row.text_in(1..6), "s  -l");
        assert_eq!(row.text_in(10..12), "界");
    }

    #[test]
    fn frames_update_rows_and_resize_the_screen() {
        let mut screen = Screen::blank(TermSize::new(10, 3));
        let frame = Frame {
            size: TermSize::new(10, 3),
            full: false,
            rows: vec![RowUpdate {
                row: 1,
                spans: vec![span(0, "hello")],
            }],
            cursor: Some(Cursor {
                row: 1,
                col: 5,
                shape: CursorShape::Block,
            }),
            modes: Modes::default(),
            display_offset: 0,
            history_size: 0,
        };
        screen.apply(&frame);
        screen.apply(&frame); // idempotent
        assert_eq!(screen.text(), "\nhello");
        assert_eq!(screen.cursor.map(|c| (c.row, c.col)), Some((1, 5)));

        screen.apply(&Frame {
            size: TermSize::new(20, 5),
            full: true,
            rows: vec![RowUpdate {
                row: 4,
                spans: vec![span(2, "x")],
            }],
            ..frame
        });
        assert_eq!(screen.rows.len(), 5);
        assert_eq!(screen.text(), "\n\n\n\n  x");
    }

    #[test]
    fn events_have_a_compact_tagged_wire_form() {
        let event = TerminalEvent::Frame(Frame {
            size: TermSize::new(80, 24),
            full: false,
            rows: vec![RowUpdate {
                row: 0,
                spans: vec![Span {
                    col: 0,
                    text: "$ ".into(),
                    wide: false,
                    style: Style {
                        fg: Color::Indexed(2),
                        bg: Color::Default,
                        flags: flags::BOLD,
                    },
                }],
            }],
            cursor: None,
            modes: Modes::default(),
            display_offset: 0,
            history_size: 0,
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "frame",
                "size": {"cols": 80, "rows": 24},
                "rows": [{"row": 0, "spans": [{"col": 0, "text": "$ ", "style": {"fg": {"indexed": 2}, "flags": 1}}]}],
                "modes": {},
                "display_offset": 0,
                "history_size": 0,
            })
        );
        let back: TerminalEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);

        let exited = serde_json::to_value(TerminalEvent::Exited { code: Some(3) }).unwrap();
        assert_eq!(exited, serde_json::json!({"type": "exited", "code": 3}));
        let bytes = rmp_serde::to_vec_named(&event).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<TerminalEvent>(&bytes).unwrap(),
            event
        );
    }
}
