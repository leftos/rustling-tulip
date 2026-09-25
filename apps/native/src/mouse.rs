//! Mouse input for the terminal pane: which grid cell a pixel falls in, and
//! the xterm mouse reports sent to a child that asked for them.

use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::{TermMode, viewport_to_point};

use crate::term::GridSize;

/// Copy a mouse selection to the clipboard as soon as the button is released.
pub const COPY_ON_SELECT: bool = true;

/// X10 encodes a coordinate as one byte offset by 32, so it cannot go past 223.
const X10_MAX_COORD: usize = 223;

/// xterm's limit in UTF-8 mode: 2015 + 32 is the last two-byte character.
const UTF8_MAX_COORD: usize = 2015;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSize {
    pub width: f32,
    pub height: f32,
}

/// A cell of the visible screen, 0-based, and which half of it was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportCell {
    pub row: usize,
    pub col: usize,
    pub side: Side,
}

impl ViewportCell {
    /// The cell under `(x, y)`, in pixels from the grid's top-left corner,
    /// clamped to the grid.
    pub fn at(x: f32, y: f32, cell: CellSize, size: GridSize) -> Self {
        let col = (x / cell.width).max(0.0);
        let row = (y / cell.height).max(0.0);
        let last_col = size.cols.saturating_sub(1);
        let (col, side) = match whole(col) {
            c if c > last_col => (last_col, Side::Right),
            c if col.fract() < 0.5 => (c, Side::Left),
            c => (c, Side::Right),
        };
        Self {
            row: whole(row).min(size.rows.saturating_sub(1)),
            col,
            side,
        }
    }

    /// The grid point of this cell while the view is `display_offset` lines
    /// back in history.
    pub fn to_point(self, display_offset: usize) -> Point {
        viewport_to_point(display_offset, Point::new(self.row, Column(self.col)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    /// Motion with no button held.
    None,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub button: Button,
    pub kind: ReportKind,
    pub mods: Mods,
    pub cell: ViewportCell,
}

/// The report format the child asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// `CSI < b ; x ; y M/m` (mode 1006).
    Sgr,
    /// X10 with each coordinate as a UTF-8 character (mode 1005).
    Utf8,
    /// `CSI M` and three raw bytes.
    X10,
}

impl Encoding {
    pub fn of(mode: TermMode) -> Self {
        if mode.contains(TermMode::SGR_MOUSE) {
            Self::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            Self::Utf8
        } else {
            Self::X10
        }
    }
}

/// Encodes `report` in `encoding`. Coordinates are 1-based.
pub fn encode(report: &Report, encoding: Encoding) -> Vec<u8> {
    let sgr = encoding == Encoding::Sgr;
    let mods = report.mods;
    let mod_bits = u8::from(mods.shift) * 4 + u8::from(mods.alt) * 8 + u8::from(mods.ctrl) * 16;
    let motion = if report.kind == ReportKind::Motion {
        32
    } else {
        0
    };
    let code = button_code(report.button) + motion + mod_bits;
    let (x, y) = (report.cell.col + 1, report.cell.row + 1);
    if sgr {
        let end = if report.kind == ReportKind::Release {
            'm'
        } else {
            'M'
        };
        return format!("\x1b[<{code};{x};{y}{end}").into_bytes();
    }
    // X10 has no button number on release.
    let code = if report.kind == ReportKind::Release {
        button_code(Button::None) + mod_bits
    } else {
        code
    };
    let mut bytes = vec![0x1b, b'[', b'M', 32 + code];
    if encoding == Encoding::Utf8 {
        push_utf8_coord(&mut bytes, x);
        push_utf8_coord(&mut bytes, y);
    } else {
        bytes.extend([x10_coord(x), x10_coord(y)]);
    }
    bytes
}

/// Mode 1005: the coordinate plus 32 as one UTF-8 character.
fn push_utf8_coord(bytes: &mut Vec<u8>, coord: usize) {
    let value = u32::try_from(32 + coord.min(UTF8_MAX_COORD)).unwrap_or(u32::MAX);
    let ch = char::from_u32(value).unwrap_or(char::REPLACEMENT_CHARACTER);
    let mut buf = [0; 4];
    bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

fn button_code(button: Button) -> u8 {
    match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::None => 3,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    }
}

fn x10_coord(coord: usize) -> u8 {
    u8::try_from(32 + coord.min(X10_MAX_COORD)).unwrap_or(u8::MAX)
}

/// Who a mouse gesture belongs to, decided when its first button goes down
/// and kept until its last button comes up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// The child asked for the mouse: presses, motion and releases go to it.
    Report,
    /// A left-button text selection.
    Select,
}

/// The gesture in progress and the buttons it has pressed.
#[derive(Debug, Default)]
pub struct Tracker {
    gesture: Option<Gesture>,
    /// Bit per button, in [`button_bit`] order.
    pressed: u8,
}

fn button_bit(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 4,
        Button::None | Button::WheelUp | Button::WheelDown => 0,
    }
}

impl Tracker {
    /// A button went down with the terminal in `mode`. Returns the gesture
    /// that handles it: a report of the press, the start of a selection, or
    /// `None` to ignore it.
    pub fn down(&mut self, button: Button, mode: TermMode, shift: bool) -> Option<Gesture> {
        let bit = button_bit(button);
        match self.gesture {
            Some(Gesture::Report) => {
                self.pressed |= bit;
                Some(Gesture::Report)
            }
            Some(Gesture::Select) => None,
            None => {
                let gesture = if reports(mode, shift) {
                    Gesture::Report
                } else if button == Button::Left {
                    Gesture::Select
                } else {
                    return None;
                };
                self.gesture = Some(gesture);
                self.pressed = bit;
                Some(gesture)
            }
        }
    }

    /// The gesture a move belongs to, or `None` when no button is down.
    pub fn moving(&self) -> Option<Gesture> {
        self.gesture
    }

    /// A button came up. Returns the gesture that handles the release, or
    /// `None` for a button this gesture never pressed.
    pub fn up(&mut self, button: Button) -> Option<Gesture> {
        let bit = button_bit(button);
        if self.pressed & bit == 0 {
            return None;
        }
        self.pressed &= !bit;
        let gesture = self.gesture;
        if self.pressed == 0 {
            self.gesture = None;
        }
        gesture
    }

    /// The button a drag report names: the lowest one held.
    pub fn held(&self) -> Option<Button> {
        [Button::Left, Button::Middle, Button::Right]
            .into_iter()
            .find(|b| self.pressed & button_bit(*b) != 0)
    }
}

/// The whole part of a non-negative cell coordinate.
fn whole(v: f32) -> usize {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "non-negative, and a pixel offset over a cell size is small"
    )]
    let n = v.floor() as usize;
    n
}

/// Whether the mouse goes to the child rather than selecting: the child
/// asked for reports and Shift is not held.
pub fn reports(mode: TermMode, shift: bool) -> bool {
    mode.intersects(TermMode::MOUSE_MODE) && !shift
}

/// Whether a move is reported: any motion in motion mode, and motion with a
/// button held in drag mode.
pub fn reports_motion(mode: TermMode, button_held: bool) -> bool {
    mode.contains(TermMode::MOUSE_MOTION) || (button_held && mode.contains(TermMode::MOUSE_DRAG))
}

/// The wheel sends arrow keys on the alternate screen when the program
/// asked for alternate scroll and not for mouse reports.
pub fn wheel_sends_arrows(mode: TermMode) -> bool {
    !mode.intersects(TermMode::MOUSE_MODE)
        && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
}

/// Up (positive `lines`) or down arrows, one per line scrolled.
pub fn wheel_arrows(lines: i32, app_cursor: bool) -> Vec<u8> {
    let lead = if app_cursor { b'O' } else { b'[' };
    let letter = if lines > 0 { b'A' } else { b'B' };
    [0x1b, lead, letter].repeat(lines.unsigned_abs() as usize)
}

/// A single click starts a character selection, a double click selects a
/// word and a triple click a line.
pub fn selection_type(click_count: usize) -> SelectionType {
    match click_count {
        0 | 1 => SelectionType::Simple,
        2 => SelectionType::Semantic,
        _ => SelectionType::Lines,
    }
}

/// The text to put on the clipboard when a selection drag ends.
pub fn copy_on_select(enabled: bool, selection: Option<String>) -> Option<String> {
    selection.filter(|text| enabled && !text.is_empty())
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line, Point, Side};

    use alacritty_terminal::term::TermMode;

    use super::{
        Button, CellSize, Encoding, Gesture, Mods, Report, ReportKind, Tracker, ViewportCell,
        copy_on_select, encode,
    };
    use crate::term::GridSize;

    const CELL: CellSize = CellSize {
        width: 10.0,
        height: 20.0,
    };
    const GRID: GridSize = GridSize { cols: 80, rows: 24 };

    fn cell(row: usize, col: usize) -> ViewportCell {
        ViewportCell {
            row,
            col,
            side: Side::Left,
        }
    }

    fn report(button: Button, kind: ReportKind, at: ViewportCell) -> Report {
        Report {
            button,
            kind,
            mods: Mods::default(),
            cell: at,
        }
    }

    #[test]
    fn pixel_maps_to_cell_and_side() {
        assert_eq!(ViewportCell::at(0.0, 0.0, CELL, GRID), cell(0, 0));
        assert_eq!(
            ViewportCell::at(36.0, 45.0, CELL, GRID),
            ViewportCell {
                row: 2,
                col: 3,
                side: Side::Right
            }
        );
        assert_eq!(ViewportCell::at(34.0, 45.0, CELL, GRID), cell(2, 3));
    }

    #[test]
    fn pixel_outside_the_grid_clamps_to_its_edge() {
        assert_eq!(ViewportCell::at(-5.0, -5.0, CELL, GRID), cell(0, 0));
        assert_eq!(
            ViewportCell::at(5000.0, 5000.0, CELL, GRID),
            ViewportCell {
                row: 23,
                col: 79,
                side: Side::Right
            }
        );
    }

    #[test]
    fn scrolled_back_view_maps_to_history_lines() {
        let at = ViewportCell::at(15.0, 5.0, CELL, GRID);
        assert_eq!(at.to_point(0), Point::new(Line(0), Column(1)));
        assert_eq!(at.to_point(5), Point::new(Line(-5), Column(1)));
    }

    #[test]
    fn sgr_press_and_release() {
        let press = report(Button::Left, ReportKind::Press, cell(4, 9));
        assert_eq!(encode(&press, Encoding::Sgr), b"\x1b[<0;10;5M".to_vec());
        let release = report(Button::Right, ReportKind::Release, cell(4, 9));
        assert_eq!(encode(&release, Encoding::Sgr), b"\x1b[<2;10;5m".to_vec());
    }

    #[test]
    fn x10_press_and_release() {
        let press = report(Button::Middle, ReportKind::Press, cell(0, 0));
        assert_eq!(
            encode(&press, Encoding::X10),
            vec![0x1b, b'[', b'M', 33, 33, 33]
        );
        let release = report(Button::Middle, ReportKind::Release, cell(0, 0));
        assert_eq!(
            encode(&release, Encoding::X10),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
    }

    #[test]
    fn x10_clamps_coordinates() {
        let far = report(Button::Left, ReportKind::Press, cell(300, 250));
        assert_eq!(
            encode(&far, Encoding::X10),
            vec![0x1b, b'[', b'M', 32, 255, 255]
        );
    }

    #[test]
    fn wheel_reports_buttons_64_and_65() {
        let up = report(Button::WheelUp, ReportKind::Press, cell(0, 0));
        let down = report(Button::WheelDown, ReportKind::Press, cell(0, 0));
        assert_eq!(encode(&up, Encoding::Sgr), b"\x1b[<64;1;1M".to_vec());
        assert_eq!(encode(&down, Encoding::Sgr), b"\x1b[<65;1;1M".to_vec());
        assert_eq!(
            encode(&up, Encoding::X10),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
    }

    #[test]
    fn modifiers_add_their_bits() {
        let mut press = report(Button::Left, ReportKind::Press, cell(0, 0));
        press.mods = Mods {
            shift: true,
            alt: true,
            ctrl: true,
        };
        assert_eq!(encode(&press, Encoding::Sgr), b"\x1b[<28;1;1M".to_vec());
        press.mods = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert_eq!(encode(&press, Encoding::Sgr), b"\x1b[<16;1;1M".to_vec());
    }

    #[test]
    fn drag_motion_adds_32() {
        let drag = report(Button::Left, ReportKind::Motion, cell(1, 1));
        assert_eq!(encode(&drag, Encoding::Sgr), b"\x1b[<32;2;2M".to_vec());
        let hover = report(Button::None, ReportKind::Motion, cell(1, 1));
        assert_eq!(encode(&hover, Encoding::Sgr), b"\x1b[<35;2;2M".to_vec());
    }

    #[test]
    fn utf8_mode_encodes_coordinates_as_utf8_characters() {
        let far = report(Button::Left, ReportKind::Press, cell(0, 99));
        assert_eq!(
            encode(&far, Encoding::Utf8),
            vec![0x1b, b'[', b'M', 32, 0xc2, 0x84, 33]
        );
    }

    #[test]
    fn utf8_mode_clamps_at_2015() {
        let beyond = report(Button::Left, ReportKind::Press, cell(5000, 2014));
        let clamped = [0xdf, 0xbf];
        let bytes = encode(&beyond, Encoding::Utf8);
        assert_eq!(&bytes[..4], &[0x1b, b'[', b'M', 32]);
        assert_eq!(&bytes[4..6], &clamped);
        assert_eq!(&bytes[6..], &clamped);
    }

    #[test]
    fn encoding_follows_the_mode() {
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        assert_eq!(Encoding::of(utf8), Encoding::Utf8);
        assert_eq!(Encoding::of(utf8 | TermMode::SGR_MOUSE), Encoding::Sgr);
        assert_eq!(Encoding::of(TermMode::MOUSE_REPORT_CLICK), Encoding::X10);
    }

    #[test]
    fn gesture_is_decided_at_press_and_kept_until_release() {
        let mode = TermMode::MOUSE_DRAG;
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.down(Button::Left, mode, true),
            Some(Gesture::Select)
        );
        assert_eq!(tracker.moving(), Some(Gesture::Select));
        assert_eq!(tracker.up(Button::Left), Some(Gesture::Select));
        assert_eq!(tracker.moving(), None);

        assert_eq!(
            tracker.down(Button::Left, mode, false),
            Some(Gesture::Report)
        );
        assert_eq!(tracker.moving(), Some(Gesture::Report));
        assert_eq!(tracker.up(Button::Left), Some(Gesture::Report));
        assert_eq!(tracker.moving(), None);
    }

    #[test]
    fn each_reported_button_gets_its_own_release() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.down(Button::Left, mode, false),
            Some(Gesture::Report)
        );
        assert_eq!(
            tracker.down(Button::Right, mode, false),
            Some(Gesture::Report)
        );
        assert_eq!(tracker.held(), Some(Button::Left));
        assert_eq!(tracker.up(Button::Left), Some(Gesture::Report));
        assert_eq!(tracker.held(), Some(Button::Right));
        assert_eq!(tracker.up(Button::Middle), None);
        assert_eq!(tracker.up(Button::Right), Some(Gesture::Report));
        assert_eq!(tracker.up(Button::Right), None);
        assert_eq!(tracker.moving(), None);
    }

    #[test]
    fn only_the_left_button_selects() {
        let mut tracker = Tracker::default();
        let mode = TermMode::empty();
        assert_eq!(tracker.down(Button::Right, mode, false), None);
        assert_eq!(tracker.up(Button::Right), None);
        assert_eq!(
            tracker.down(Button::Left, mode, false),
            Some(Gesture::Select)
        );
        assert_eq!(tracker.down(Button::Right, mode, false), None);
        assert_eq!(tracker.up(Button::Right), None);
        assert_eq!(tracker.up(Button::Left), Some(Gesture::Select));
    }

    #[test]
    fn copy_on_select_copies_only_a_non_empty_selection_when_enabled() {
        let text = || Some("hello".to_owned());
        assert_eq!(copy_on_select(true, text()), text());
        assert_eq!(copy_on_select(false, text()), None);
        assert_eq!(copy_on_select(true, Some(String::new())), None);
        assert_eq!(copy_on_select(true, None), None);
    }
}
