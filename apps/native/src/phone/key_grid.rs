//! The phone's key grid as plain Rust: the stored keys, the default grid,
//! and the bytes a key's presses send. The Terminal page's bar and the
//! composer's Keys dock draw it, and [`super::key_bar`] holds the sticky
//! Ctrl that a single-stroke key takes.

use gpui::{Keystroke, Modifiers};
use protocol::ShiftEnter;
use serde::{Deserialize, Serialize};

use super::reorder;
use crate::drag::Side;
use crate::keys;
use crate::term_input;

/// One key of the grid: what the bar and the dock draw and what a tap does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridKey {
    /// Unique in the grid; the tail of the bar key's driver id.
    pub id: String,
    /// The name drawn on the key.
    pub label: String,
    pub action: KeyAction,
}

/// The driver id of the bar's pinned Hide button, which no key may take.
const HIDE_ID: &str = "hide";

/// What the key editor's label field says while the draft has no press to
/// name it.
const KEY_LABEL_PLACEHOLDER: &str = "Key label";

/// What a tap on a key does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyAction {
    /// Types its presses in order.
    Send { steps: Vec<Step> },
    /// Sticky Ctrl.
    Ctrl,
    /// Paste from the clipboard.
    Paste,
    /// The Quick replies.
    Quick,
    /// A kind a later release added: drawn dimmed, does nothing.
    #[serde(other)]
    Unknown,
}

/// One press: a key with the modifiers held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    pub key: StepKey,
}

/// The key one press presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKey {
    Char {
        value: char,
    },
    Named {
        name: NamedKey,
    },
    /// A press key a later release added: types nothing.
    #[serde(other)]
    Unknown,
}

/// A key with a name, which sends a fixed xterm sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    Esc,
    Tab,
    Enter,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    /// A named key a later release added: types nothing.
    #[serde(other)]
    Unknown,
}

impl NamedKey {
    /// The name the key draws: its part of a derived label, and its row in
    /// the key picker.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Esc => "Esc",
            Self::Tab => "Tab",
            Self::Enter => "Enter",
            Self::Backspace => "Bksp",
            Self::Up => "↑",
            Self::Down => "↓",
            Self::Left => "←",
            Self::Right => "→",
            Self::Home => "Home",
            Self::End => "End",
            Self::PageUp => "PgUp",
            Self::PageDown => "PgDn",
            Self::F1 => "F1",
            Self::F2 => "F2",
            Self::F3 => "F3",
            Self::F4 => "F4",
            Self::F5 => "F5",
            Self::F6 => "F6",
            Self::F7 => "F7",
            Self::F8 => "F8",
            Self::F9 => "F9",
            Self::F10 => "F10",
            Self::F11 => "F11",
            Self::F12 => "F12",
            Self::Unknown => "",
        }
    }

    /// Every named key the picker offers, in the order it draws them.
    pub(crate) const ALL: [Self; 24] = [
        Self::Esc,
        Self::Tab,
        Self::Enter,
        Self::Backspace,
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Home,
        Self::End,
        Self::PageUp,
        Self::PageDown,
        Self::F1,
        Self::F2,
        Self::F3,
        Self::F4,
        Self::F5,
        Self::F6,
        Self::F7,
        Self::F8,
        Self::F9,
        Self::F10,
        Self::F11,
        Self::F12,
    ];

    /// The tail of the picker row's driver id.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Esc => "esc",
            Self::Tab => "tab",
            Self::Enter => "enter",
            Self::Backspace => "backspace",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Home => "home",
            Self::End => "end",
            Self::PageUp => "pageup",
            Self::PageDown => "pagedown",
            Self::F1 => "f1",
            Self::F2 => "f2",
            Self::F3 => "f3",
            Self::F4 => "f4",
            Self::F5 => "f5",
            Self::F6 => "f6",
            Self::F7 => "f7",
            Self::F8 => "f8",
            Self::F9 => "f9",
            Self::F10 => "f10",
            Self::F11 => "f11",
            Self::F12 => "f12",
            Self::Unknown => "unknown",
        }
    }

    /// The gpui key name its press builds its bytes from.
    fn key_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Esc => "escape",
            Self::Tab => "tab",
            Self::Enter => "enter",
            Self::Backspace => "backspace",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Home => "home",
            Self::End => "end",
            Self::PageUp => "pageup",
            Self::PageDown => "pagedown",
            Self::F1 => "f1",
            Self::F2 => "f2",
            Self::F3 => "f3",
            Self::F4 => "f4",
            Self::F5 => "f5",
            Self::F6 => "f6",
            Self::F7 => "f7",
            Self::F8 => "f8",
            Self::F9 => "f9",
            Self::F10 => "f10",
            Self::F11 => "f11",
            Self::F12 => "f12",
            Self::Unknown => return None,
        })
    }
}

impl StepKey {
    /// Whether the key is one this build does not know.
    fn is_unknown(self) -> bool {
        match self {
            Self::Char { .. } => false,
            Self::Named { name } => name == NamedKey::Unknown,
            Self::Unknown => true,
        }
    }
}

/// What the key editor has built so far: its presses, each still missing
/// its key while the picker has not chosen one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyDraft {
    pub(crate) steps: Vec<DraftStep>,
}

/// One press as the key editor holds it: the modifiers held, and the key
/// while one is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DraftStep {
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) key: Option<StepKey>,
}

impl KeyDraft {
    /// The draft of `key`: its presses, or none for a key that sends none.
    pub(crate) fn of(key: &GridKey) -> Self {
        let steps = match &key.action {
            KeyAction::Send { steps } => steps.iter().map(DraftStep::of).collect(),
            KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick | KeyAction::Unknown => {
                Vec::new()
            }
        };
        Self { steps }
    }

    /// The label the presses that have a key make, as the label field's
    /// placeholder shows it.
    pub(crate) fn derived_label(&self) -> String {
        let steps: Vec<Step> = self.steps.iter().filter_map(|step| step.step()).collect();
        derived_label(&steps)
    }

    /// What the label field says: the presses' label, or what to type while
    /// they name nothing.
    pub(crate) fn label_placeholder(&self) -> String {
        let derived = self.derived_label();
        if derived.is_empty() {
            KEY_LABEL_PLACEHOLDER.to_owned()
        } else {
            derived
        }
    }

    /// Moves the press at `from` to `side` of the one at `to`; an index that
    /// names nothing leaves them as they are.
    pub(crate) fn move_step(&mut self, from: usize, to: usize, side: Side) {
        self.steps = reorder::reordered(&self.steps, from, to, side);
    }
}

impl DraftStep {
    /// The draft press of `step`.
    fn of(step: &Step) -> Self {
        Self {
            ctrl: step.ctrl,
            alt: step.alt,
            shift: step.shift,
            key: Some(step.key),
        }
    }

    /// The press it builds, or `None` while it has no key.
    fn step(self) -> Option<Step> {
        Some(Step {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            key: self.key?,
        })
    }
}

/// Why the key editor refused a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DraftError {
    /// The key has no press at all.
    NoPresses,
    /// The press at this number, counted from 1, has no key.
    NoKey(usize),
    /// The press at this number types nothing, as this label shows.
    TypesNothing(usize, String),
}

impl DraftError {
    /// The message the editor shows.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::NoPresses => "Add at least one press.".to_owned(),
            Self::NoKey(press) => format!("Press {press} has no key."),
            Self::TypesNothing(press, label) => {
                format!("Press {press} ({label}) types nothing in a terminal.")
            }
        }
    }
}

/// The modes a draft's presses are checked in: what the surface that sends
/// them reads is decided at send time, so only "types nothing" is asked.
const CHECK_MODES: KeyModes = KeyModes {
    app_cursor: false,
    shift_enter: ShiftEnter::BackslashCr,
};

/// The label and presses a draft saves as, or why it cannot: a label that
/// is blank takes the label its presses make (P18), and a press that types
/// nothing is refused (P4).
pub(crate) fn check_draft(
    label: &str,
    draft: &KeyDraft,
) -> Result<(String, Vec<Step>), DraftError> {
    if draft.steps.is_empty() {
        return Err(DraftError::NoPresses);
    }
    let mut steps = Vec::with_capacity(draft.steps.len());
    for (index, draft_step) in draft.steps.iter().enumerate() {
        let Some(step) = draft_step.step() else {
            return Err(DraftError::NoKey(index + 1));
        };
        steps.push(step);
    }
    for (index, step) in steps.iter().enumerate() {
        if step_bytes(step, CHECK_MODES).is_none() {
            let label = derived_label(std::slice::from_ref(step));
            return Err(DraftError::TypesNothing(index + 1, label));
        }
    }
    let typed = label.trim();
    let label = if typed.is_empty() {
        derived_label(&steps)
    } else {
        typed.to_owned()
    };
    Ok((label, steps))
}

/// One change the key editor and the Keys page make to the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyEdit {
    /// Appends a key.
    Add(GridKey),
    /// Replaces the key at this index, keeping its place.
    Replace(usize, GridKey),
    /// Drops the key at this index.
    Remove(usize),
    /// Moves the key at this index to `side` of the one at `to`.
    Move { from: usize, to: usize, side: Side },
}

/// The grid with `edit` made; `None`, changing nothing, while an index
/// names nothing or a protected key would go.
pub(crate) fn apply_key_edit(keys: &[GridKey], edit: KeyEdit) -> Option<Vec<GridKey>> {
    let mut keys = keys.to_vec();
    match edit {
        KeyEdit::Add(key) => keys.push(key),
        KeyEdit::Replace(index, key) => *keys.get_mut(index)? = key,
        KeyEdit::Remove(index) => {
            if is_protected(keys.get(index)?) {
                return None;
            }
            keys.remove(index);
        }
        KeyEdit::Move { from, to, side } => {
            if from >= keys.len() || to >= keys.len() {
                return None;
            }
            return Some(reorder::reordered(&keys, from, to, side));
        }
    }
    Some(keys)
}

/// The name a press's key button draws: the character itself (its upper
/// case while Shift is held), a named key's label, or nothing while it is
/// a key this build does not know.
pub(crate) fn press_key_label(key: StepKey, shift: bool) -> String {
    match key {
        StepKey::Char { value: ' ' } => "Space".to_owned(),
        StepKey::Char { value } => match shift.then(|| upper_char(value)).flatten() {
            Some(upper) => upper.to_string(),
            None => value.to_string(),
        },
        StepKey::Named { name } => name.label().to_owned(),
        StepKey::Unknown => String::new(),
    }
}

/// What the terminal's modes say about the bytes a press sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyModes {
    /// The terminal's DECCKM mode: arrows send `ESC O x` instead of `ESC [ x`.
    pub(crate) app_cursor: bool,
    /// What Shift+Enter sends: the session's own choice, as its agent's
    /// descriptor names it.
    pub(crate) shift_enter: ShiftEnter,
}

/// The grid a phone with nothing saved draws: the Terminal bar's keys in
/// their order, then the Keys tray's extras.
pub(crate) fn defaults() -> Vec<GridKey> {
    let mut keys = vec![
        named("esc", "Esc", NamedKey::Esc),
        named("tab", "Tab", NamedKey::Tab),
        GridKey {
            id: "ctrl".to_owned(),
            label: "Ctrl".to_owned(),
            action: KeyAction::Ctrl,
        },
        named("up", "↑", NamedKey::Up),
        named("down", "↓", NamedKey::Down),
        named("left", "←", NamedKey::Left),
        named("right", "→", NamedKey::Right),
        GridKey {
            id: "paste".to_owned(),
            label: "Paste".to_owned(),
            action: KeyAction::Paste,
        },
        GridKey {
            id: "quick".to_owned(),
            label: "Quick".to_owned(),
            action: KeyAction::Quick,
        },
        send(
            "shift-tab",
            "Shift+Tab",
            vec![Step {
                ctrl: false,
                alt: false,
                shift: true,
                key: StepKey::Named {
                    name: NamedKey::Tab,
                },
            }],
        ),
        named("enter", "Enter", NamedKey::Enter),
        ctrl_key("ctrl-c", "Ctrl+C", 'c'),
    ];
    for digit in '1'..='9' {
        let label = digit.to_string();
        keys.push(send(
            &label,
            &label,
            vec![plain(StepKey::Char { value: digit })],
        ));
    }
    keys
}

/// A key of one plain press of a named key.
fn named(id: &str, label: &str, name: NamedKey) -> GridKey {
    send(id, label, vec![plain(StepKey::Named { name })])
}

/// A key of one press of the character `value` with Ctrl held.
fn ctrl_key(id: &str, label: &str, value: char) -> GridKey {
    let mut step = plain(StepKey::Char { value });
    step.ctrl = true;
    send(id, label, vec![step])
}

/// A send key of `steps`.
fn send(id: &str, label: &str, steps: Vec<Step>) -> GridKey {
    GridKey {
        id: id.to_owned(),
        label: label.to_owned(),
        action: KeyAction::Send { steps },
    }
}

/// A press of `key` with no modifier held.
fn plain(key: StepKey) -> Step {
    Step {
        ctrl: false,
        alt: false,
        shift: false,
        key,
    }
}

/// The bytes `step` sends, or `None` when it types nothing.
pub(crate) fn step_bytes(step: &Step, modes: KeyModes) -> Option<Vec<u8>> {
    match step.key {
        StepKey::Char { value } => char_bytes(step, value),
        StepKey::Named { name } => name_bytes(step, name, modes),
        StepKey::Unknown => None,
    }
}

/// A character press's bytes: the character itself, its upper case with
/// Shift, the control byte of its Ctrl keystroke (Shift ignored), and an
/// `ESC` prefix with Alt.
fn char_bytes(step: &Step, value: char) -> Option<Vec<u8>> {
    let mut bytes = if step.ctrl {
        let key = if value == ' ' {
            "space".to_owned()
        } else {
            value.to_string()
        };
        let keystroke = Keystroke {
            key,
            key_char: None,
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
        };
        keys::to_bytes(&keystroke, false)?
    } else if step.shift {
        upper_case(value)?
    } else {
        value.to_string().into_bytes()
    };
    if step.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// `value`'s upper case, or `None` when it has none (a digit, a symbol, a
/// character whose upper case is more than one character).
fn upper_char(value: char) -> Option<char> {
    if !value.is_lowercase() && !value.is_uppercase() {
        return None;
    }
    let mut upper = value.to_uppercase();
    let first = upper.next()?;
    upper.next().is_none().then_some(first)
}

/// `value`'s upper case as its bytes, or `None` when it has none.
fn upper_case(value: char) -> Option<Vec<u8>> {
    Some(upper_char(value)?.to_string().into_bytes())
}

/// A named press's bytes, as `keys::to_bytes` gives its keystroke, except
/// that Shift+Enter sends the session's own Shift+Enter bytes, as the pane's
/// keyboard does (`term_input::key_action`).
fn name_bytes(step: &Step, name: NamedKey, modes: KeyModes) -> Option<Vec<u8>> {
    if name == NamedKey::Enter && step.shift && !step.ctrl {
        return Some(term_input::shift_enter_bytes(modes.shift_enter).to_vec());
    }
    let keystroke = Keystroke {
        key: name.key_name()?.to_owned(),
        key_char: None,
        modifiers: Modifiers {
            control: step.ctrl,
            alt: step.alt,
            shift: step.shift,
            ..Modifiers::default()
        },
    };
    keys::to_bytes(&keystroke, modes.app_cursor)
}

/// The bytes `steps` send in order, or `None` when one of them types
/// nothing. Armed Ctrl joins a key of one plain press.
pub(crate) fn key_bytes(steps: &[Step], sticky_ctrl: bool, modes: KeyModes) -> Option<Vec<u8>> {
    if steps.is_empty() {
        return None;
    }
    let sticky = sticky_ctrl && single_plain_press(steps);
    let mut bytes = Vec::new();
    for step in steps {
        let mut step = *step;
        if sticky {
            step.ctrl = true;
        }
        bytes.extend(step_bytes(&step, modes)?);
    }
    Some(bytes)
}

/// Whether armed Ctrl applies to `key`: a send key of one press with no
/// modifier of its own.
pub(crate) fn takes_sticky_ctrl(key: &GridKey) -> bool {
    match &key.action {
        KeyAction::Send { steps } => single_plain_press(steps),
        KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick | KeyAction::Unknown => false,
    }
}

/// Whether `steps` is one press of a key this build knows with no modifier
/// held.
fn single_plain_press(steps: &[Step]) -> bool {
    match steps {
        [step] => !step.ctrl && !step.alt && !step.shift && !step.key.is_unknown(),
        _ => false,
    }
}

/// Whether `key` cannot be removed: Ctrl, Paste or Quick.
pub(crate) fn is_protected(key: &GridKey) -> bool {
    matches!(
        key.action,
        KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick
    )
}

/// Whether `key` draws dimmed and does nothing: an action or a press key a
/// later release added.
pub(crate) fn is_inert(key: &GridKey) -> bool {
    match &key.action {
        KeyAction::Send { steps } => steps.iter().any(|step| step.key.is_unknown()),
        KeyAction::Unknown => true,
        KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick => false,
    }
}

/// The grid to draw: the saved one, or the built-in grid while nothing is
/// saved.
pub(crate) fn resolved(saved: Option<&[GridKey]>) -> Vec<GridKey> {
    match saved {
        Some(keys) => keys.to_vec(),
        None => defaults(),
    }
}

/// What a loaded grid reads as (P22): Ctrl, Paste and Quick each exactly
/// once (a missing one appended at the end, a repeat dropped), an id that is
/// unfit or repeats an earlier one replaced by the next `key-{n}`, and a
/// blank label on a send key replaced by its derived label.
pub(crate) fn normalized(keys: Vec<GridKey>) -> Vec<GridKey> {
    let fixed = defaults();
    let reserved: Vec<String> = fixed
        .iter()
        .filter(|key| is_protected(key) && !keys.iter().any(|saved| saved.action == key.action))
        .map(|key| key.id.clone())
        .collect();
    let mut kept: Vec<GridKey> = Vec::with_capacity(keys.len() + 3);
    for mut key in keys {
        if is_protected(&key) && kept.iter().any(|early| early.action == key.action) {
            continue;
        }
        fit_id(&mut key, &kept, &reserved);
        fit_label(&mut key);
        kept.push(key);
    }
    for key in fixed.into_iter().filter(is_protected) {
        if !kept.iter().any(|early| early.action == key.action) {
            kept.push(key);
        }
    }
    kept
}

/// Gives `key` a fresh `key-{n}` id when its own is unfit, repeats an
/// earlier key's, or is one the appended protected keys will take.
fn fit_id(key: &mut GridKey, kept: &[GridKey], reserved: &[String]) {
    let repeats = kept.iter().any(|early| early.id == key.id);
    if fits_id(&key.id) && !repeats && !reserved.contains(&key.id) {
        return;
    }
    key.id = next_id(kept);
}

/// Whether `id` is lower-case letters, digits and `-`, not empty, and not
/// the pinned Hide button's.
fn fits_id(id: &str) -> bool {
    id != HIDE_ID
        && !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Makes a send key's blank label its presses' derived one.
fn fit_label(key: &mut GridKey) {
    let KeyAction::Send { steps } = &key.action else {
        return;
    };
    if key.label.trim().is_empty() {
        key.label = derived_label(steps);
    }
}

/// The id an added key takes: one past the highest `key-{n}` in use, in a
/// width that cannot overflow on any stored number.
pub(crate) fn next_id(keys: &[GridKey]) -> String {
    let highest = keys
        .iter()
        .filter_map(|key| key.id.strip_prefix("key-"))
        .filter_map(|number| number.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("key-{}", highest + 1)
}

/// The label a key's presses make (P18): each press's modifiers in Ctrl,
/// Alt, Shift order and then the key, the presses joined by spaces.
pub(crate) fn derived_label(steps: &[Step]) -> String {
    steps
        .iter()
        .map(step_label)
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One press's part of the derived label, empty for a press key this build
/// does not know.
fn step_label(step: &Step) -> String {
    if step.key.is_unknown() {
        return String::new();
    }
    let mut label = String::new();
    for (held, modifier) in [
        (step.ctrl, "Ctrl+"),
        (step.alt, "Alt+"),
        (step.shift, "Shift+"),
    ] {
        if held {
            label.push_str(modifier);
        }
    }
    match step.key {
        StepKey::Char { value } => label.push_str(&char_label(value, step)),
        StepKey::Named { name } => label.push_str(name.label()),
        StepKey::Unknown => {}
    }
    label
}

/// The character a press draws: `Space` for a space, the upper case of a
/// character with a modifier held (one with no case keeps itself), and the
/// character itself otherwise.
fn char_label(value: char, step: &Step) -> String {
    if value == ' ' {
        return "Space".to_owned();
    }
    let modified = step.ctrl || step.alt || step.shift;
    if modified && let Some(upper) = upper_char(value) {
        return upper.to_string();
    }
    value.to_string()
}

/// How many equal columns of at least `min_cell` pt, `gap` apart, fit in
/// `width`, and at least one however narrow the width.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the count is clamped to at least one first, and a phone's width in points is far below what a usize holds"
)]
pub(crate) fn grid_columns(width: f32, min_cell: f32, gap: f32) -> usize {
    ((width + gap) / (min_cell + gap)).max(1.0) as usize
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a named test fixture that is missing should fail with its message"
)]
mod tests {
    use super::*;

    /// The step a plain press of `key` is.
    fn plain_step(key: StepKey) -> Step {
        Step {
            ctrl: false,
            alt: false,
            shift: false,
            key,
        }
    }

    /// The step a press of `key` with the three modifiers held is.
    fn held_step(ctrl: bool, alt: bool, shift: bool, key: StepKey) -> Step {
        Step {
            ctrl,
            alt,
            shift,
            key,
        }
    }

    fn named_step(name: NamedKey) -> StepKey {
        StepKey::Named { name }
    }

    fn char_step(value: char) -> StepKey {
        StepKey::Char { value }
    }

    fn modes(app_cursor: bool) -> KeyModes {
        KeyModes {
            app_cursor,
            shift_enter: ShiftEnter::BackslashCr,
        }
    }

    /// Modes of a session whose agent takes a line feed for Shift+Enter.
    fn lf_modes() -> KeyModes {
        KeyModes {
            app_cursor: false,
            shift_enter: ShiftEnter::Lf,
        }
    }

    fn off() -> KeyModes {
        modes(false)
    }

    /// `pairs` as owned rows, for comparing against decoded keys.
    fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, label)| ((*id).to_owned(), (*label).to_owned()))
            .collect()
    }

    /// The default grid's send keys' bytes, for comparing against `pairs`.
    fn sent() -> Vec<(String, Option<Vec<u8>>)> {
        defaults()
            .into_iter()
            .filter_map(|key| match key.action {
                KeyAction::Send { steps } => Some((key.id, key_bytes(&steps, false, off()))),
                KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick | KeyAction::Unknown => None,
            })
            .collect()
    }

    /// `pairs` as the rows [`sent`] gives, for comparing against it.
    fn sends(pairs: &[(&str, &[u8])]) -> Vec<(String, Option<Vec<u8>>)> {
        pairs
            .iter()
            .map(|(id, bytes)| ((*id).to_owned(), Some((*bytes).to_vec())))
            .collect()
    }

    /// The default grid's key with `id`.
    fn key_of(id: &str) -> GridKey {
        defaults()
            .into_iter()
            .find(|key| key.id == id)
            .expect("the default grid holds the key")
    }

    /// A grid key of one press.
    fn key_of_step(id: &str, step: Step) -> GridKey {
        GridKey {
            id: id.to_owned(),
            label: id.to_owned(),
            action: KeyAction::Send { steps: vec![step] },
        }
    }

    /// A grid key whose action this build does not know.
    fn unknown_key() -> GridKey {
        GridKey {
            id: "macro".to_owned(),
            label: "Macro".to_owned(),
            action: KeyAction::Unknown,
        }
    }

    #[test]
    fn grid_columns_at_390_points() {
        assert_eq!(grid_columns(358.0, 72.0, 8.0), 4);
    }

    #[test]
    fn grid_columns_narrower() {
        assert_eq!(grid_columns(300.0, 72.0, 8.0), 3);
    }

    #[test]
    fn grid_columns_never_below_one() {
        assert_eq!(grid_columns(60.0, 72.0, 8.0), 1);
        assert_eq!(grid_columns(0.0, 72.0, 8.0), 1);
    }

    #[test]
    fn the_default_grid_is_the_bar_then_the_trays_extras() {
        let keys: Vec<(String, String)> = defaults()
            .into_iter()
            .map(|key| (key.id, key.label))
            .collect();
        assert_eq!(
            keys,
            rows(&[
                ("esc", "Esc"),
                ("tab", "Tab"),
                ("ctrl", "Ctrl"),
                ("up", "↑"),
                ("down", "↓"),
                ("left", "←"),
                ("right", "→"),
                ("paste", "Paste"),
                ("quick", "Quick"),
                ("shift-tab", "Shift+Tab"),
                ("enter", "Enter"),
                ("ctrl-c", "Ctrl+C"),
                ("1", "1"),
                ("2", "2"),
                ("3", "3"),
                ("4", "4"),
                ("5", "5"),
                ("6", "6"),
                ("7", "7"),
                ("8", "8"),
                ("9", "9"),
            ])
        );
    }

    #[test]
    fn the_default_grid_sends_the_old_tray_bytes() {
        assert_eq!(
            sent(),
            sends(&[
                ("esc", b"\x1b"),
                ("tab", b"\t"),
                ("up", b"\x1b[A"),
                ("down", b"\x1b[B"),
                ("left", b"\x1b[D"),
                ("right", b"\x1b[C"),
                ("shift-tab", b"\x1b[Z"),
                ("enter", b"\r"),
                ("ctrl-c", b"\x03"),
                ("1", b"1"),
                ("2", b"2"),
                ("3", b"3"),
                ("4", b"4"),
                ("5", b"5"),
                ("6", b"6"),
                ("7", b"7"),
                ("8", b"8"),
                ("9", b"9"),
            ])
        );
    }

    #[test]
    fn step_bytes_named_keys() {
        for (name, bytes) in [
            (NamedKey::Esc, &b"\x1b"[..]),
            (NamedKey::Tab, b"\t"),
            (NamedKey::Enter, b"\r"),
            (NamedKey::Backspace, b"\x7f"),
        ] {
            assert_eq!(
                step_bytes(&plain_step(named_step(name)), off()).as_deref(),
                Some(bytes),
                "{name:?}"
            );
        }
    }

    #[test]
    fn step_bytes_ctrl_tab_is_tab() {
        assert_eq!(
            step_bytes(
                &held_step(true, false, false, named_step(NamedKey::Tab)),
                off()
            )
            .as_deref(),
            Some(&b"\t"[..]),
            "to_bytes leaves Tab plain under Ctrl"
        );
    }

    #[test]
    fn step_bytes_shift_tab() {
        assert_eq!(
            step_bytes(
                &held_step(false, false, true, named_step(NamedKey::Tab)),
                off()
            )
            .as_deref(),
            Some(&b"\x1b[Z"[..])
        );
    }

    #[test]
    fn step_bytes_ctrl_backspace() {
        assert_eq!(
            step_bytes(
                &held_step(true, false, false, named_step(NamedKey::Backspace)),
                off()
            )
            .as_deref(),
            Some(&b"\x08"[..])
        );
    }

    #[test]
    fn step_bytes_up_honours_application_cursor_mode() {
        let up = plain_step(named_step(NamedKey::Up));
        assert_eq!(step_bytes(&up, off()).as_deref(), Some(&b"\x1b[A"[..]));
        assert_eq!(
            step_bytes(&up, modes(true)).as_deref(),
            Some(&b"\x1bOA"[..]),
            "DECCKM sends SS3"
        );
    }

    #[test]
    fn step_bytes_modified_arrow_is_always_csi() {
        let step = held_step(true, false, false, named_step(NamedKey::Up));
        for app_cursor in [false, true] {
            assert_eq!(
                step_bytes(&step, modes(app_cursor)).as_deref(),
                Some(&b"\x1b[1;5A"[..]),
                "app_cursor {app_cursor}"
            );
        }
    }

    #[test]
    fn step_bytes_alt_left() {
        assert_eq!(
            step_bytes(
                &held_step(false, true, false, named_step(NamedKey::Left)),
                off()
            )
            .as_deref(),
            Some(&b"\x1b[1;3D"[..])
        );
    }

    #[test]
    fn step_bytes_navigation_and_function_keys() {
        for (name, bytes) in [
            (NamedKey::Home, &b"\x1b[H"[..]),
            (NamedKey::PageUp, b"\x1b[5~"),
            (NamedKey::F1, b"\x1bOP"),
            (NamedKey::F5, b"\x1b[15~"),
        ] {
            assert_eq!(
                step_bytes(&plain_step(named_step(name)), off()).as_deref(),
                Some(bytes),
                "{name:?}"
            );
        }
    }

    #[test]
    fn step_bytes_character_and_its_upper_case() {
        assert_eq!(
            step_bytes(&plain_step(char_step('b')), off()).as_deref(),
            Some(&b"b"[..])
        );
        assert_eq!(
            step_bytes(&held_step(false, false, true, char_step('b')), off()).as_deref(),
            Some(&b"B"[..])
        );
    }

    #[test]
    fn step_bytes_ctrl_letters_are_control_bytes() {
        for value in ['b', 'c', 'v'] {
            let expected = match value {
                'b' => &b"\x02"[..],
                'c' => b"\x03",
                _ => b"\x16",
            };
            assert_eq!(
                step_bytes(&held_step(true, false, false, char_step(value)), off()).as_deref(),
                Some(expected),
                "Ctrl+{value}"
            );
        }
        assert_eq!(
            step_bytes(&held_step(true, false, true, char_step('c')), off()).as_deref(),
            Some(&b"\x03"[..]),
            "Shift is ignored under Ctrl"
        );
    }

    #[test]
    fn step_bytes_ctrl_space_is_nul() {
        for value in [' ', '2'] {
            assert_eq!(
                step_bytes(&held_step(true, false, false, char_step(value)), off()).as_deref(),
                Some(&b"\x00"[..]),
                "Ctrl+{value:?}"
            );
        }
    }

    #[test]
    fn step_bytes_alt_prefixes_escape() {
        assert_eq!(
            step_bytes(&held_step(false, true, false, char_step('b')), off()).as_deref(),
            Some(&b"\x1bb"[..])
        );
        assert_eq!(
            step_bytes(&held_step(true, true, false, char_step('b')), off()).as_deref(),
            Some(&b"\x1b\x02"[..]),
            "Ctrl+Alt+B is ESC then the control byte"
        );
    }

    #[test]
    fn step_bytes_a_press_that_types_nothing() {
        assert_eq!(
            step_bytes(&held_step(true, false, false, char_step('1')), off()),
            None,
            "Ctrl+1 has no control byte"
        );
        assert_eq!(
            step_bytes(&held_step(false, false, true, char_step('1')), off()),
            None,
            "1 has no upper case"
        );
    }

    #[test]
    fn step_bytes_utf8_character() {
        assert_eq!(
            step_bytes(&plain_step(char_step('é')), off()).as_deref(),
            Some(&b"\xc3\xa9"[..])
        );
    }

    #[test]
    fn step_bytes_unknown_key_types_nothing() {
        assert_eq!(step_bytes(&plain_step(StepKey::Unknown), off()), None);
        assert_eq!(
            step_bytes(&plain_step(named_step(NamedKey::Unknown)), off()),
            None
        );
    }

    #[test]
    fn shift_enter_sends_backslash_return_by_default() {
        let step = held_step(false, false, true, named_step(NamedKey::Enter));
        assert_eq!(step_bytes(&step, off()).as_deref(), Some(&b"\\\r"[..]));
    }

    #[test]
    fn shift_enter_sends_line_feed_for_an_lf_session() {
        let step = held_step(false, false, true, named_step(NamedKey::Enter));
        assert_eq!(step_bytes(&step, lf_modes()).as_deref(), Some(&b"\n"[..]));
    }

    #[test]
    fn shift_enter_with_ctrl_is_a_plain_return() {
        let step = held_step(true, false, true, named_step(NamedKey::Enter));
        assert_eq!(step_bytes(&step, lf_modes()).as_deref(), Some(&b"\r"[..]));
    }

    #[test]
    fn key_bytes_sends_each_press_in_order() {
        let steps = [
            plain_step(named_step(NamedKey::Esc)),
            plain_step(char_step(':')),
            plain_step(char_step('w')),
            plain_step(named_step(NamedKey::Enter)),
        ];
        assert_eq!(
            key_bytes(&steps, false, off()).as_deref(),
            Some(&b"\x1b:w\r"[..])
        );
    }

    #[test]
    fn key_bytes_one_silent_press_silences_the_key() {
        let steps = [
            plain_step(named_step(NamedKey::Esc)),
            plain_step(StepKey::Unknown),
        ];
        assert_eq!(key_bytes(&steps, false, off()), None);
    }

    #[test]
    fn key_bytes_no_presses_types_nothing() {
        assert_eq!(key_bytes(&[], false, off()), None);
    }

    #[test]
    fn key_bytes_sticky_ctrl_joins_a_single_press() {
        let steps = [plain_step(named_step(NamedKey::Up))];
        assert_eq!(
            key_bytes(&steps, true, off()).as_deref(),
            Some(&b"\x1b[1;5A"[..])
        );
    }

    #[test]
    fn key_bytes_sticky_ctrl_on_a_letter_is_its_control_byte() {
        let steps = [plain_step(char_step('c'))];
        assert_eq!(
            key_bytes(&steps, true, off()).as_deref(),
            Some(&b"\x03"[..])
        );
    }

    #[test]
    fn key_bytes_sticky_ctrl_on_esc_is_esc() {
        let steps = [plain_step(named_step(NamedKey::Esc))];
        assert_eq!(
            key_bytes(&steps, true, off()).as_deref(),
            Some(&b"\x1b"[..])
        );
    }

    #[test]
    fn a_single_plain_press_takes_sticky_ctrl() {
        assert!(takes_sticky_ctrl(&key_of("esc")), "Esc");
        assert!(
            takes_sticky_ctrl(&key_of_step("one", plain_step(char_step('1')))),
            "a character alone"
        );
    }

    #[test]
    fn a_chord_or_sequence_ignores_sticky_ctrl() {
        assert!(!takes_sticky_ctrl(&key_of("shift-tab")), "Shift+Tab");
        assert!(!takes_sticky_ctrl(&key_of("ctrl-c")), "Ctrl+C");
        let sequence = GridKey {
            id: "seq".to_owned(),
            label: "Esc Esc".to_owned(),
            action: KeyAction::Send {
                steps: vec![
                    plain_step(named_step(NamedKey::Esc)),
                    plain_step(named_step(NamedKey::Esc)),
                ],
            },
        };
        assert!(!takes_sticky_ctrl(&sequence), "a sequence");
    }

    #[test]
    fn action_keys_ignore_sticky_ctrl() {
        for id in ["ctrl", "paste", "quick"] {
            assert!(!takes_sticky_ctrl(&key_of(id)), "{id}");
        }
        assert!(!takes_sticky_ctrl(&unknown_key()), "an Unknown action");
    }

    #[test]
    fn ctrl_paste_and_quick_are_protected() {
        for id in ["ctrl", "paste", "quick"] {
            assert!(is_protected(&key_of(id)), "{id}");
        }
        assert!(!is_protected(&key_of("esc")), "a send key");
        assert!(!is_protected(&unknown_key()), "an Unknown action");
    }

    #[test]
    fn unknown_keys_are_inert() {
        assert!(is_inert(&unknown_key()), "an Unknown action");
        let unknown_press = key_of_step("press", held_step(false, false, false, StepKey::Unknown));
        assert!(is_inert(&unknown_press), "a press with an Unknown key");
        assert!(!is_inert(&key_of("esc")), "Esc");
    }

    /// A key with `id` and the same label, whose action is one this build
    /// does not know.
    fn id_key(id: &str) -> GridKey {
        GridKey {
            id: id.to_owned(),
            label: id.to_owned(),
            action: KeyAction::Unknown,
        }
    }

    #[test]
    fn resolved_absent_is_the_defaults() {
        assert_eq!(resolved(None), defaults());
    }

    #[test]
    fn resolved_saved_is_the_saved_grid() {
        let saved = vec![key_of("enter"), unknown_key()];
        assert_eq!(resolved(Some(&saved)), saved);
    }

    #[test]
    fn normalized_keeps_the_defaults() {
        assert_eq!(normalized(defaults()), defaults());
    }

    #[test]
    fn normalized_empty_grid_gets_the_protected_keys() {
        assert_eq!(
            normalized(Vec::new()),
            vec![key_of("ctrl"), key_of("paste"), key_of("quick")]
        );
    }

    #[test]
    fn normalized_appends_a_missing_protected_key() {
        let mut saved = defaults();
        saved.retain(|key| key.id != "quick");
        let mut expected = defaults();
        expected.retain(|key| key.id != "quick");
        expected.push(key_of("quick"));

        assert_eq!(normalized(saved), expected);
    }

    #[test]
    fn normalized_drops_a_repeated_protected_key() {
        let paste = key_of("paste");
        let mut again = paste.clone();
        again.id = "paste-2".to_owned();

        assert_eq!(
            normalized(vec![paste, key_of("esc"), again]),
            vec![
                key_of("paste"),
                key_of("esc"),
                key_of("ctrl"),
                key_of("quick")
            ]
        );
    }

    #[test]
    fn normalized_renames_a_repeated_id() {
        let esc = key_of("esc");
        let mut again = esc.clone();
        again.label = "Another Esc".to_owned();

        let keys = normalized(vec![esc, again]);

        assert_eq!(keys[0].id, "esc");
        assert_eq!(keys[1].id, "key-1");
    }

    #[test]
    fn normalized_replaces_an_unfit_id() {
        let bad = GridKey {
            id: "Bad Id".to_owned(),
            label: "Bad".to_owned(),
            action: KeyAction::Unknown,
        };

        assert_eq!(normalized(vec![bad])[0].id, "key-1");
    }

    #[test]
    fn normalized_renames_a_key_holding_a_missing_protected_id() {
        let p = GridKey {
            id: "paste".to_owned(),
            label: "P".to_owned(),
            action: KeyAction::Send {
                steps: vec![plain_step(char_step('p'))],
            },
        };

        let keys = normalized(vec![p]);

        let ids: Vec<&str> = keys.iter().map(|key| key.id.as_str()).collect();
        assert_eq!(ids, ["key-1", "ctrl", "paste", "quick"]);
        assert!(
            keys.iter()
                .any(|key| key.id == "paste" && key.action == KeyAction::Paste),
            "the appended Paste keeps its fixed id"
        );
    }

    #[test]
    fn normalized_renames_a_key_with_the_hide_id() {
        let hide = GridKey {
            id: "hide".to_owned(),
            label: "Hide".to_owned(),
            action: KeyAction::Send {
                steps: vec![plain_step(named_step(NamedKey::Esc))],
            },
        };

        assert_eq!(normalized(vec![hide])[0].id, "key-1");
    }

    #[test]
    fn next_id_past_the_largest_u32() {
        let mut keys = defaults();
        keys.push(id_key("key-4294967295"));

        assert_eq!(next_id(&keys), "key-4294967296");
    }

    #[test]
    fn normalized_labels_a_blank_key() {
        let key = GridKey {
            id: "ctrl-b".to_owned(),
            label: "  ".to_owned(),
            action: KeyAction::Send {
                steps: vec![held_step(true, false, false, char_step('b'))],
            },
        };

        assert_eq!(normalized(vec![key])[0].label, "Ctrl+B");
    }

    #[test]
    fn normalized_keeps_unknown_keys() {
        assert_eq!(normalized(vec![unknown_key()])[0], unknown_key());
    }

    #[test]
    fn derived_label_of_one_press() {
        for (step, expected) in [
            (held_step(true, false, false, char_step('b')), "Ctrl+B"),
            (held_step(false, false, true, char_step('b')), "Shift+B"),
            (held_step(false, true, false, char_step('b')), "Alt+B"),
            (plain_step(char_step('b')), "b"),
            (plain_step(named_step(NamedKey::Esc)), "Esc"),
            (plain_step(named_step(NamedKey::Backspace)), "Bksp"),
            (
                held_step(false, false, true, named_step(NamedKey::Tab)),
                "Shift+Tab",
            ),
            (
                held_step(true, true, false, named_step(NamedKey::Up)),
                "Ctrl+Alt+↑",
            ),
        ] {
            assert_eq!(derived_label(&[step]), expected);
        }
    }

    #[test]
    fn derived_label_of_a_sequence() {
        let steps = [
            plain_step(named_step(NamedKey::Esc)),
            plain_step(char_step(':')),
            plain_step(char_step('w')),
            plain_step(named_step(NamedKey::Enter)),
        ];

        assert_eq!(derived_label(&steps), "Esc : w Enter");
    }

    #[test]
    fn derived_label_names_space_and_named_keys() {
        assert_eq!(derived_label(&[plain_step(char_step(' '))]), "Space");
        assert_eq!(
            derived_label(&[plain_step(named_step(NamedKey::F12))]),
            "F12"
        );
        assert_eq!(
            derived_label(&[plain_step(named_step(NamedKey::PageUp))]),
            "PgUp"
        );
        assert_eq!(
            derived_label(&[plain_step(named_step(NamedKey::PageDown))]),
            "PgDn"
        );
        assert_eq!(derived_label(&[]), "");
    }

    #[test]
    fn label_placeholder_names_the_field_when_the_draft_has_no_press() {
        assert_eq!(draft(Vec::new()).label_placeholder(), KEY_LABEL_PLACEHOLDER);
    }

    #[test]
    fn label_placeholder_is_the_derived_label_once_a_press_is_set() {
        let key_draft = draft(vec![draft_step(false, Some(named_step(NamedKey::Esc)))]);

        assert_eq!(key_draft.label_placeholder(), "Esc");
    }

    #[test]
    fn next_id_starts_at_one() {
        assert_eq!(next_id(&defaults()), "key-1");
    }

    #[test]
    fn next_id_follows_the_highest() {
        let mut keys = defaults();
        keys.push(id_key("key-1"));
        keys.push(id_key("key-3"));

        assert_eq!(next_id(&keys), "key-4");
    }

    #[test]
    fn next_id_ignores_a_non_number() {
        let mut keys = defaults();
        keys.push(id_key("key-x"));

        assert_eq!(next_id(&keys), "key-1");
    }

    /// A press the key editor holds, one key still missing while `key` is
    /// `None`.
    fn draft_step(ctrl: bool, key: Option<StepKey>) -> DraftStep {
        DraftStep {
            ctrl,
            alt: false,
            shift: false,
            key,
        }
    }

    /// The draft of `steps`.
    fn draft(steps: Vec<DraftStep>) -> KeyDraft {
        KeyDraft { steps }
    }

    #[test]
    fn check_draft_blank_label_takes_the_derived_label() {
        let draft = draft(vec![draft_step(true, Some(char_step('b')))]);

        assert_eq!(
            check_draft("", &draft),
            Ok((
                "Ctrl+B".to_owned(),
                vec![held_step(true, false, false, char_step('b'))]
            ))
        );
    }

    #[test]
    fn check_draft_trims_the_label() {
        let draft = draft(vec![draft_step(true, Some(char_step('b')))]);

        assert_eq!(
            check_draft("  tmux ", &draft),
            Ok((
                "tmux".to_owned(),
                vec![held_step(true, false, false, char_step('b'))]
            ))
        );
    }

    #[test]
    fn check_draft_refuses_no_presses() {
        assert_eq!(
            check_draft("x", &KeyDraft::default()),
            Err(DraftError::NoPresses)
        );
        assert_eq!(DraftError::NoPresses.message(), "Add at least one press.");
    }

    #[test]
    fn check_draft_refuses_a_press_with_no_key() {
        let draft = draft(vec![
            draft_step(false, Some(named_step(NamedKey::Esc))),
            draft_step(false, None),
        ]);

        assert_eq!(check_draft("x", &draft), Err(DraftError::NoKey(2)));
        assert_eq!(DraftError::NoKey(2).message(), "Press 2 has no key.");
    }

    #[test]
    fn check_draft_refuses_a_press_that_types_nothing() {
        let draft = draft(vec![draft_step(true, Some(char_step('1')))]);

        assert_eq!(
            check_draft("x", &draft),
            Err(DraftError::TypesNothing(1, "Ctrl+1".to_owned()))
        );
        assert_eq!(
            DraftError::TypesNothing(1, "Ctrl+1".to_owned()).message(),
            "Press 1 (Ctrl+1) types nothing in a terminal."
        );
    }

    #[test]
    fn check_draft_takes_a_sequence() {
        let draft = draft(vec![
            draft_step(false, Some(named_step(NamedKey::Esc))),
            draft_step(false, Some(char_step(':'))),
            draft_step(false, Some(char_step('w'))),
            draft_step(false, Some(named_step(NamedKey::Enter))),
        ]);

        let (label, steps) = check_draft("vim save", &draft).expect("the sequence saves");

        assert_eq!(label, "vim save");
        assert_eq!(
            key_bytes(&steps, false, off()).as_deref(),
            Some(&b"\x1b:w\r"[..])
        );
    }

    #[test]
    fn move_step_moves_a_press() {
        let mut edited = draft(vec![
            draft_step(false, Some(named_step(NamedKey::Esc))),
            draft_step(false, Some(named_step(NamedKey::Tab))),
            draft_step(false, Some(named_step(NamedKey::Enter))),
        ]);

        edited.move_step(0, 2, Side::After);

        assert_eq!(
            edited,
            draft(vec![
                draft_step(false, Some(named_step(NamedKey::Tab))),
                draft_step(false, Some(named_step(NamedKey::Enter))),
                draft_step(false, Some(named_step(NamedKey::Esc))),
            ])
        );
    }

    #[test]
    fn draft_of_a_key_holds_its_presses() {
        assert_eq!(
            KeyDraft::of(&key_of("ctrl-c")),
            draft(vec![draft_step(true, Some(char_step('c')))])
        );
        assert!(
            KeyDraft::of(&key_of("ctrl")).steps.is_empty(),
            "a key that sends no press starts with none"
        );
    }

    #[test]
    fn apply_key_edit_add_appends() {
        let added = key_of_step("one", plain_step(char_step('1')));
        let mut expected = defaults();
        expected.push(added.clone());

        assert_eq!(
            apply_key_edit(&defaults(), KeyEdit::Add(added)),
            Some(expected)
        );
    }

    #[test]
    fn apply_key_edit_replace_keeps_the_place() {
        let esc = key_of("esc");

        let keys = apply_key_edit(&defaults(), KeyEdit::Replace(11, esc.clone()))
            .expect("index 11 names a key");

        assert_eq!(keys[11], esc);
        assert_eq!(keys.len(), defaults().len());
    }

    #[test]
    fn apply_key_edit_remove_drops_a_key() {
        let mut expected = defaults();
        expected.remove(0);

        assert_eq!(
            apply_key_edit(&defaults(), KeyEdit::Remove(0)),
            Some(expected)
        );
    }

    #[test]
    fn apply_key_edit_refuses_removing_a_protected_key() {
        assert_eq!(
            apply_key_edit(&defaults(), KeyEdit::Remove(7)),
            None,
            "Paste cannot go"
        );
        assert_eq!(
            apply_key_edit(&defaults(), KeyEdit::Remove(40)),
            None,
            "past the grid"
        );
    }

    #[test]
    fn apply_key_edit_moves_paste() {
        let mut expected = defaults();
        let paste = expected.remove(7);
        expected.insert(0, paste);

        assert_eq!(
            apply_key_edit(
                &defaults(),
                KeyEdit::Move {
                    from: 7,
                    to: 0,
                    side: Side::Before
                }
            ),
            Some(expected)
        );
    }

    #[test]
    fn named_key_ids_are_the_picker_ids() {
        let ids: Vec<&str> = NamedKey::ALL.iter().map(|name| name.id()).collect();

        assert_eq!(
            ids,
            [
                "esc",
                "tab",
                "enter",
                "backspace",
                "up",
                "down",
                "left",
                "right",
                "home",
                "end",
                "pageup",
                "pagedown",
                "f1",
                "f2",
                "f3",
                "f4",
                "f5",
                "f6",
                "f7",
                "f8",
                "f9",
                "f10",
                "f11",
                "f12"
            ]
        );
    }
}
