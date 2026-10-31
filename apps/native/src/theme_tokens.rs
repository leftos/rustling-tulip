//! The theme editor's tokens as one table: every colour and share the editor
//! offers, its group and label, reading and writing one row's value, parsing
//! what a user types, and re-deriving the tokens that follow the one edited.
//!
//! A row names a [`UiTheme`] field, so the table is the editor's list and the
//! theme's storage seen at once. The tokens that only follow another one -
//! the aliases, the alpha washes and the mixes - have no row; [`recompute`]
//! sets them from the heads they follow.

use alacritty_terminal::vte::ansi::Rgb;

use crate::theme::{TerminalPalette, rgb};
use crate::ui_theme::UiTheme;

/// The share of the accent a recomputed launched tint carries: fixed, since a stored theme holds no selection share to double.
const LAUNCHED_TINT_SHARE: u32 = 36;
/// The share of the danger colour a recomputed `danger_bg` carries.
const DANGER_BG_SHARE: u32 = 18;
/// The alpha of `input_selection`, the accent's wash in a text field.
const INPUT_SELECTION_ALPHA: u32 = 0x4d;
/// The alpha of `ask_wash`, a Dashboard row waiting on the user.
const ASK_WASH_ALPHA: u32 = 0x12;
/// The alpha of `ask_border`, a Dashboard queue chip's edge.
const ASK_BORDER_ALPHA: u32 = 0x59;
/// The alpha of `danger_wash`, a stuck flag's fill.
const DANGER_WASH_ALPHA: u32 = 0x1a;
/// The alpha of `ok_wash`, the disc behind a passed check mark.
const OK_WASH_ALPHA: u32 = 0x29;

/// The heading a row is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenGroup {
    /// The window's grounds and fills.
    Surfaces,
    /// The body, label and quiet text.
    Text,
    /// Dividers and edges.
    Lines,
    /// The session accent, its label, primary buttons and the selected row.
    AccentAndSelection,
    /// The status glyphs and their labels.
    Status,
    /// The source-control letters and the diff washes.
    DiffAndSyntax,
    /// The terminal pane's own colours.
    Terminal,
}

/// How a row's value is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `#rrggbb`.
    Opaque,
    /// `#rrggbbaa`.
    Alpha,
    /// A decimal share from 0.0 to 1.0.
    Share,
}

/// One editable token: the field it reads, the words it is listed under and
/// how its value is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenRow {
    /// The [`UiTheme`] field it reads, which is also its stored name.
    pub name: &'static str,
    /// The words it is listed under.
    pub label: &'static str,
    /// The heading it is listed under.
    pub group: TokenGroup,
    /// How its value is typed.
    pub kind: TokenKind,
}

/// A row's value: a colour as `0xRRGGBB` or `0xRRGGBBAA`, or a share.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenValue {
    /// A colour, with its alpha byte for a row whose kind is
    /// [`TokenKind::Alpha`].
    Color(u32),
    /// A share from 0.0 to 1.0.
    Share(f32),
}

const fn row(
    name: &'static str,
    label: &'static str,
    group: TokenGroup,
    kind: TokenKind,
) -> TokenRow {
    TokenRow {
        name,
        label,
        group,
        kind,
    }
}

/// Every token the editor offers, by group, in the order the groups are
/// listed.
pub const ROWS: &[TokenRow] = &[
    row("ground", "Ground", TokenGroup::Surfaces, TokenKind::Opaque),
    row("sunken", "Sunken", TokenGroup::Surfaces, TokenKind::Opaque),
    row(
        "surface",
        "Surface",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row("raised", "Raised", TokenGroup::Surfaces, TokenKind::Opaque),
    row("chip", "Chip", TokenGroup::Surfaces, TokenKind::Opaque),
    row("hover", "Hover", TokenGroup::Surfaces, TokenKind::Opaque),
    row(
        "pressed",
        "Pressed",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row(
        "row_card",
        "Row card",
        TokenGroup::Surfaces,
        TokenKind::Alpha,
    ),
    row(
        "row_hover",
        "Row hover",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row(
        "popover",
        "Popover",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row(
        "popover_hover",
        "Popover hover",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row(
        "popover_pressed",
        "Popover pressed",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row("modal", "Modal", TokenGroup::Surfaces, TokenKind::Opaque),
    row(
        "modal_inset",
        "Modal inset",
        TokenGroup::Surfaces,
        TokenKind::Opaque,
    ),
    row(
        "popover_shadow",
        "Popover shadow",
        TokenGroup::Surfaces,
        TokenKind::Alpha,
    ),
    row(
        "dialog_shadow",
        "Dialog shadow",
        TokenGroup::Surfaces,
        TokenKind::Alpha,
    ),
    row(
        "toast_shadow",
        "Toast shadow",
        TokenGroup::Surfaces,
        TokenKind::Alpha,
    ),
    row("scrim", "Scrim", TokenGroup::Surfaces, TokenKind::Alpha),
    row(
        "shell_mark_border",
        "Shell mark border",
        TokenGroup::Surfaces,
        TokenKind::Alpha,
    ),
    row("text", "Text", TokenGroup::Text, TokenKind::Opaque),
    row("text_2", "Text 2", TokenGroup::Text, TokenKind::Opaque),
    row("subtle", "Subtle", TokenGroup::Text, TokenKind::Opaque),
    row("faint", "Faint", TokenGroup::Text, TokenKind::Opaque),
    row("line", "Line", TokenGroup::Lines, TokenKind::Opaque),
    row(
        "line_strong",
        "Line strong",
        TokenGroup::Lines,
        TokenKind::Opaque,
    ),
    row(
        "popover_edge",
        "Popover edge",
        TokenGroup::Lines,
        TokenKind::Opaque,
    ),
    row(
        "modal_edge",
        "Modal edge",
        TokenGroup::Lines,
        TokenKind::Opaque,
    ),
    row(
        "menu_rule",
        "Menu rule",
        TokenGroup::Lines,
        TokenKind::Opaque,
    ),
    row(
        "card_edge",
        "Card edge",
        TokenGroup::Lines,
        TokenKind::Opaque,
    ),
    row(
        "accent",
        "Accent",
        TokenGroup::AccentAndSelection,
        TokenKind::Opaque,
    ),
    row(
        "on_accent",
        "On accent",
        TokenGroup::AccentAndSelection,
        TokenKind::Opaque,
    ),
    row(
        "primary",
        "Primary",
        TokenGroup::AccentAndSelection,
        TokenKind::Opaque,
    ),
    row(
        "selection",
        "Selection",
        TokenGroup::AccentAndSelection,
        TokenKind::Opaque,
    ),
    row("working", "Working", TokenGroup::Status, TokenKind::Opaque),
    row("asking", "Asking", TokenGroup::Status, TokenKind::Opaque),
    row(
        "on_asking",
        "On asking",
        TokenGroup::Status,
        TokenKind::Opaque,
    ),
    row("waiting", "Waiting", TokenGroup::Status, TokenKind::Opaque),
    row("danger", "Danger", TokenGroup::Status, TokenKind::Opaque),
    row(
        "danger_text",
        "Danger text",
        TokenGroup::Status,
        TokenKind::Opaque,
    ),
    row(
        "status_ok",
        "Status ok",
        TokenGroup::Status,
        TokenKind::Opaque,
    ),
    row(
        "status_idle",
        "Status idle",
        TokenGroup::Status,
        TokenKind::Opaque,
    ),
    row(
        "lilac",
        "Lilac",
        TokenGroup::DiffAndSyntax,
        TokenKind::Opaque,
    ),
    row(
        "ochre",
        "Ochre",
        TokenGroup::DiffAndSyntax,
        TokenKind::Opaque,
    ),
    row("rose", "Rose", TokenGroup::DiffAndSyntax, TokenKind::Opaque),
    row(
        "periwinkle",
        "Periwinkle",
        TokenGroup::DiffAndSyntax,
        TokenKind::Opaque,
    ),
    row(
        "change_deleted",
        "Change deleted",
        TokenGroup::DiffAndSyntax,
        TokenKind::Opaque,
    ),
    row(
        "diff_delete",
        "Diff delete",
        TokenGroup::DiffAndSyntax,
        TokenKind::Opaque,
    ),
    row(
        "diff_filler",
        "Diff filler",
        TokenGroup::DiffAndSyntax,
        TokenKind::Opaque,
    ),
    row(
        "diff_delete_wash_alpha",
        "Diff delete wash alpha",
        TokenGroup::DiffAndSyntax,
        TokenKind::Share,
    ),
    row(
        "diff_insert_wash_alpha",
        "Diff insert wash alpha",
        TokenGroup::DiffAndSyntax,
        TokenKind::Share,
    ),
    row(
        "diff_filler_alpha",
        "Diff filler alpha",
        TokenGroup::DiffAndSyntax,
        TokenKind::Share,
    ),
    row(
        "diff_word_wash_alpha",
        "Diff word wash alpha",
        TokenGroup::DiffAndSyntax,
        TokenKind::Share,
    ),
    row(
        "terminal_fg",
        "Foreground",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "terminal_bg",
        "Background",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "terminal_caret",
        "Caret",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "terminal_selection",
        "Selection",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "terminal_selection_fg",
        "Selection text",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "terminal_selection_alpha",
        "Selection opacity",
        TokenGroup::Terminal,
        TokenKind::Share,
    ),
    row("ansi_0", "ANSI 0", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_1", "ANSI 1", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_2", "ANSI 2", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_3", "ANSI 3", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_4", "ANSI 4", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_5", "ANSI 5", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_6", "ANSI 6", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_7", "ANSI 7", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_8", "ANSI 8", TokenGroup::Terminal, TokenKind::Opaque),
    row("ansi_9", "ANSI 9", TokenGroup::Terminal, TokenKind::Opaque),
    row(
        "ansi_10",
        "ANSI 10",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "ansi_11",
        "ANSI 11",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "ansi_12",
        "ANSI 12",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "ansi_13",
        "ANSI 13",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "ansi_14",
        "ANSI 14",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
    row(
        "ansi_15",
        "ANSI 15",
        TokenGroup::Terminal,
        TokenKind::Opaque,
    ),
];

/// The value of the row `name` in `t`, or `None` when no row has that name.
#[must_use]
pub fn get(t: &UiTheme, name: &str) -> Option<TokenValue> {
    get_ui(t, name).or_else(|| get_terminal(t, name))
}

/// Sets the row `name` in `t` to `value`, its field being that row's own
/// type; `false` when no row has that name or `value` is not its kind.
pub fn set(t: &mut UiTheme, name: &str, value: TokenValue) -> bool {
    if set_ui(t, name, value) {
        return true;
    }
    set_terminal(t, name, value)
}

/// Re-derives the tokens that follow others in `t` after the row `edited`
/// changed: the aliases and the alpha washes always, and the mixes whose head
/// is `edited`.
pub fn recompute(t: &mut UiTheme, edited: &str) {
    t.panel_bg = t.surface;
    t.bar_bg = t.sunken;
    t.hover_bg = t.hover;
    t.border = t.line;
    t.muted = t.text_2;
    t.warning = t.asking;
    t.selected_bg = t.selection;
    t.input_text = t.text;
    t.input_placeholder = t.subtle;
    t.input_cursor = t.accent;
    t.context_hot = t.danger;
    t.input_selection = wash(t.accent, INPUT_SELECTION_ALPHA);
    t.ask_wash = wash(t.asking, ASK_WASH_ALPHA);
    t.ask_border = wash(t.asking, ASK_BORDER_ALPHA);
    t.danger_wash = wash(t.danger, DANGER_WASH_ALPHA);
    t.ok_wash = wash(t.status_ok, OK_WASH_ALPHA);
    match edited {
        "surface" => {
            t.launched_tint = mix(t.surface, t.accent, LAUNCHED_TINT_SHARE);
            t.danger_bg = mix(t.surface, t.danger, DANGER_BG_SHARE);
        }
        "accent" => t.launched_tint = mix(t.surface, t.accent, LAUNCHED_TINT_SHARE),
        "danger" => {
            t.danger_bg = mix(t.surface, t.danger, DANGER_BG_SHARE);
            t.status_err = t.danger;
        }
        "primary" | "ground" => {
            let (hover, pressed) = primary_mixes(t.primary, t.ground);
            t.primary_hover = hover;
            t.primary_pressed = pressed;
        }
        "waiting" => t.spawn_badge_ok = t.waiting,
        _ => {}
    }
}

/// What a user typed in `row`'s field, as its value: six hex digits for an
/// opaque row, eight for one carrying alpha, a decimal from 0.0 to 1.0 for a
/// share, each with an optional `#` and surrounding space.
#[must_use]
pub fn parse_value(row: &TokenRow, raw: &str) -> Option<TokenValue> {
    let trimmed = raw.trim();
    match row.kind {
        TokenKind::Opaque => parse_hex(trimmed, 6).map(TokenValue::Color),
        TokenKind::Alpha => parse_hex(trimmed, 8).map(TokenValue::Color),
        TokenKind::Share => trimmed
            .parse::<f32>()
            .ok()
            .filter(|share| (0.0..=1.0).contains(share))
            .map(TokenValue::Share),
    }
}

/// `value` as the editor shows it: `#rrggbb` or `#rrggbbaa` for a colour,
/// following the row's kind, and a share's shortest decimal of at least two
/// and at most three places that reads back as the same value.
#[must_use]
pub fn format_value(row: &TokenRow, value: TokenValue) -> String {
    match value {
        TokenValue::Color(color) if row.kind == TokenKind::Alpha => format!("#{color:08x}"),
        TokenValue::Color(color) => format!("#{:06x}", color & 0x00ff_ffff),
        TokenValue::Share(share) => format_share(share),
    }
}

/// `share` to two places, or to three when two do not read back as the same
/// value.
fn format_share(share: f32) -> String {
    for places in 2..=3 {
        let text = format!("{share:.places$}");
        if text.parse::<f32>() == Ok(share) {
            return text;
        }
    }
    format!("{share:.3}")
}

/// `pct` percent of `over` blended over `base`, channel by channel, the
/// generator's integer mix.
fn mix(base: u32, over: u32, pct: u32) -> u32 {
    let channel = |shift: u32| {
        let base = (base >> shift) & 0xff;
        let over = (over >> shift) & 0xff;
        ((base * (100 - pct) + over * pct) / 100) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// `color` at the alpha byte `alpha`, as `0xRRGGBBAA`.
fn wash(color: u32, alpha: u32) -> u32 {
    ((color & 0x00ff_ffff) << 8) | alpha
}

/// The primary button's hover and pressed fills from `primary` on `ground`,
/// as the built-in generator mixes them: hover toward white on a dark theme
/// and toward black on a light one, pressed toward black and toward the
/// ground.
fn primary_mixes(primary: u32, ground: u32) -> (u32, u32) {
    if is_light(ground) {
        (mix(primary, 0, 25), mix(primary, ground, 12))
    } else {
        (mix(primary, 0x00ff_ffff, 55), mix(primary, 0, 12))
    }
}

/// Whether `color` reads as a light colour: its channels weighted as the eye
/// weighs them, half the full range or more.
fn is_light(color: u32) -> bool {
    let [_, r, g, b] = color.to_be_bytes();
    let weighted = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    weighted / 255.0 > 0.5
}

/// `raw` as a colour when it is an optional `#` and exactly `digits` hex
/// digits.
fn parse_hex(raw: &str, digits: usize) -> Option<u32> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != digits || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

fn get_ui(t: &UiTheme, name: &str) -> Option<TokenValue> {
    let color = |value: u32| Some(TokenValue::Color(value));
    match name {
        "ground" => color(t.ground),
        "sunken" => color(t.sunken),
        "surface" => color(t.surface),
        "raised" => color(t.raised),
        "chip" => color(t.chip),
        "hover" => color(t.hover),
        "pressed" => color(t.pressed),
        "row_card" => color(t.row_card),
        "row_hover" => color(t.row_hover),
        "popover" => color(t.popover),
        "popover_hover" => color(t.popover_hover),
        "popover_pressed" => color(t.popover_pressed),
        "modal" => color(t.modal),
        "modal_inset" => color(t.modal_inset),
        "popover_shadow" => color(t.popover_shadow),
        "dialog_shadow" => color(t.dialog_shadow),
        "toast_shadow" => color(t.toast_shadow),
        "scrim" => color(t.scrim),
        "shell_mark_border" => color(t.shell_mark_border),
        "text" => color(t.text),
        "text_2" => color(t.text_2),
        "subtle" => color(t.subtle),
        "faint" => color(t.faint),
        "line" => color(t.line),
        "line_strong" => color(t.line_strong),
        "popover_edge" => color(t.popover_edge),
        "modal_edge" => color(t.modal_edge),
        "menu_rule" => color(t.menu_rule),
        "card_edge" => color(t.card_edge),
        "accent" => color(t.accent),
        "on_accent" => color(t.on_accent),
        "primary" => color(t.primary),
        "selection" => color(t.selection),
        "working" => color(t.working),
        "asking" => color(t.asking),
        "on_asking" => color(t.on_asking),
        "waiting" => color(t.waiting),
        "danger" => color(t.danger),
        "danger_text" => color(t.danger_text),
        "status_ok" => color(t.status_ok),
        "status_idle" => color(t.status_idle),
        "lilac" => color(t.lilac),
        "ochre" => color(t.ochre),
        "rose" => color(t.rose),
        "periwinkle" => color(t.periwinkle),
        "change_deleted" => color(t.change_deleted),
        "diff_delete" => color(t.diff_delete),
        "diff_filler" => color(t.diff_filler),
        "diff_delete_wash_alpha" => Some(TokenValue::Share(t.diff_delete_wash_alpha)),
        "diff_insert_wash_alpha" => Some(TokenValue::Share(t.diff_insert_wash_alpha)),
        "diff_filler_alpha" => Some(TokenValue::Share(t.diff_filler_alpha)),
        "diff_word_wash_alpha" => Some(TokenValue::Share(t.diff_word_wash_alpha)),
        _ => None,
    }
}

fn set_ui(t: &mut UiTheme, name: &str, value: TokenValue) -> bool {
    match (name, value) {
        ("ground", TokenValue::Color(value)) => t.ground = value,
        ("sunken", TokenValue::Color(value)) => t.sunken = value,
        ("surface", TokenValue::Color(value)) => t.surface = value,
        ("raised", TokenValue::Color(value)) => t.raised = value,
        ("chip", TokenValue::Color(value)) => t.chip = value,
        ("hover", TokenValue::Color(value)) => t.hover = value,
        ("pressed", TokenValue::Color(value)) => t.pressed = value,
        ("row_card", TokenValue::Color(value)) => t.row_card = value,
        ("row_hover", TokenValue::Color(value)) => t.row_hover = value,
        ("popover", TokenValue::Color(value)) => t.popover = value,
        ("popover_hover", TokenValue::Color(value)) => t.popover_hover = value,
        ("popover_pressed", TokenValue::Color(value)) => t.popover_pressed = value,
        ("modal", TokenValue::Color(value)) => t.modal = value,
        ("modal_inset", TokenValue::Color(value)) => t.modal_inset = value,
        ("popover_shadow", TokenValue::Color(value)) => t.popover_shadow = value,
        ("dialog_shadow", TokenValue::Color(value)) => t.dialog_shadow = value,
        ("toast_shadow", TokenValue::Color(value)) => t.toast_shadow = value,
        ("scrim", TokenValue::Color(value)) => t.scrim = value,
        ("shell_mark_border", TokenValue::Color(value)) => t.shell_mark_border = value,
        ("text", TokenValue::Color(value)) => t.text = value,
        ("text_2", TokenValue::Color(value)) => t.text_2 = value,
        ("subtle", TokenValue::Color(value)) => t.subtle = value,
        ("faint", TokenValue::Color(value)) => t.faint = value,
        ("line", TokenValue::Color(value)) => t.line = value,
        ("line_strong", TokenValue::Color(value)) => t.line_strong = value,
        ("popover_edge", TokenValue::Color(value)) => t.popover_edge = value,
        ("modal_edge", TokenValue::Color(value)) => t.modal_edge = value,
        ("menu_rule", TokenValue::Color(value)) => t.menu_rule = value,
        ("card_edge", TokenValue::Color(value)) => t.card_edge = value,
        ("accent", TokenValue::Color(value)) => t.accent = value,
        ("on_accent", TokenValue::Color(value)) => t.on_accent = value,
        ("primary", TokenValue::Color(value)) => t.primary = value,
        ("selection", TokenValue::Color(value)) => t.selection = value,
        ("working", TokenValue::Color(value)) => t.working = value,
        ("asking", TokenValue::Color(value)) => t.asking = value,
        ("on_asking", TokenValue::Color(value)) => t.on_asking = value,
        ("waiting", TokenValue::Color(value)) => t.waiting = value,
        ("danger", TokenValue::Color(value)) => t.danger = value,
        ("danger_text", TokenValue::Color(value)) => t.danger_text = value,
        ("status_ok", TokenValue::Color(value)) => t.status_ok = value,
        ("status_idle", TokenValue::Color(value)) => t.status_idle = value,
        ("lilac", TokenValue::Color(value)) => t.lilac = value,
        ("ochre", TokenValue::Color(value)) => t.ochre = value,
        ("rose", TokenValue::Color(value)) => t.rose = value,
        ("periwinkle", TokenValue::Color(value)) => t.periwinkle = value,
        ("change_deleted", TokenValue::Color(value)) => t.change_deleted = value,
        ("diff_delete", TokenValue::Color(value)) => t.diff_delete = value,
        ("diff_filler", TokenValue::Color(value)) => t.diff_filler = value,
        ("diff_delete_wash_alpha", TokenValue::Share(value)) => t.diff_delete_wash_alpha = value,
        ("diff_insert_wash_alpha", TokenValue::Share(value)) => t.diff_insert_wash_alpha = value,
        ("diff_filler_alpha", TokenValue::Share(value)) => t.diff_filler_alpha = value,
        ("diff_word_wash_alpha", TokenValue::Share(value)) => t.diff_word_wash_alpha = value,
        (_, _) => return false,
    }
    true
}

/// `color` as the `0xRRGGBB` a terminal row reads and writes.
fn packed(color: Rgb) -> u32 {
    u32::from(color.r) << 16 | u32::from(color.g) << 8 | u32::from(color.b)
}

/// The terminal colour `name` reads, or `None` when it names no terminal
/// field.
fn get_terminal(t: &UiTheme, name: &str) -> Option<TokenValue> {
    let terminal: &TerminalPalette = &t.terminal;
    if name == "terminal_selection_alpha" {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a share is a fraction the editor holds as an f32"
        )]
        return Some(TokenValue::Share(terminal.selection_alpha as f32));
    }
    let color = match name {
        "terminal_fg" => terminal.fg,
        "terminal_bg" => terminal.bg,
        "terminal_caret" => terminal.caret,
        "terminal_selection" => terminal.selection,
        "terminal_selection_fg" => terminal.selection_fg,
        "ansi_0" => terminal.ansi[0],
        "ansi_1" => terminal.ansi[1],
        "ansi_2" => terminal.ansi[2],
        "ansi_3" => terminal.ansi[3],
        "ansi_4" => terminal.ansi[4],
        "ansi_5" => terminal.ansi[5],
        "ansi_6" => terminal.ansi[6],
        "ansi_7" => terminal.ansi[7],
        "ansi_8" => terminal.ansi[8],
        "ansi_9" => terminal.ansi[9],
        "ansi_10" => terminal.ansi[10],
        "ansi_11" => terminal.ansi[11],
        "ansi_12" => terminal.ansi[12],
        "ansi_13" => terminal.ansi[13],
        "ansi_14" => terminal.ansi[14],
        "ansi_15" => terminal.ansi[15],
        _ => return None,
    };
    Some(TokenValue::Color(packed(color)))
}

/// Sets the terminal field `name` to `value`, `false` when it names no
/// terminal field or `value` is not its kind.
fn set_terminal(t: &mut UiTheme, name: &str, value: TokenValue) -> bool {
    match (name, value) {
        ("terminal_fg", TokenValue::Color(value)) => t.terminal.fg = rgb(value),
        ("terminal_bg", TokenValue::Color(value)) => t.terminal.bg = rgb(value),
        ("terminal_caret", TokenValue::Color(value)) => t.terminal.caret = rgb(value),
        ("terminal_selection", TokenValue::Color(value)) => t.terminal.selection = rgb(value),
        ("terminal_selection_fg", TokenValue::Color(value)) => {
            t.terminal.selection_fg = rgb(value);
        }
        ("terminal_selection_alpha", TokenValue::Share(value)) => {
            t.terminal.selection_alpha = f64::from(value);
        }
        ("ansi_0", TokenValue::Color(value)) => t.terminal.ansi[0] = rgb(value),
        ("ansi_1", TokenValue::Color(value)) => t.terminal.ansi[1] = rgb(value),
        ("ansi_2", TokenValue::Color(value)) => t.terminal.ansi[2] = rgb(value),
        ("ansi_3", TokenValue::Color(value)) => t.terminal.ansi[3] = rgb(value),
        ("ansi_4", TokenValue::Color(value)) => t.terminal.ansi[4] = rgb(value),
        ("ansi_5", TokenValue::Color(value)) => t.terminal.ansi[5] = rgb(value),
        ("ansi_6", TokenValue::Color(value)) => t.terminal.ansi[6] = rgb(value),
        ("ansi_7", TokenValue::Color(value)) => t.terminal.ansi[7] = rgb(value),
        ("ansi_8", TokenValue::Color(value)) => t.terminal.ansi[8] = rgb(value),
        ("ansi_9", TokenValue::Color(value)) => t.terminal.ansi[9] = rgb(value),
        ("ansi_10", TokenValue::Color(value)) => t.terminal.ansi[10] = rgb(value),
        ("ansi_11", TokenValue::Color(value)) => t.terminal.ansi[11] = rgb(value),
        ("ansi_12", TokenValue::Color(value)) => t.terminal.ansi[12] = rgb(value),
        ("ansi_13", TokenValue::Color(value)) => t.terminal.ansi[13] = rgb(value),
        ("ansi_14", TokenValue::Color(value)) => t.terminal.ansi[14] = rgb(value),
        ("ansi_15", TokenValue::Color(value)) => t.terminal.ansi[15] = rgb(value),
        (_, _) => return false,
    }
    true
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::builtin_themes::{BUILT_INS, CATPPUCCIN_LATTE, NORD};

    /// The position of `group` in the order the groups are listed.
    fn rank(group: TokenGroup) -> usize {
        match group {
            TokenGroup::Surfaces => 0,
            TokenGroup::Text => 1,
            TokenGroup::Lines => 2,
            TokenGroup::AccentAndSelection => 3,
            TokenGroup::Status => 4,
            TokenGroup::DiffAndSyntax => 5,
            TokenGroup::Terminal => 6,
        }
    }

    /// The row named `name`.
    fn row(name: &str) -> &'static TokenRow {
        ROWS.iter().find(|row| row.name == name).expect("the row")
    }

    /// The value the row `name` reads in `theme`.
    fn read(theme: &UiTheme, name: &str) -> TokenValue {
        let value = get(theme, name);
        assert!(value.is_some(), "{name} does not read");
        value.expect("asserted above")
    }

    #[test]
    fn rows_are_74_in_seven_groups_with_the_counts_above() {
        assert_eq!(ROWS.len(), 74);
        let count = |group: TokenGroup| ROWS.iter().filter(|row| row.group == group).count();
        assert_eq!(count(TokenGroup::Surfaces), 19);
        assert_eq!(count(TokenGroup::Text), 4);
        assert_eq!(count(TokenGroup::Lines), 6);
        assert_eq!(count(TokenGroup::AccentAndSelection), 4);
        assert_eq!(count(TokenGroup::Status), 8);
        assert_eq!(count(TokenGroup::DiffAndSyntax), 11);
        assert_eq!(count(TokenGroup::Terminal), 22);

        let mut groups: Vec<usize> = ROWS.iter().map(|row| rank(row.group)).collect();
        groups.dedup();
        assert_eq!(
            groups,
            [0, 1, 2, 3, 4, 5, 6],
            "each group is one run, in the listed order"
        );

        let mut names: Vec<&str> = ROWS.iter().map(|row| row.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 74, "no name repeats");
    }

    #[test]
    fn every_row_reads_and_writes_its_field_on_every_built_in() {
        for (id, _, theme) in BUILT_INS {
            for row in ROWS {
                let original = read(theme, row.name);
                let changed = match original {
                    TokenValue::Color(color) => TokenValue::Color(color ^ 0x0001_0001),
                    TokenValue::Share(_) => TokenValue::Share(0.42),
                };
                let mut written = *theme;
                assert!(
                    set(&mut written, row.name, changed),
                    "{id}: {} writes",
                    row.name
                );
                assert_eq!(
                    get(&written, row.name),
                    Some(changed),
                    "{id}: {} reads back",
                    row.name
                );
                for other in ROWS {
                    if other.name == row.name {
                        continue;
                    }
                    assert_eq!(
                        get(&written, other.name),
                        get(theme, other.name),
                        "{id}: writing {} moved {}",
                        row.name,
                        other.name
                    );
                }
            }
        }
        let mut theme = UiTheme::CONTRAST;
        assert!(!set(&mut theme, "not_a_row", TokenValue::Color(0)));
        assert!(
            !set(&mut theme, "text", TokenValue::Share(0.5)),
            "a share on a colour row"
        );
    }

    #[test]
    fn no_row_names_an_alias_or_derived_token() {
        const DERIVED: [&str; 23] = [
            "panel_bg",
            "bar_bg",
            "hover_bg",
            "border",
            "muted",
            "warning",
            "selected_bg",
            "input_text",
            "input_placeholder",
            "input_cursor",
            "context_hot",
            "input_selection",
            "ask_wash",
            "ask_border",
            "danger_wash",
            "ok_wash",
            "launched_tint",
            "danger_bg",
            "primary_hover",
            "primary_pressed",
            "status_err",
            "spawn_badge_ok",
            "transparent",
        ];
        for name in DERIVED {
            assert!(
                ROWS.iter().all(|row| row.name != name),
                "{name} follows another token and has no row"
            );
        }
    }

    #[test]
    fn every_built_in_keeps_its_aliases_and_washes_after_a_recompute() {
        for (id, _, theme) in BUILT_INS {
            let mut recomputed = *theme;
            recompute(&mut recomputed, "line");
            assert_eq!(recomputed, *theme, "{id}");
        }
    }

    #[test]
    fn editing_accent_moves_the_input_cursor_selection_and_launched_tint() {
        let mut theme = NORD;
        theme.accent = 0x00ff_0000;
        recompute(&mut theme, "accent");
        assert_eq!(theme.input_cursor, 0x00ff_0000);
        assert_eq!(theme.input_selection, 0xff00_004d);
        assert_eq!(
            theme.launched_tint,
            mix(NORD.surface, 0x00ff_0000, LAUNCHED_TINT_SHARE)
        );
        assert_ne!(theme.launched_tint, NORD.launched_tint);
        assert_eq!(
            theme.selection, NORD.selection,
            "a row, not a derived token"
        );
    }

    #[test]
    fn editing_danger_moves_status_err_and_editing_line_leaves_it() {
        let mut theme = NORD;
        theme.danger = 0x00ab_cdef;
        recompute(&mut theme, "danger");
        assert_eq!(theme.status_err, 0x00ab_cdef);
        assert_eq!(theme.context_hot, 0x00ab_cdef);
        assert_eq!(theme.danger_wash, 0xabcd_ef1a);
        assert_eq!(
            theme.danger_bg,
            mix(NORD.surface, 0x00ab_cdef, DANGER_BG_SHARE)
        );

        let mut theme = NORD;
        theme.line = 0x00fe_dcba;
        recompute(&mut theme, "line");
        assert_eq!(theme.border, 0x00fe_dcba);
        assert_eq!(theme.status_err, NORD.status_err, "only danger moves it");
    }

    #[test]
    fn mix_matches_the_generator_on_nords_launched_tint() {
        assert_eq!(mix(0x0036_3c48, 0x00b4_8ead, 40), 0x0068_5c70);
    }

    #[test]
    fn the_primary_mixes_match_the_generator_on_nord_and_latte() {
        for theme in [NORD, CATPPUCCIN_LATTE] {
            for edited in ["primary", "ground"] {
                let mut recomputed = theme;
                recompute(&mut recomputed, edited);
                assert_eq!(recomputed.primary_hover, theme.primary_hover, "{edited}");
                assert_eq!(
                    recomputed.primary_pressed, theme.primary_pressed,
                    "{edited}"
                );
            }
        }
    }

    #[test]
    fn parse_value_takes_six_or_eight_digits_by_kind_and_refuses_others() {
        let opaque = row("text");
        let alpha = row("row_card");
        let share = row("diff_filler_alpha");
        assert_eq!(
            parse_value(opaque, "#aabbcc"),
            Some(TokenValue::Color(0x00aa_bbcc))
        );
        assert_eq!(
            parse_value(opaque, " AABBCC "),
            Some(TokenValue::Color(0x00aa_bbcc)),
            "any case, with or without the #"
        );
        assert_eq!(parse_value(opaque, "#abc"), None);
        assert_eq!(parse_value(opaque, "gggggg"), None);
        assert_eq!(
            parse_value(opaque, "#aabbccdd"),
            None,
            "eight digits, an opaque row"
        );
        assert_eq!(
            parse_value(alpha, "#aabbccdd"),
            Some(TokenValue::Color(0xaabb_ccdd))
        );
        assert_eq!(
            parse_value(alpha, "#aabbcc"),
            None,
            "six digits, an alpha row"
        );
        assert_eq!(parse_value(share, "0.13"), Some(TokenValue::Share(0.13)));
        assert_eq!(parse_value(share, "0"), Some(TokenValue::Share(0.0)));
        assert_eq!(parse_value(share, " 1.0 "), Some(TokenValue::Share(1.0)));
        assert_eq!(parse_value(share, "1.5"), None);
        assert_eq!(parse_value(share, "-0.1"), None);
        assert_eq!(parse_value(share, "a share"), None);
    }

    #[test]
    fn format_and_parse_round_trip_on_every_row() {
        for (id, _, theme) in BUILT_INS {
            for row in ROWS {
                let value = read(theme, row.name);
                let text = format_value(row, value);
                assert_eq!(
                    parse_value(row, &text),
                    Some(value),
                    "{id}: {} reads back {text}",
                    row.name
                );
                match row.kind {
                    TokenKind::Opaque => {
                        assert!(
                            text.len() == 7 && text.starts_with('#'),
                            "{id}: {} is {text}",
                            row.name
                        );
                    }
                    TokenKind::Alpha => {
                        assert!(
                            text.len() == 9 && text.starts_with('#'),
                            "{id}: {} is {text}",
                            row.name
                        );
                    }
                    TokenKind::Share => {
                        let places = text
                            .split_once('.')
                            .map_or(0, |(_, fraction)| fraction.len());
                        assert!((2..=3).contains(&places), "{id}: {} is {text}", row.name);
                    }
                }
            }
        }
    }
}
