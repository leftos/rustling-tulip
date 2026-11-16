# Phone Settings lists: Quick replies and Keys

Design for RT-1749 (Edit the phone's Quick replies in Settings: add, edit, remove, reorder) and RT-1756 (Arrange the phone's Keys in a grid in Settings, with multi-key keys like Ctrl+B), linked from both issues.

Line references are read against `main` at `eb9201f27`. This file is short-lived: the last part of RT-1756 promotes its durable rules into `docs/native-client.md` ("Phone shape") and `docs/on-disk.md` and deletes it.

## Goal

Today the phone's one-tap replies and its keys are fixed tables in code: the Quick groups `AGENT_GROUPS` and `SHELL_GROUPS` (`apps/native/src/phone/composer.rs:16-38`), the Terminal page's key bar `KEYS` (`apps/native/src/phone/key_bar.rs:9-19`) and the composer's Keys dock `KEY_TRAY` (`composer.rs:69-134`).

The bar and the dock list different keys and send by different routes: the bar through `TerminalPane::press` (`apps/native/src/phone/terminal_view.rs:193`), the dock as fixed bytes through `RootView::send_phone_input` (`apps/native/src/phone/session_view.rs:755`).

After these two items:

- Settings on the phone has a **Quick replies** category: two sets (agent sessions, shells), each a list of titled groups of replies. The user adds, edits, removes and reorders groups and replies, and restores a set's defaults. A reply still types its text, then Return (`quick_bytes`).
- Settings has a **Keys** category: one grid of keys the user rearranges by long-press drag, adds to and removes from. A key is a label and one or more presses, each press one key with Ctrl, Alt or Shift held, so a key can be a chord (Ctrl+B) or a short sequence (Esc : w Enter).
- The same grid drives the Terminal page's key bar (one sideways-scrolling row) and the composer's Keys dock (wrapping rows), and every key in both sends through one path.
- Both lists live on the phone in `native-ui.json`, the same for every host; nothing changes on the wire or in the daemon. An untouched list follows the code's defaults; Restore defaults returns to them.

## Decisions

### Owner rulings

Binding, quoted from Linear.

1. RT-1749: "the replies are kept on the phone, the same for every host, with no daemon or wire change." The defaults are seeded from today's groups, a way to restore them is offered, and the new stored format must survive every later update.
2. RT-1756: "one shared grid drives both the Keys dock and the Terminal key bar, and it is kept on the phone, the same for every host."

### Orchestrator decisions

Binding; the design works within them.

1. **Store.** Orchestrator decision. New optional fields on `UiState` in `native-ui.json` (`apps/native/src/sidebar.rs` `UiState`, `load_ui_state`, `save_ui_state`), saved through the same path as the phone's theme and text size (`RootView::save_ui`). An absent field is the code's defaults; Restore defaults clears the field.

   Each field decodes tolerantly: a value that fails to decode logs a warning and reads as absent, never failing the whole file (the `decodable_popouts` pattern). Every enum in the stored types carries a `#[serde(other)] Unknown` variant. A test loads the released v0.5.1-alpha file (`v0_5_1_alpha_ui_file`, `load_released`) unchanged, and a round-trip test covers the new fields.
2. **No feature branch.** Orchestrator decision. Both land on `main`, RT-1749 first, RT-1756 after it lands.
3. **Reorder by long-press drag.** Orchestrator decision. It reuses the phone precedent (`long_press_drag_area`, `apps/native/src/phone/long_press.rs`) and the pure reorder core (`drag::reorder`, `drag::side_of`, `apps/native/src/drag.rs`); a 2-D grid gets a pure hit-test function with unit tests. Each list and grid also offers per-item Remove; Add appends.
4. **Text entry.** Orchestrator decision. A reply, a group title and a key label are typed into a `TextInput` on a Settings sub-page, as the add-host form does (`apps/native/src/phone/hosts_view.rs`). Settings navigation gains a sub-page state and the `BackStep` variants it needs.
5. **Quick replies data rules.** Orchestrator decision. A reply's text is single-line and non-empty; a group title is non-empty and unique within its set (agent or shell), since titles feed driver ids. Saving a change that breaks a rule is refused in the editor with an inline message.
6. **Keys.** Orchestrator decision. The default grid is the union of today's bar keys (bar order first) and the tray's extra keys. The Terminal key bar is one horizontally scrolling row of the grid's keys in grid order (it never wraps, so the PTY height claim is unchanged); the Keys dock shows the same keys as wrapping rows.

   Paste, Quick and Ctrl cannot be removed (they can be moved). Sticky Ctrl applies to single-stroke keys only; a chord or sequence key ignores it and leaves it armed.
7. **Key model.** Orchestrator decision. A stored key is a label plus an ordered list of steps; a step is modifiers (Ctrl, Alt, Shift) plus one key: a character, or a named key from a fixed list (Esc, Tab, Enter, Backspace, the arrows, Home, End, PageUp, PageDown, F1 to F12). The multi-key editor is a label field, then the steps list (add, remove, reorder), each step modifier toggles and a key picker.
8. **Send path.** Orchestrator decision. The bar and the dock send a key through one path, which honours the terminal's application-cursor mode for arrow keys and delivers Ctrl+C and Ctrl+V chords to the PTY as control bytes, never as the client's copy or paste. The path chosen is P1 below.

### Proposed by the design pass

Each is open to the orchestrator; the evidence is the code at `eb9201f27`. The UI calls a step a "press".

P1. **The one send path is `key_grid::key_bytes` over `keys::to_bytes`, then a sink chosen by surface; never `term_input::key_action`.** Proposed by the design pass. `PhoneRoot::press_grid_key` turns a key's steps into bytes with the pure `key_grid::key_bytes`, which builds each named key's bytes with `keys::to_bytes` (`apps/native/src/keys.rs:82`; application-cursor arrows in `named_key`), then sends them.

   On the Terminal page the sink is `TerminalPane::type_bytes` (`apps/native/src/term_view.rs:1694`: it notes the input, clears the selection and scrolls to the live screen, as the Terminal page's Quick reply already does, `terminal_view.rs:233-235`). In the Keys dock the sink is `RootView::send_phone_input` (`apps/native/src/chat_view.rs:2842`), as the dock does today (`session_view.rs:755`).

   Against `TerminalPane::press`, the bar's route today (`terminal_view.rs:193` into `run_key`, `term_view.rs:1594-1601`): it runs `term_input::key_action` (`apps/native/src/term_input.rs:112`), whose copy keys are `CopyKeys::current()`, `Ctrl` on every target but macOS (`term_input.rs:94-100`; iOS is not `target_os = "macos"`). So Ctrl+V becomes the client's Paste (`term_input.rs:124-130`) and Ctrl+C with a selection becomes Copy.

   `keys::to_bytes` has no copy branch: Ctrl+C is `0x03`, Ctrl+V `0x16`. Application-cursor mode is kept because the bar passes the pane's mode (`Term::app_cursor`, `apps/native/src/term.rs:833`, read today in `run_key`, `term_view.rs:1598`) into `key_bytes` through a new `TerminalPane::app_cursor` accessor.

P2. **The Keys dock sends with application-cursor mode off.** Proposed by the design pass. The dock draws only on the Chat page (`dock_drawn`, `session_view.rs:600`), and the phone holds a terminal only while the Terminal page shows (`wanted_terminal` in `terminal_view.rs`), so on the Chat page there is no terminal mode to read. The dock's arrows send `ESC [ A`, as the tray does today (`KEY_TRAY`'s `↑` is `\x1b[A`).

P3. **A Shift+Enter press sends the session's Shift+Enter bytes.** Proposed by the design pass. `keys::to_bytes` sends a plain `\r` for Shift+Enter, while the pane sends the session's choice (`shift_enter_bytes`, `term_input.rs:19-24`, from `SessionContext.shift_enter`, `term_input.rs:28-50`).

   `key_bytes` takes the session's `ShiftEnter` in its `KeyModes`: from the pane on the Terminal page (a new `TerminalPane::shift_enter` accessor), and in the dock from `SessionContext::of(session, sidebar.agents(host))`, built as `lib.rs:6187` builds the pane's. `shift_enter_bytes` becomes `pub(crate)`.

P4. **A press that types nothing is refused at Save, and a character press builds its own bytes.** Proposed by the design pass. Some combinations have no bytes (Ctrl with a digit other than 2: `control_byte` in `keys.rs`; Shift with a character that has no upper case). `key_grid::check_draft` refuses a key any of whose presses has no bytes ("Press 2 (Ctrl+1) types nothing in a terminal.").

   A character press is the character itself, its upper case with Shift, `keys::to_bytes` of the Ctrl keystroke with Ctrl (Shift then ignored), and an `ESC` prefix with Alt, so Ctrl+Alt+B is `ESC 0x02`. `keys::to_bytes` alone cannot be used for characters: it returns `None` for a plain character, whose text arrives through the input handler, and reads Ctrl+Alt with a character as AltGr (`to_bytes`'s `key_char` branch).

P5. **The stored types are `pub`, in `pub mod quick_replies` and `pub mod key_grid`.** Proposed by the design pass. `UiState` is a `pub` struct with `pub` fields (`sidebar.rs:523`), so its field types must be reachable. It also lets the setters on `RootView` be `pub fn` that specs call through `h.root_update`, and keeps unused-item warnings off a part that lands a setter before its Settings caller. The functions stay `pub(crate)`.

P6. **Tolerance per entry, as `decodable_popouts` does it.** Proposed by the design pass. `decodable_popouts` (`sidebar.rs:1012-1029`) reads a non-list as nothing and drops each entry that does not decode with a warning. The new fields do the same: `null` or a value that is not a list reads as absent (the defaults), and each group or key that does not decode is dropped with a warning naming it.

   After decoding, each list is normalised (P22), so `ui_state()` never holds a list the views cannot draw.

P7. **Two reply fields, one per set.** Proposed by the design pass. `phone_agent_replies` and `phone_shell_replies`, so Restore defaults on one set clears that field alone and the other keeps its edits.

P8. **Each key stores an id; the bar keeps `phone-bar-{id}`, the dock keeps `phone-key-{index}`.** Proposed by the design pass. Today's bar ids (`phone-bar-esc` to `phone-bar-quick`, pinned in `ui_phone_terminal.rs` `BAR_KEYS`) come from `Key::id` (`key_bar.rs:59-72`). The default keys carry those ids, the tray's extras take `shift-tab`, `enter`, `ctrl-c` and `1` to `9`, and a key the user adds takes `key-{n}`, one past the highest `n` in use.

   The dock's `phone-key-{index}` is the key's index in the grid, so the dock's indices shift: Ctrl+C moves from `phone-key-6` to `phone-key-11` (`ui_phone_session.rs` `keys_types_each_key`).

P9. **Hide stays outside the grid, pinned at the bar's right end.** Proposed by the design pass. Hide is not in `KEYS` today; `shown` appends it while the keyboard is up (`key_bar.rs:24-30`), and `the_key_bar_fits_a_375_point_phone_with_the_keyboard_up` (`ui_phone_terminal.rs:838`) holds it inside the phone. In the scrolling row it would scroll away; pinned beside the scroller it stays in reach and the spec keeps its meaning.

P10. **Paste, Quick and Ctrl act in the dock too.** Proposed by the design pass. In the dock, Paste pastes into the composer's field (as the composer's Paste button does, `paste_into_composer`), Quick switches the composer's dock to Quick (`Dock::tap`, `composer.rs`), and Ctrl arms the sticky Ctrl for the next dock key, lit while armed.

P11. **One sticky Ctrl for the bar and the dock.** Proposed by the design pass. Both read `PhoneRoot.key_bar` (`root_view.rs:61`), which is reset when the terminal opens (`terminal_view.rs:69`) and, newly, when the Keys dock opens. `KeyBar` keeps only the armed flag: `toggle_ctrl`, `take_ctrl_for(&GridKey)` and the existing `typed`.

P12. **Settings becomes a page plus a stack of sub-pages, and is no longer `Copy`.** Proposed by the design pass. `Settings { page: Option<Page>, stack: Vec<SubPage> }` (`settings.rs:42` derives `Copy` today; `settings_view.rs:140` copies it out, which becomes a borrow).

   Back pops the innermost sub-page first (`BackStep::Pop`), then the page (`ToList`), then closes (`Close`); the back gesture goes through the same `settings_back` (`settings_view.rs:99-123`). Opening a page clears the stack.

P13. **Editor drafts live on `PhoneRoot` and are saved by a Save button.** Proposed by the design pass. As `PhoneHosts` holds the add form's `TextInput` entities (`hosts_view.rs:83-111`), `PhoneRoot` gets `reply_editor: Option<TextEditor>` (RT-1749) and `key_editor: Option<KeyEditor>` (RT-1756). Each is made when its editor sub-page opens and dropped when that sub-page leaves the stack or Settings closes (`root_view.rs:269, 342` close Settings when a session opens).

   Save, or the field's Submit (Enter), checks and saves; Back discards. `field_box` (`hosts_view.rs:441`) becomes `pub(super)` for these editors.

P14. **One drag area per page, hit-testing recorded list boxes.** Proposed by the design pass. `long_press_drag_area` keeps its "holds the touch" flag in keyed state under the fixed key `"long-press-drag"` (`long_press.rs:127`), and its callbacks get window points only.

   Each editable page draws one area over its body; each list records its box each frame (a `canvas` writing into an `Rc<RefCell<…>>` on `PhoneRoot`), and the press start picks the list and row through `reorder::list_slot` or `reorder::grid_slot`. List rows truncate their text so every row of a list is one height.

P15. **A key's Remove is in its editor; Paste, Quick and Ctrl open no editor.** Proposed by the design pass. A second 44 pt target inside each grid cell would not fit four columns at 390 pt. Reply and group rows have room for a Remove button and carry one.

P16. **Restore defaults asks first, inline.** Proposed by the design pass. The button turns into a row "Replace these with the defaults?" with Restore and Cancel. It is dimmed and takes no tap while the field is absent (the defaults already show).

P17. **An empty set is allowed.** Proposed by the design pass. A set with no groups shows "No quick replies. Add them in Settings › Quick replies." in its Quick dock (`phone-quick-empty`); a group with no replies draws no section in the dock.

P18. **A blank key label saves the derived label.** Proposed by the design pass. The label field's placeholder shows the label the presses make (`derived_label`: "Ctrl+B", "Esc : w Enter"); Save with the field blank stores that.

P19. **The Settings grid has fixed columns; the dock wraps by width.** Proposed by the design pass. The Keys page lays its grid out in `grid_columns(width, 72.0, 8.0)` equal columns (4 at 390 pt), so `grid_slot` is exact. The dock keeps today's `flex_wrap` of label-wide buttons (`session_view.rs:738-761`).

P20. **The new categories come last.** Proposed by the design pass. `Page::ALL` (`settings.rs:18`) becomes Notifications, Hosts, Appearance, Quick replies, Keys, with keys `quick-replies` and `keys` (`phone-settings-row-quick-replies`, `phone-settings-row-keys`).

P21. **The phone web app is untouched.** Proposed by the design pass. `apps/pwa/src/terminal-keys.ts` and `apps/pwa/src/quick.ts` keep their own lists; the rulings name the native phone app.

P22. **Load normalisation.** Proposed by the design pass. Replies: a title is trimmed, a blank title becomes "Group", and a repeated title gets " 2", " 3" until unique; a reply that is blank or holds a line break is dropped.

   Keys: Ctrl, Paste and Quick appear exactly once (a missing one is appended at the end, a repeat dropped); an id that is not lower-case letters, digits and `-`, or repeats an earlier one, is replaced by `key-{n}`; a blank label on a send key becomes its derived label.

P23. **A sub-page whose item is gone closes.** Proposed by the design pass. Sub-pages name items by index. After any edit, `Settings::retain_valid` drops the innermost sub-pages whose index no longer names an item (a Restore under an open group page is the case the UI can reach).

P24. **A bar key is at least a touch target wide and at most 96 pt.** Proposed by the design pass. Today's bar keys shrink to share the width (`key_bar_row`, `terminal_view.rs:154-182`, `flex_1().min_w_0()`). In the scrolling row each key is `flex_none`, at least `TOUCH_TARGET` wide, and a longer label truncates at 96 pt.

## Stored formats

Three new fields on `UiState` (`apps/native/src/sidebar.rs`), beside `theme` and `ui_text_size`:

```rust
/// The phone's Quick replies for sessions with an agent, in order; None is
/// quick_replies::defaults(ReplySet::Agent). A value that is not a list
/// loads as None, and a group that does not decode is dropped.
#[serde(default, deserialize_with = "decodable_reply_groups", skip_serializing_if = "Option::is_none")]
pub phone_agent_replies: Option<Vec<ReplyGroup>>,
/// The same for sessions with no agent; None is the y/n pair.
#[serde(default, deserialize_with = "decodable_reply_groups", skip_serializing_if = "Option::is_none")]
pub phone_shell_replies: Option<Vec<ReplyGroup>>,
/// The phone's key grid, in grid order; None is key_grid::defaults().
#[serde(default, deserialize_with = "decodable_grid_keys", skip_serializing_if = "Option::is_none")]
pub phone_keys: Option<Vec<GridKey>>,
```

A field missing from the file takes `None` through `#[serde(default)]` without calling the decoder; `null` reaches the decoder, which returns `None`. The decoders return `Some(normalized(list))` (P22).

Quick replies, in `apps/native/src/phone/quick_replies.rs`:

```rust
/// One titled group of replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyGroup {
    /// The section label; unique within its set.
    pub title: String,
    /// The texts sent, each typed then Return; single-line, not blank.
    #[serde(default)]
    pub replies: Vec<String>,
}
```

Keys, in `apps/native/src/phone/key_grid.rs`:

```rust
/// One key of the grid: what the bar and the dock draw and what a tap does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridKey {
    /// Unique in the grid; the tail of the bar key's driver id.
    pub id: String,
    /// The name drawn on the key.
    pub label: String,
    pub action: KeyAction,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKey {
    Char { value: char },
    Named { name: NamedKey },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    Esc, Tab, Enter, Backspace, Up, Down, Left, Right, Home, End, PageUp, PageDown,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    #[serde(other)]
    Unknown,
}
```

The file's new keys, written only once the user edits a list:

```json
{
  "phone_agent_replies": [
    { "title": "confirm", "replies": ["yes", "no"] },
    { "title": "mine", "replies": ["run the tests"] }
  ],
  "phone_keys": [
    { "id": "esc", "label": "Esc", "action": { "kind": "send", "steps": [{ "ctrl": false, "alt": false, "shift": false, "key": { "kind": "named", "name": "esc" } }] } },
    { "id": "ctrl", "label": "Ctrl", "action": { "kind": "ctrl" } },
    { "id": "key-1", "label": "tmux", "action": { "kind": "send", "steps": [{ "ctrl": true, "alt": false, "shift": false, "key": { "kind": "char", "value": "b" } }] } }
  ]
}
```

What a file reads as:

| The file holds | It reads as |
| --- | --- |
| No `phone_*` key (every released file, v0.5.1-alpha included) | `None` for all three: today's groups and the default grid, nothing migrated |
| `null` | `None`, no warning |
| A value that is not a list | `None`, one warning naming the field and the value's JSON type |
| A list with an entry that does not decode (a group whose `replies` is a string, a key with no `action`, a `value` of two characters) | the list without that entry, one warning per entry dropped |
| `[]` for a reply set | `Some([])`: an empty set (P17) |
| `[]` for keys | Ctrl, Paste and Quick (P22) |
| A key action, press key or named key a later release added | `Unknown` in place: the key draws dimmed and inert, and is written back as `unknown` (Risks) |
| A list breaking the data rules (blank or repeated titles, multi-line replies, repeated ids) | the normalised list (P22) |

## Pure cores

Each table row is one named unit test; the test name is in the last column.

### quick_replies (`apps/native/src/phone/quick_replies.rs`)

```rust
pub enum ReplySet { Agent, Shell }
impl ReplySet {
    pub(crate) fn of(has_agent: bool) -> Self;
    pub(crate) fn key(self) -> &'static str;   // "agent", "shell"
    pub(crate) fn label(self) -> &'static str; // "Agent sessions", "Shells"
}
pub(crate) fn defaults(set: ReplySet) -> Vec<ReplyGroup>;
pub(crate) fn groups(saved: Option<&[ReplyGroup]>, set: ReplySet) -> Vec<ReplyGroup>;
pub(crate) fn quick_bytes(text: &str) -> Vec<u8>;
pub(crate) fn normalized(groups: Vec<ReplyGroup>) -> Vec<ReplyGroup>;
pub(crate) fn check_title(groups: &[ReplyGroup], typed: &str, editing: Option<usize>) -> Result<String, ListError>;
pub(crate) fn check_reply(typed: &str) -> Result<String, ListError>;
pub(crate) fn apply(groups: &[ReplyGroup], edit: ReplyEdit) -> Option<Vec<ReplyGroup>>; // None: an index names nothing
pub(crate) enum ReplyEdit {
    AddGroup { title: String },
    RenameGroup { group: usize, title: String },
    RemoveGroup { group: usize },
    AddReply { group: usize, text: String },
    SetReply { group: usize, reply: usize, text: String },
    RemoveReply { group: usize, reply: usize },
    MoveGroup { from: usize, to: usize, side: Side },               // A5
    MoveReply { group: usize, from: usize, to: usize, side: Side }, // A5
}
pub(crate) enum ListError { BlankTitle, TitleTaken(String), BlankReply, MultiLineReply }
impl ListError { pub(crate) fn message(&self) -> String; }
```

`apply` never checks the data rules; the editor runs `check_title` or `check_reply` first and saves only an `Ok`.

`ReplySet::of`, `defaults`, `groups`:

| Input | Expected | Test |
| --- | --- | --- |
| `ReplySet::of(true)`, `of(false)` | `Agent`, `Shell` | `the_set_follows_whether_the_session_has_an_agent` |
| `defaults(Agent)` | confirm [yes, no]; common [continue, commit and push, retry, skip, drastically simplify] | `agent_defaults_are_todays_groups` |
| `defaults(Shell)` | confirm [y, n] | `shell_defaults_are_y_and_n` |
| `groups(None, Agent)` | `defaults(Agent)` | `an_absent_set_is_its_defaults` |
| `groups(Some(&[]), Shell)` | `[]` | `an_empty_saved_set_stays_empty` |
| `groups(Some(&[mine]), Agent)` | `[mine]` | `a_saved_set_replaces_the_defaults` |

`quick_bytes` (moved from `composer.rs` with its test):

| Input | Expected | Test |
| --- | --- | --- |
| `"continue"`, `"yes"` | `b"continue\r"`, `b"yes\r"` | `a_quick_reply_types_its_text_then_return` |
| `"é"` | `c3 a9 0d` | `a_quick_reply_types_utf8_then_return` |

`normalized`:

| Input | Expected | Test |
| --- | --- | --- |
| `[{" confirm ", [yes]}]` | `[{"confirm", [yes]}]` | `normalized_trims_titles` |
| `[{"", [a]}]` | `[{"Group", [a]}]` | `normalized_names_a_blank_title_group` |
| `[{a,[x]}, {a,[y]}, {a,[z]}]` | titles `a`, `a 2`, `a 3` | `normalized_numbers_repeated_titles` |
| `[{a,[x]}, {"a 2",[y]}, {a,[z]}]` | titles `a`, `a 2`, `a 3` | `normalized_numbering_skips_a_taken_title` |
| `[{a, ["", "  ", "ok"]}]` | `[{a, [ok]}]` | `normalized_drops_blank_replies` |
| `[{a, ["one\ntwo", "x\ry", "ok"]}]` | `[{a, [ok]}]` | `normalized_drops_multi_line_replies` |
| `defaults(Agent)`, `defaults(Shell)` | unchanged | `normalized_keeps_the_defaults` |

`check_title`, with `groups = defaults(Agent)` (confirm, common):

| Input | Expected | Test |
| --- | --- | --- |
| `"  mine "`, `None` | `Ok("mine")` | `check_title_trims_a_new_title` |
| `""`, `None`; `"  "`, `Some(0)` | `Err(BlankTitle)` | `check_title_refuses_a_blank_title` |
| `"common"`, `None` | `Err(TitleTaken("common"))` | `check_title_refuses_a_taken_title` |
| `"confirm"`, `Some(0)` | `Ok("confirm")` | `check_title_lets_a_group_keep_its_title` |
| `"common"`, `Some(0)` | `Err(TitleTaken("common"))` | `check_title_refuses_renaming_onto_another` |
| `"Common"`, `None` | `Ok("Common")` | `check_title_compares_exactly` |

`check_reply` and `ListError::message`:

| Input | Expected | Test |
| --- | --- | --- |
| `"continue"` | `Ok("continue")` | `check_reply_takes_one_line` |
| `"  keep my spaces "` | `Ok` of the same text, as typed | `check_reply_keeps_the_text_as_typed` |
| `""`, `"   "` | `Err(BlankReply)` | `check_reply_refuses_a_blank_reply` |
| `"a\nb"`, `"a\rb"` | `Err(MultiLineReply)` | `check_reply_refuses_a_line_break` |
| each `ListError` | "A group needs a title." / "Another group is already called “common”." / "A reply needs some text." / "A reply is one line: it is typed, then Return." | `list_errors_say_what_to_fix` |

`apply`, with `G = defaults(Agent)`:

| Input | Expected | Test |
| --- | --- | --- |
| `AddGroup { "mine" }` | G, then `{mine, []}` | `apply_add_group_appends` |
| `RenameGroup { 1, "often" }` | common renamed, its replies kept | `apply_rename_keeps_the_replies` |
| `RemoveGroup { 0 }` | `[common]` | `apply_remove_group_drops_it` |
| `AddReply { 0, "maybe" }` | confirm [yes, no, maybe] | `apply_add_reply_appends` |
| `SetReply { 1, 2, "try again" }` | retry becomes try again, in place | `apply_set_reply_edits_in_place` |
| `RemoveReply { 1, 0 }` | common without continue | `apply_remove_reply_drops_it` |
| `RemoveGroup { 5 }`, `RemoveReply { 1, 9 }`, `SetReply { 7, 0, "x" }` | `None` | `apply_refuses_an_index_that_names_nothing` |
| `MoveGroup { 1, 0, Before }` | `[common, confirm]` | `apply_move_group_to_the_top` |
| `MoveReply { 1, 0, 2, After }` | [commit and push, retry, continue, skip, drastically simplify] | `apply_move_reply_down` |
| `MoveReply { 1, 4, 0, Before }` | [drastically simplify, continue, commit and push, retry, skip] | `apply_move_reply_to_the_top` |
| `MoveGroup { 0, 0, After }` | G unchanged | `apply_move_onto_itself_changes_nothing` |

### settings (`apps/native/src/phone/settings.rs`)

```rust
pub(crate) enum Page { Notifications, Hosts, Appearance, QuickReplies, Keys }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubPage {
    ReplyGroup { set: ReplySet, group: usize },                    // A3
    GroupTitle { set: ReplySet, group: Option<usize> },            // A4; None: a new group
    Reply { set: ReplySet, group: usize, reply: Option<usize> },   // A4; None: a new reply
    KeyEditor { key: Option<usize> },                              // B6; None: a new key
    KeyPicker { step: usize },                                     // B6
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) page: Option<Page>,
    /// The sub-pages open over the page, innermost last.
    pub(crate) stack: Vec<SubPage>,
}
pub(crate) enum BackStep { Pop, ToList, Close }
impl Settings {
    pub(crate) fn open(&mut self, page: Page);       // the page, its stack cleared
    pub(crate) fn push(&mut self, sub: SubPage);     // nothing without a page
    pub(crate) fn top(&self) -> Option<SubPage>;
    pub(crate) fn back(&mut self) -> BackStep;
    pub(crate) fn title(&self) -> &'static str;
    pub(crate) fn retain_valid(&mut self, valid: impl Fn(SubPage) -> bool); // P23
}
impl SubPage { pub(crate) fn title(self) -> &'static str; }
```

| Input | Expected | Test |
| --- | --- | --- |
| `Page::ALL`'s keys | notifications, hosts, appearance, quick-replies, keys | `the_categories_are_listed_in_order` |
| no page, `push(ReplyGroup)` | stack `[]` | `a_sub_page_needs_a_page` |
| page QuickReplies, stack `[ReplyGroup]`, `back()` | `Pop`; page QuickReplies, stack `[]` | `back_from_a_sub_page_pops_it` |
| page QuickReplies, stack `[ReplyGroup, Reply]`, `back()` three times | `Pop`, `Pop`, `ToList` | `back_steps_out_one_sub_page_at_a_time` |
| page QuickReplies, stack `[ReplyGroup]`, `open(Keys)` | page Keys, stack `[]` | `opening_a_page_clears_its_sub_pages` |
| no page / page QuickReplies / top ReplyGroup / GroupTitle / Reply / KeyEditor / KeyPicker | "Settings" / "Quick replies" / "Group" / "Group title" / "Reply" / "Key" / "Choose a key" | `the_title_is_the_innermost_pages` |
| stack `[ReplyGroup{Agent,1}, Reply{Agent,1,Some(0)}]`, valid only for group 0 | stack `[]` | `a_sub_page_whose_item_is_gone_closes_with_those_over_it` |
| stack `[ReplyGroup{Agent,0}, Reply{Agent,0,Some(3)}]`, valid only for replies under 3 | stack `[ReplyGroup{Agent,0}]` | `only_the_stale_sub_pages_close` |

The existing `back_from_a_page_goes_to_the_list` and `back_from_the_list_closes` stay. A variant lands with the part that first constructs it (marked above), so no part leaves a variant unused.

### reorder (`apps/native/src/phone/reorder.rs`)

```rust
pub(crate) struct ListBox { pub(crate) top: f32, pub(crate) height: f32 }
pub(crate) struct GridBox { pub(crate) left: f32, pub(crate) top: f32, pub(crate) width: f32, pub(crate) height: f32, pub(crate) cols: usize, pub(crate) gap: f32 }
/// The row of an equal-height list a finger at y points at, and which half.
pub(crate) fn list_slot(y: f32, list: ListBox, len: usize) -> Option<(usize, Side)>;
/// The cell of a fixed-column grid a finger at (x, y) points at, and which half.
pub(crate) fn grid_slot(x: f32, y: f32, grid: GridBox, len: usize) -> Option<(usize, Side)>; // B7
/// The items with the one at from moved to side of the one at to (drag::reorder over indices).
pub(crate) fn reordered<T: Clone>(items: &[T], from: usize, to: usize, side: Side) -> Vec<T>;
pub(crate) enum ListId { ReplyGroups(ReplySet), Replies, KeyGrid, KeySteps } // KeyGrid, KeySteps: B7
pub(crate) struct ReorderDrag { pub(crate) list: ListId, pub(crate) from: usize, pub(crate) over: Option<(usize, Side)> }
```

`Side` is `crate::drag::Side`. A list's row pitch is `height / len`; the row is clamped into the list and the half comes from `drag::side_of` over that row. A grid's cell width is `(width - gap * (cols - 1)) / cols` and its row pitch `(height + gap) / rows`; a column or row outside the grid clamps to its edge, a point in a gap belongs to the cell before it, and a cell past the last key is the last key's `After`.

`list_slot`, with `ListBox { top: 100, height: 200 }` and `len` 4 (rows 50 tall):

| Input `y` | Expected | Test |
| --- | --- | --- |
| 100, 124.9 | `(0, Before)` | `list_slot_top_half_of_the_first_row` |
| 125 | `(0, After)` | `list_slot_middle_counts_as_after` |
| 160 | `(1, Before)` | `list_slot_second_row` |
| 299 | `(3, After)` | `list_slot_bottom_of_the_last_row` |
| 40 (above) | `(0, Before)` | `list_slot_above_is_before_the_first` |
| 400 (below) | `(3, After)` | `list_slot_below_is_after_the_last` |
| any, `len` 0; any, `height` 0 | `None` | `list_slot_empty_list_has_no_slot` |

`grid_slot`, with `GridBox { left: 16, top: 100, width: 352, height: 148, cols: 4, gap: 8 }` and `len` 10 (cells 82 by 44; x pitch 90, y pitch 52; three rows):

| Input (`x`, `y`) | Expected | Test |
| --- | --- | --- |
| (20, 110) | `(0, Before)` | `grid_slot_left_half_of_the_first_cell` |
| (60, 110) | `(0, After)` | `grid_slot_right_half_of_the_first_cell` |
| (100, 110), in the gap after cell 0 | `(0, After)` | `grid_slot_gap_belongs_to_the_cell_before` |
| (110, 110) | `(1, Before)` | `grid_slot_second_column` |
| (20, 160) | `(4, Before)` | `grid_slot_second_row` |
| (110, 210) | `(9, Before)` | `grid_slot_last_key` |
| (200, 210), (290, 210): cells past the last key | `(9, After)` | `grid_slot_past_the_last_key_is_after_it` |
| (20, 40), above | `(0, Before)` | `grid_slot_above_clamps_to_the_first_row` |
| (500, 110), right of it | `(3, After)` | `grid_slot_right_clamps_to_the_last_column` |
| any, `len` 0; any, `cols` 0 | `None` | `grid_slot_empty_grid_has_no_slot` |

`reordered`:

| Input | Expected | Test |
| --- | --- | --- |
| `[a, b, c, d]`, from 0, to 2, `After` | `[b, c, a, d]` | `reordered_moves_down` |
| `[a, b, c, d]`, from 3, to 0, `Before` | `[d, a, b, c]` | `reordered_moves_to_the_top` |
| `[a, b, c, d]`, from 0, to 1, `Before` | unchanged | `reordered_before_the_next_changes_nothing` |
| `[a, b, c, d]`, from 1, to 1, `After` | unchanged | `reordered_onto_itself_changes_nothing` |
| `[a, b, c]`, from 5, to 0, `Before` | unchanged | `reordered_out_of_range_changes_nothing` |

### key_grid (`apps/native/src/phone/key_grid.rs`)

```rust
pub(crate) struct KeyModes { pub(crate) app_cursor: bool, pub(crate) shift_enter: ShiftEnter } // shift_enter: B4
pub(crate) fn defaults() -> Vec<GridKey>;
pub(crate) fn resolved(saved: Option<&[GridKey]>) -> Vec<GridKey>;                       // B3
pub(crate) fn normalized(keys: Vec<GridKey>) -> Vec<GridKey>;                            // B3
pub(crate) fn step_bytes(step: &Step, modes: KeyModes) -> Option<Vec<u8>>;
pub(crate) fn key_bytes(steps: &[Step], sticky_ctrl: bool, modes: KeyModes) -> Option<Vec<u8>>;
pub(crate) fn takes_sticky_ctrl(key: &GridKey) -> bool;
pub(crate) fn is_protected(key: &GridKey) -> bool;  // Ctrl, Paste, Quick
pub(crate) fn is_inert(key: &GridKey) -> bool;      // an Unknown action, or a press with an Unknown key
pub(crate) fn derived_label(steps: &[Step]) -> String;                                   // B3
pub(crate) fn next_id(keys: &[GridKey]) -> String;                                       // B3
pub(crate) fn grid_columns(width: f32, min_cell: f32, gap: f32) -> usize;               // B5
pub(crate) struct KeyDraft { pub(crate) steps: Vec<DraftStep> }                          // B6
pub(crate) struct DraftStep { pub(crate) ctrl: bool, pub(crate) alt: bool, pub(crate) shift: bool, pub(crate) key: Option<StepKey> }
impl KeyDraft {
    pub(crate) fn of(key: &GridKey) -> Self;                                             // B6
    pub(crate) fn move_step(&mut self, from: usize, to: usize, side: Side);              // B7
}
pub(crate) fn check_draft(label: &str, draft: &KeyDraft) -> Result<(String, Vec<Step>), DraftError>; // B6
pub(crate) enum DraftError { NoPresses, NoKey(usize), TypesNothing(usize, String) }      // B6; presses count from 1
pub(crate) enum KeyEdit { Add(GridKey), Replace(usize, GridKey), Remove(usize), Move { from: usize, to: usize, side: Side } } // B6, Move in B7
pub(crate) fn apply_key_edit(keys: &[GridKey], edit: KeyEdit) -> Option<Vec<GridKey>>;   // None: an index names nothing, or a protected key is removed
impl NamedKey {
    pub(crate) const ALL: [Self; 24];
    pub(crate) fn id(self) -> &'static str;     // "esc", "pageup", "f1": the picker's ids
    pub(crate) fn label(self) -> &'static str;  // "Esc", "PgUp", "↑"
}
```

`KeyBar` (`apps/native/src/phone/key_bar.rs`) keeps the armed flag only:

```rust
impl KeyBar {
    pub(crate) fn new() -> Self;
    pub(crate) fn armed(&self) -> bool;
    pub(crate) fn toggle_ctrl(&mut self);
    /// Whether armed Ctrl applies to the key: true, and disarmed, for a key
    /// that takes sticky Ctrl while armed; false, and left as it was, else.
    pub(crate) fn take_ctrl_for(&mut self, key: &GridKey) -> bool;
    pub(crate) fn typed(&mut self, text: &str) -> Option<Keystroke>; // unchanged
}
```

`defaults` and `resolved`:

| Input | Expected | Test |
| --- | --- | --- |
| `defaults()` ids and labels | esc Esc, tab Tab, ctrl Ctrl, up ↑, down ↓, left ←, right →, paste Paste, quick Quick, shift-tab Shift+Tab, enter Enter, ctrl-c Ctrl+C, 1 to 9 the digits | `the_default_grid_is_the_bar_then_the_trays_extras` |
| `key_bytes` of each default send key, modes off | Esc `1b`, Tab `09`, ↑ `1b 5b 41`, ↓ `1b 5b 42`, ← `1b 5b 44`, → `1b 5b 43`, Shift+Tab `1b 5b 5a`, Enter `0d`, Ctrl+C `03`, each digit itself | `the_default_grid_sends_the_old_tray_bytes` |
| `resolved(None)` (B3) | `defaults()` | `resolved_absent_is_the_defaults` |
| `resolved(Some(&k))` (B3) | `k` | `resolved_saved_is_the_saved_grid` |

`step_bytes`, modes `{ app_cursor: false, shift_enter: BackslashCr }` unless the row says:

| Press | Expected | Test |
| --- | --- | --- |
| Esc; Tab; Enter; Backspace | `1b`; `09`; `0d`; `7f` | `step_bytes_named_keys` |
| Ctrl+Tab | `09` (`to_bytes` leaves Tab plain under Ctrl) | `step_bytes_ctrl_tab_is_tab` |
| Shift+Tab | `1b 5b 5a` | `step_bytes_shift_tab` |
| Ctrl+Backspace | `08` | `step_bytes_ctrl_backspace` |
| Up; Up with `app_cursor` | `1b 5b 41`; `1b 4f 41` | `step_bytes_up_honours_application_cursor_mode` |
| Ctrl+Up with `app_cursor` | `1b 5b 31 3b 35 41` | `step_bytes_modified_arrow_is_always_csi` |
| Alt+Left | `1b 5b 31 3b 33 44` | `step_bytes_alt_left` |
| Home; PageUp; F1; F5 | `1b 5b 48`; `1b 5b 35 7e`; `1b 4f 50`; `1b 5b 31 35 7e` | `step_bytes_navigation_and_function_keys` |
| char b; Shift+b | `62`; `42` | `step_bytes_character_and_its_upper_case` |
| Ctrl+b; Ctrl+c; Ctrl+v; Ctrl+Shift+c | `02`; `03`; `16`; `03` | `step_bytes_ctrl_letters_are_control_bytes` |
| Ctrl+space (char `' '`); Ctrl+2 | `00`; `00` | `step_bytes_ctrl_space_is_nul` |
| Alt+b; Ctrl+Alt+b | `1b 62`; `1b 02` | `step_bytes_alt_prefixes_escape` |
| Ctrl+1; Shift+1 | `None` | `step_bytes_a_press_that_types_nothing` |
| char é | `c3 a9` | `step_bytes_utf8_character` |
| `StepKey::Unknown`; `Named { Unknown }` | `None` | `step_bytes_unknown_key_types_nothing` |
| Shift+Enter, `BackslashCr` (B4) | `5c 0d` | `shift_enter_sends_backslash_return_by_default` |
| Shift+Enter, `Lf` (B4) | `0a` | `shift_enter_sends_line_feed_for_an_lf_session` |
| Ctrl+Shift+Enter (B4) | `0d` | `shift_enter_with_ctrl_is_a_plain_return` |

`key_bytes`:

| Input | Expected | Test |
| --- | --- | --- |
| [Esc, `:`, `w`, Enter], not sticky | `1b 3a 77 0d` | `key_bytes_sends_each_press_in_order` |
| [Esc, Unknown] | `None` | `key_bytes_one_silent_press_silences_the_key` |
| `[]` | `None` | `key_bytes_no_presses_types_nothing` |
| [Up], sticky | `1b 5b 31 3b 35 41` | `key_bytes_sticky_ctrl_joins_a_single_press` |
| [char c], sticky | `03` | `key_bytes_sticky_ctrl_on_a_letter_is_its_control_byte` |
| [Esc], sticky | `1b` | `key_bytes_sticky_ctrl_on_esc_is_esc` |

`takes_sticky_ctrl`, `is_protected`, `is_inert`, `KeyBar`:

| Input | Expected | Test |
| --- | --- | --- |
| Esc; char 1 (one press, no modifier) | `true` | `a_single_plain_press_takes_sticky_ctrl` |
| Shift+Tab; Ctrl+C; [Esc, Esc] | `false` | `a_chord_or_sequence_ignores_sticky_ctrl` |
| Ctrl, Paste, Quick, an Unknown action | `false` | `action_keys_ignore_sticky_ctrl` |
| Ctrl, Paste, Quick; a send key; an Unknown action | protected; not; not | `ctrl_paste_and_quick_are_protected` |
| an Unknown action; a press with an Unknown key; Esc | inert; inert; not | `unknown_keys_are_inert` |
| unarmed `take_ctrl_for(Esc)` | `false`, unarmed | `take_ctrl_for_unarmed_is_false` |
| armed `take_ctrl_for(Esc)` | `true`, unarmed after | `take_ctrl_for_a_plain_key_disarms` |
| armed `take_ctrl_for(Ctrl+C)`; armed `take_ctrl_for([Esc, :])` | `false`, still armed | `take_ctrl_for_a_chord_leaves_ctrl_armed` |
| `toggle_ctrl()` twice | armed, then unarmed | `toggle_ctrl_arms_and_disarms` |

The `typed` tests in `key_bar.rs` (`armed_ctrl_turns_a_typed_letter_into_its_ctrl_keystroke_once`, `armed_ctrl_then_space_is_nul`, `armed_ctrl_then_return_is_a_return`, `ctrl_c_from_the_bar_is_the_interrupt_byte`) stay, arming through `toggle_ctrl` once B2 removes `press`.

`normalized`, `derived_label`, `next_id` (B3):

| Input | Expected | Test |
| --- | --- | --- |
| `defaults()` | unchanged | `normalized_keeps_the_defaults` |
| `[]` | `[ctrl, paste, quick]` with the default ids and labels | `normalized_empty_grid_gets_the_protected_keys` |
| the defaults without quick | the same, then quick at the end | `normalized_appends_a_missing_protected_key` |
| `[paste, esc, paste (id paste-2)]` | `[paste, esc, ctrl, quick]` | `normalized_drops_a_repeated_protected_key` |
| two keys with id `esc` | the second gets `key-1` | `normalized_renames_a_repeated_id` |
| a key with id `Bad Id` | `key-1` | `normalized_replaces_an_unfit_id` |
| a send key labelled `"  "` with [Ctrl+b] | label `Ctrl+B` | `normalized_labels_a_blank_key` |
| a key with an Unknown action | kept as it is | `normalized_keeps_unknown_keys` |
| [Ctrl+b]; [Esc]; [Shift+Tab]; [Ctrl+Alt+Up] | `Ctrl+B`; `Esc`; `Shift+Tab`; `Ctrl+Alt+↑` | `derived_label_of_one_press` |
| [Esc, `:`, `w`, Enter] | `Esc : w Enter` | `derived_label_of_a_sequence` |
| [char `' '`]; [F12]; [PageUp]; `[]` | `Space`; `F12`; `PgUp`; `""` | `derived_label_names_space_and_named_keys` |
| `defaults()` | `key-1` | `next_id_starts_at_one` |
| the defaults plus `key-1`, `key-3` | `key-4` | `next_id_follows_the_highest` |
| the defaults plus `key-x` | `key-1` | `next_id_ignores_a_non_number` |

`grid_columns` (B5):

| Input | Expected | Test |
| --- | --- | --- |
| (358, 72, 8) | 4 | `grid_columns_at_390_points` |
| (300, 72, 8) | 3 | `grid_columns_narrower` |
| (60, 72, 8); (0, 72, 8) | 1 | `grid_columns_never_below_one` |

`check_draft`, `apply_key_edit`, `NamedKey` (B6), the moves (B7):

| Input | Expected | Test |
| --- | --- | --- |
| `""`, [Ctrl+b] | `Ok(("Ctrl+B", [Ctrl+b]))` | `check_draft_blank_label_takes_the_derived_label` |
| `"  tmux "`, [Ctrl+b] | `Ok(("tmux", [Ctrl+b]))` | `check_draft_trims_the_label` |
| `"x"`, `[]` | `Err(NoPresses)`: "Add at least one press." | `check_draft_refuses_no_presses` |
| `"x"`, [Esc, a press with no key] | `Err(NoKey(2))`: "Press 2 has no key." | `check_draft_refuses_a_press_with_no_key` |
| `"x"`, [Ctrl+1] | `Err(TypesNothing(1, "Ctrl+1"))`: "Press 1 (Ctrl+1) types nothing in a terminal." | `check_draft_refuses_a_press_that_types_nothing` |
| `"vim save"`, [Esc, `:`, `w`, Enter] | `Ok` with the four presses | `check_draft_takes_a_sequence` |
| `KeyDraft::of` the default ctrl-c key | one press: Ctrl on, key char c | `draft_of_a_key_holds_its_presses` |
| `Add(k)` to the defaults | the defaults, then `k` | `apply_key_edit_add_appends` |
| `Replace(11, k)` | `k` at index 11 | `apply_key_edit_replace_keeps_the_place` |
| `Remove(0)` | the defaults without Esc | `apply_key_edit_remove_drops_a_key` |
| `Remove(7)` (Paste); `Remove(40)` | `None` | `apply_key_edit_refuses_removing_a_protected_key` |
| `NamedKey::ALL` ids | esc, tab, enter, backspace, up, down, left, right, home, end, pageup, pagedown, f1 … f12 | `named_key_ids_are_the_picker_ids` |
| `Move { 7, 0, Before }` (B7) | Paste first, then Esc and the rest | `apply_key_edit_moves_paste` |
| [Esc, Tab, Enter], `move_step(0, 2, After)` (B7) | [Tab, Enter, Esc] | `move_step_moves_a_press` |

## Views

| File | Draws |
| --- | --- |
| `apps/native/src/phone/settings_view.rs` | the category list (two more rows), the header title from `Settings::title`, and the body by page and top sub-page; `settings_back` handles `Pop`, drops an editor whose sub-page left and runs `retain_valid`; the open helpers the shot example calls (`open_settings_replies`, `open_settings_reply_group(set, group)`, `open_settings_keys`, `open_settings_key_editor(index)`) |
| `apps/native/src/phone/quick_replies_view.rs` (new) | the Quick replies page, a group page, the title and reply editors, the Restore confirm, the page's drag area |
| `apps/native/src/phone/keys_view.rs` (new) | the Keys page grid, Add key, Restore with its confirm, the key editor and the key picker, their drag areas |
| `apps/native/src/phone/reorder.rs` (new) | the pure slots, plus the glue: a `canvas` that records a list's box, and the page's area over `long_press_drag_area` that sets `PhoneRoot.reorder_drag` on press, its `over` on each move, and applies the move on lift |
| `apps/native/src/phone/terminal_view.rs` | the key bar: a scrolling row of the grid's keys, Hide pinned after it |
| `apps/native/src/phone/session_view.rs` | the Quick dock over the saved groups, the Keys dock over the grid, `press_grid_key` |
| `apps/native/src/phone/root_view.rs` | the new fields: `reply_editor`, `key_editor`, `reorder_drag`, `list_boxes` |

Layout:

- **Quick replies page.** For each set, a `section_label` with the set's label, its group rows (title, "{n} replies" muted, a Remove button and a chevron; a touch target tall; the title truncates), then Add group and Restore defaults. While a row is lifted it draws at reduced opacity and a 2 pt accent line marks the drop slot.
- **Group page.** A Title row (the title and a chevron, opening the title editor), a "Replies" label, the reply rows (text truncated, Remove, chevron), then Add reply.
- **Text editor.** The field in its `field_box`, focused on open, then the inline refusal in the danger colour, then Save, all at the top of the sub-page so a raised keyboard cannot cover them.
- **Keys page.** A muted hint "Long-press a key to move it; tap one to change it.", the grid in `grid_columns` equal columns of outlined cells (inert keys dimmed), then Add key and Restore defaults.
- **Key editor.** The label field (placeholder: the derived label), a "Presses" label, a row per press (Ctrl, Alt and Shift toggles drawn `Primary` while on, the key button showing the key's label or "Choose key", Remove), Add press, the inline refusal, Save, and Remove key for a saved key.
- **Key picker.** A one-character field with Use, refusing anything but one character ("Type one character."), then one row per `NamedKey::ALL`; a tap sets the press's key and pops back to the editor.
- **Key bar.** `phone-key-bar` holds the scrolling row `phone-key-bar-scroll` (`flex_1`, `overflow_x_scroll`), its keys `flex_none` (P24), Ctrl lit while armed, inert keys dimmed; then Hide while the keyboard is up (P9). The bar stays one key tall.
- **Keys dock.** Today's `flex_wrap` over the grid in order, Ctrl lit while armed, inert keys dimmed.

Driver ids; today's ids keep their meaning except the dock's indices (P8):

| Id | What | Part |
| --- | --- | --- |
| `phone-settings-row-quick-replies`, `phone-settings-row-keys` | the category rows | A3, B5 |
| `phone-replies-{set}`, `phone-replies-{set}-groups` | a set's section and its group list (`set` is `agent` or `shell`) | A3 |
| `phone-replies-{set}-group-{g}` | a group row; a tap opens its page | A3 |
| `phone-replies-{set}-group-{g}-remove` | its Remove | A4 |
| `phone-replies-{set}-add-group` | Add group | A4 |
| `phone-replies-{set}-restore`, `phone-replies-{set}-restore-confirm`, `phone-replies-{set}-restore-cancel` | Restore defaults and its confirm row | A4 |
| `phone-replies-group-title` | the group page's Title row | A3 (its tap: A4) |
| `phone-replies-replies`, `phone-replies-reply-{r}` | the group page's reply list and rows | A3 |
| `phone-replies-reply-{r}-remove`, `phone-replies-add-reply` | a reply's Remove, Add reply | A4 |
| `phone-edit-field`, `phone-edit-save`, `phone-edit-error` | a text editor's field, Save and refusal | A4 |
| `phone-quick-empty` | the Quick dock's empty line | A2 |
| `phone-dock-quick`, `phone-terminal-quick`, `phone-quick-{title}-{index}`, `phone-terminal-quick-{title}-{index}` | unchanged | |
| `phone-key-bar`, `phone-bar-hide`, `phone-dock-keys` | unchanged | |
| `phone-key-bar-scroll` | the bar's scrolling row | B2 |
| `phone-bar-{id}` | a bar key: `phone-bar-esc` … `phone-bar-quick` as today, then `phone-bar-shift-tab`, `phone-bar-enter`, `phone-bar-ctrl-c`, `phone-bar-1` … `phone-bar-9`, and `phone-bar-key-{n}` for added keys | B2 |
| `phone-key-{index}` | a dock key, by its index in the grid | B1 |
| `phone-keys-grid`, `phone-keys-cell-{index}` | the Keys page grid and its cells | B5 |
| `phone-keys-restore`, `phone-keys-restore-confirm`, `phone-keys-restore-cancel` | Restore defaults and its confirm row | B5 |
| `phone-keys-add` | Add key | B6 |
| `phone-key-editor-label`, `phone-key-editor-steps`, `phone-key-editor-step-{s}` with `-ctrl`, `-alt`, `-shift`, `-key`, `-remove` after it, `phone-key-editor-add-step`, `phone-key-editor-save`, `phone-key-editor-remove`, `phone-key-editor-error` | the key editor | B6 |
| `phone-key-picker-char`, `phone-key-picker-char-use`, `phone-key-picker-error`, `phone-key-picker-{name}` (`NamedKey::id`) | the key picker | B6 |

## Parts

Each part is one brief and one commit, in landing order. A part's file count covers source and spec files; `apps/native/tests/ui/main.rs` (the spec binary's module list) and `tools/test-map.json` ride with the new spec file that needs them, and docs are listed apart.

Every command runs through the repo's gate wrapper as CLAUDE.md says. The filters below follow `cargo nextest run -p rustling-tulip-native --cargo-profile test-release`. "Red first" names the tests written before the change and run once failing (or not compiling, for a new module), then green. Each item's last part writes its CHANGELOG bullet under Unreleased.

### RT-1749 Quick replies

**A1. Quick replies core, behaviour unchanged.**

- Files: `apps/native/src/phone/quick_replies.rs` (new), `apps/native/src/phone/mod.rs`, `apps/native/src/phone/composer.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/src/phone/root_view.rs`.
- Change: (1) `pub mod quick_replies` with `ReplyGroup`, `ReplySet`, `defaults`, `groups`, `quick_bytes` and their tests. (2) Delete `QuickGroup`, `AGENT_GROUPS`, `SHELL_GROUPS`, `quick_groups`, `quick_bytes` and their tests from `composer.rs`, which keeps `Dock`, `DockButton` and, until B1, the key tray. (3) `quick_dock` (`session_view.rs:669-717`) and `quick_reply_items` (`root_view.rs:303-308`) read `quick_replies::groups(None, ReplySet::of(self.shown_has_agent(cx)))` as owned strings.
- Tests: red first `-E 'test(/^phone::quick_replies::/)'`. Green and unchanged: `--test ui -E 'test(/^ui_phone_session::/) | test(/^ui_phone_terminal::/)'`.
- Shots: none; nothing drawn changes.
- Docs: `docs/architecture.md` Task Index row "Change the phone's session page": `composer.rs` holds `Dock` and the key tray, `quick_replies.rs` the Quick groups.

**A2. Saved replies.**

- Files: `apps/native/src/phone/quick_replies.rs`, `apps/native/src/sidebar.rs`, `apps/native/src/lib.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/tests/ui/ui_phone_session.rs`.
- Change: (1) `normalized` (P22). (2) The two fields, `decodable_reply_groups` (P6) and `Sidebar::set_phone_replies(set, Option<Vec<ReplyGroup>>)`; `RootView::set_phone_replies(set, groups, cx)` sets it, calls `save_ui` and notifies, as `set_ui_text_size` does (`lib.rs:3186-3191`). (3) The Quick dock and `quick_reply_items` read `groups` over the shown set's field. (4) The dock's empty line (P17).
- Tests, red first: `-E 'test(/^phone::quick_replies::tests::normalized/) | test(/^sidebar::tests::phone_repl/)'`, the sidebar tests being `phone_replies_absent_from_a_v0_5_1_alpha_file_load_as_defaults` (`load_released` over `v0_5_1_alpha_ui_file`, both fields `None`).

  Also `phone_replies_round_trip` (both `Some`, one an empty set), `phone_replies_null_loads_as_absent`, `phone_replies_that_are_not_a_list_load_as_absent`, `phone_reply_group_that_does_not_decode_is_dropped`, `phone_replies_load_normalised`.

  UI: `--test ui -E 'test(/^ui_phone_session::saved_/)'`: `saved_agent_replies_replace_the_default_groups` (seeded through `seed_ui`), `saved_shell_replies_show_for_a_shell`, `saved_reply_types_its_text_then_return`, `saved_empty_set_shows_the_empty_line`, `saved_by_set_phone_replies_and_redrawn` (calls `RootView::set_phone_replies` through `h.root_update`; `saved_ui` holds it and the open dock shows it).

  Green: `-E 'test(/^sidebar::tests::ui_file_written_by_v0_5_1_alpha/) | test(/^sidebar::tests::ui_state_round_trips/)'`.
- Shots: none.
- Docs: `docs/on-disk.md` `native-ui.json` entry: the two fields, absent meaning the built-in sets, the per-entry drop and the normalisation.

**A3. Settings sub-pages and the Quick replies page.**

- Files: `apps/native/src/phone/settings.rs`, `apps/native/src/phone/settings_view.rs`, `apps/native/src/phone/quick_replies_view.rs` (new), `apps/native/src/phone/mod.rs`, `apps/native/tests/ui/ui_phone_quick_replies.rs` (new, with `main.rs` and `tools/test-map.json`), `apps/native/examples/shot.rs`.
- Change: (1) `Settings { page, stack }`, `SubPage::ReplyGroup`, `BackStep::Pop`, `push`, `top`, `retain_valid`, the titles and `Page::QuickReplies` (P12, P20, P23). 

  (2) `settings_view.rs` borrows `Settings`, dispatches on the top sub-page, and `settings_back` handles `Pop`. (3) `quick_replies_view.rs` draws the page (both sets' group rows, a row opening its group page) and the group page (Title row, reply rows), read-only. (4) Shot views `phone-settings-replies` and `phone-settings-reply-group` (the agent set's `common` group), with `PhoneRoot::open_settings_replies` and `open_settings_reply_group`.
- Tests, red first: `-E 'test(/^phone::settings::/)'`; `--test ui -E 'test(/^ui_phone_quick_replies::/)'`: `the_quick_replies_row_opens_its_page`, `the_page_lists_each_sets_groups_in_order`, `a_group_row_opens_its_replies`, `back_from_a_group_page_returns_to_the_page_then_the_list`, `the_back_gesture_steps_out_of_a_group_page`, `saved_groups_show_on_the_page`. Green: `--test ui -E 'test(/^ui_phone_settings::/)'`.
- Shots: add `phone-settings-replies` and `phone-settings-reply-group`; refresh `phone-settings` (a fourth row).
- Docs: `docs/native-client.md` "Phone shape" Settings paragraph (the categories, which also fixes its stale "Appearance comes later", and sub-pages with Back); `docs/testing.md` (the `phone-settings` description and the two views); `CLAUDE.md` `native-shot` view list; `docs/architecture.md` Task Index row "Change a screen of the phone" (adds `quick_replies_view.rs`); `docs/README.md` glossary: "Quick replies".

**A4. Add, edit, remove and restore replies.**

- Files: `apps/native/src/phone/quick_replies.rs`, `apps/native/src/phone/settings.rs`, `apps/native/src/phone/quick_replies_view.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/src/phone/hosts_view.rs`, `apps/native/tests/ui/ui_phone_quick_replies.rs`.
- Change: (1) `check_title`, `check_reply`, `ListError`, `ReplyEdit` (no moves yet) and `apply`. (2) `SubPage::GroupTitle` and `SubPage::Reply`; `PhoneRoot.reply_editor` (P13); `field_box` becomes `pub(super)`. (3) Remove on group and reply rows, Add group, Add reply, and Restore with its confirm (P16); every save writes the whole resolved set through `RootView::set_phone_replies`, and Restore writes `None`. (4) The specs.
- Tests, red first: `-E 'test(/^phone::quick_replies::tests::(check_|list_errors|apply_)/) | test(/^phone::settings::/)'`; `--test ui -E 'test(/^ui_phone_quick_replies::/)'` adds `adding_a_reply_appends_it_and_saves`, `editing_a_reply_changes_its_text`, `a_blank_reply_is_refused_with_a_message`, `adding_a_group_with_a_taken_title_is_refused`, `renaming_a_group_renames_its_dock_ids`, `removing_a_reply_drops_it`, `removing_a_group_drops_it`, `the_first_edit_of_the_defaults_saves_the_whole_set`.

  Also: `restore_asks_then_brings_back_the_defaults` (`saved_ui` then has no `phone_agent_replies`), `cancel_keeps_the_saved_replies`, `restore_is_dimmed_while_the_defaults_show`, `restore_closes_a_group_page_that_no_longer_exists`, `back_from_an_editor_discards_the_draft`, `enter_in_the_field_saves`, `the_dock_shows_an_edited_reply`.
- Shots: refresh `phone-settings-replies` and `phone-settings-reply-group` (Remove, Add and Restore now drawn).
- Docs: `docs/native-client.md` (the editors, the data rules and their messages, Restore); `docs/testing.md` (the two views' descriptions).

**A5. Reorder by long-press drag.**

- Files: `apps/native/src/phone/reorder.rs` (new), `apps/native/src/phone/mod.rs`, `apps/native/src/phone/quick_replies.rs`, `apps/native/src/phone/quick_replies_view.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/tests/ui/ui_phone_quick_replies.rs`.
- Change: (1) `list_slot`, `reordered`, `ListId` (`ReplyGroups`, `Replies`), `ReorderDrag`, and `ReplyEdit::MoveGroup`, `MoveReply`. (2) The glue: recorded list boxes and one `long_press_drag_area` per page (P14), `PhoneRoot.reorder_drag` and `list_boxes`. (3) The lifted row and the slot line; the move saves once, on lift. (4) The specs, through `Harness::long_press_drag`.
- Tests, red first: `-E 'test(/^phone::reorder::/) | test(/^phone::quick_replies::tests::apply_move/)'`; `--test ui -E 'test(/^ui_phone_quick_replies::(a_long_press|a_tap_on_a_row)/)'`: `a_long_press_drag_moves_a_reply_below_the_next`, `a_long_press_drag_moves_a_group_to_the_top`, `a_long_press_drag_in_the_shell_set_leaves_the_agent_set`, `a_long_press_drag_saves_the_new_order_once`, `a_long_press_without_a_move_changes_nothing`, `a_tap_on_a_row_still_opens_it`.
- Shots: none; the lifted row is a passing state.
- Docs: `docs/native-client.md` (the gesture); `docs/architecture.md` Task Index row "Reorder a list by drag on the phone" (`drag.rs` `reorder`, `side_of` → `phone/reorder.rs` → `phone/long_press.rs` `long_press_drag_area`); CHANGELOG bullet for RT-1749.

### RT-1756 Keys

**B1. Key grid core and the Keys dock.**

- Files: `apps/native/src/phone/key_grid.rs` (new), `apps/native/src/phone/mod.rs`, `apps/native/src/phone/key_bar.rs`, `apps/native/src/phone/composer.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/tests/ui/ui_phone_session.rs`.
- Change: (1) `pub mod key_grid`: the stored types, `defaults`, `step_bytes`, `key_bytes` with `KeyModes { app_cursor }`, `takes_sticky_ctrl`, `is_protected`, `is_inert`. (2) `KeyBar::toggle_ctrl` and `take_ctrl_for` beside today's `press`, which the bar keeps until B2.

  (3) `keys_dock` draws `key_grid::defaults()` with `phone-key-{index}` ids; `PhoneRoot::press_dock_key` runs Paste, Quick and Ctrl (P10) and sends a send key's `key_bytes` through `send_phone_input` with application-cursor mode off (P1, P2); the armed state resets when the dock opens (P11); `KeyEntry` and `KEY_TRAY` leave `composer.rs` with their test. (4) The specs; `keys_types_each_key` taps `phone-key-11` for Ctrl+C.
- Tests, red first: `-E 'test(/^phone::key_grid::/) | test(/^phone::key_bar::tests::(take_ctrl|toggle_ctrl)/)'`; `--test ui -E 'test(/^ui_phone_session::(keys_|the_keys_dock|the_dock_)/)'`: `keys_types_each_key` (updated), `the_keys_dock_lists_the_grid_in_order` (`phone-key-0` … `phone-key-20`), `the_keys_dock_opens_with_ctrl_unarmed`.

  Also: `the_dock_ctrl_arms_the_next_key_once` (`phone-key-2` then `phone-key-3` sends `1b 5b 31 3b 35 41`, the next `phone-key-3` `1b 5b 41`), `the_dock_ctrl_leaves_a_chord_armed` (`phone-key-2`, then `phone-key-11` sends `03` and Ctrl stays lit), `the_dock_paste_pastes_into_the_composer`, `the_dock_quick_switches_to_the_quick_dock`. Green: `--test ui -E 'test(/^ui_phone_session::/)'`.
- Shots: none here; B2 adds the dock's view.
- Docs: `docs/native-client.md` (the Keys dock's keys and its Ctrl); `docs/README.md` glossary: "Key grid", "Keys dock"; `docs/architecture.md` new Task Index row "Change what a key on the phone sends" (`phone/key_grid.rs` → `phone/key_bar.rs` → `phone/session_view.rs` `press_grid_key` → `phone/terminal_view.rs` → `keys.rs` `to_bytes` → `term_view.rs` `type_bytes` → `chat_view.rs` `send_phone_input` → `ui_phone_terminal.rs`, `ui_phone_session.rs`).

**B2. The Terminal key bar over the grid.**

- Files: `apps/native/src/phone/key_bar.rs`, `apps/native/src/phone/terminal_view.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/src/term_view.rs`, `apps/native/tests/ui/ui_phone_terminal.rs`, `apps/native/examples/shot.rs`.
- Change: (1) `key_bar.rs` loses `KEYS`, `Key`, `shown`, `Action` and `press`, keeping the armed flag and `typed`. (2) `TerminalPane::app_cursor`; `press_dock_key` becomes `press_grid_key(index, surface, window, cx)` with the sink by surface (P1). (3) `key_bar_row` draws the scrolling row and the pinned Hide (P9, P24); `tap_bar` goes. (4) Shot view `phone-session-keys`.
- Tests, red first: `--test ui -E 'test(/^ui_phone_terminal::/)'` adds `the_key_bar_is_one_row_of_the_grid_in_order` (every `phone-bar-{id}` in grid order on one `y`), `the_key_bar_scrolls_sideways_to_its_last_key` (a sideways `ScrollEvent` over `phone-key-bar-scroll` brings `phone-bar-9` inside the window), `the_key_bar_stays_one_row_tall`, `the_key_bar_keeps_hide_pinned_at_its_right_end` (scrolled, Hide does not move).

  Also: `the_bar_ctrl_c_key_interrupts_with_text_selected` (a selection made as `ui_terminal`'s copy specs make one, then `phone-bar-ctrl-c` sends `03` and copies nothing), `the_bar_enter_and_digit_keys_send_their_bytes`, `armed_ctrl_is_left_armed_by_the_bar_ctrl_c_chord`.

  Green: `the_key_bar_arrows_send_cursor_keys` (application-cursor mode through the new path), `ctrl_then_a_typed_c_sends_etx`, `a_second_ctrl_tap_disarms`, `paste_sends_the_clipboard_text_bracketed_when_asked`, `quick_types_into_the_terminal`, `the_key_bar_fits_a_375_point_phone_with_the_keyboard_up`, `the_terminal_page_draws_the_terminal_and_the_key_bar`; `-E 'test(/^phone::key_bar::/)'`.
- Shots: refresh `phone-terminal` (the bar begins Esc … Quick, Shift+Tab and runs off the right edge); add `phone-session-keys` (the `phone-session` view with the Keys dock open: the grid in wrapping rows).
- Docs: `docs/native-client.md` "Phone shape" key bar paragraph; `docs/testing.md` (the `phone-terminal` description, the new view); `CLAUDE.md` view list.

**B3. Saved key grid.**

- Files: `apps/native/src/phone/key_grid.rs`, `apps/native/src/sidebar.rs`, `apps/native/src/lib.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/src/phone/terminal_view.rs`, `apps/native/tests/ui/ui_phone_terminal.rs`.
- Change: (1) `resolved`, `normalized`, `derived_label`, `next_id`. (2) `phone_keys`, `decodable_grid_keys`, `Sidebar::set_phone_keys` and `RootView::set_phone_keys(Option<Vec<GridKey>>, cx)`. (3) The bar and the dock read `resolved` over `ui_state().phone_keys`; an inert key draws dimmed and sends nothing.
- Tests, red first: `-E 'test(/^phone::key_grid::tests::(normalized|resolved|derived_label|next_id)/) | test(/^sidebar::tests::phone_key/)'`, the sidebar tests being `phone_keys_absent_from_a_v0_5_1_alpha_file_load_as_defaults`.

  Also `phone_keys_round_trip` (a chord, a sequence, every named key, the three protected kinds), `phone_keys_with_an_unknown_kind_load_as_unknown` (a `"kind": "macro"` action, a `"kind": "mouse"` press key, a `"name": "f13"`), `phone_keys_that_are_not_a_list_load_as_absent`, `phone_key_that_does_not_decode_is_dropped` (a `value` of two characters), `phone_keys_load_normalised`.

  UI: `--test ui -E 'test(/^ui_phone_terminal::saved_grid/)'`: `saved_grid_orders_the_bar`, `saved_grid_without_paste_still_draws_paste`, `saved_grid_unknown_key_is_dimmed_and_sends_nothing`, `saved_grid_chord_sends_its_bytes` (a seeded Ctrl+B sends `02`).
- Shots: none.
- Docs: `docs/on-disk.md` (`phone_keys`).

**B4. Shift+Enter presses send the session's choice.**

- Files: `apps/native/src/phone/key_grid.rs`, `apps/native/src/term_input.rs`, `apps/native/src/term_view.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/tests/ui/ui_phone_terminal.rs`.
- Change: (1) `KeyModes.shift_enter` and the Shift+Enter rule in `step_bytes` (P3); `shift_enter_bytes` becomes `pub(crate)`. (2) `TerminalPane::shift_enter`; the dock's modes from `SessionContext::of`.
- Tests, red first: `-E 'test(/^phone::key_grid::tests::shift_enter/)'`; `--test ui -E 'test(/^ui_phone_terminal::a_shift_enter_key/)'`: `a_shift_enter_key_sends_backslash_return_to_a_shell` (a seeded Shift+Enter key sends `5c 0d`).
- Shots: none.
- Docs: `docs/native-client.md` (one sentence).

**B5. The Keys page.**

- Files: `apps/native/src/phone/settings.rs`, `apps/native/src/phone/settings_view.rs`, `apps/native/src/phone/keys_view.rs` (new), `apps/native/src/phone/mod.rs`, `apps/native/src/phone/key_grid.rs`, `apps/native/tests/ui/ui_phone_keys.rs` (new, with `main.rs` and `tools/test-map.json`).
- Change: (1) `Page::Keys` and its row. (2) `grid_columns`. (3) `keys_view.rs`: the hint, the grid in fixed columns (P19), Restore with its confirm (P16); `open_settings_keys`.
- Tests, red first: `-E 'test(/^phone::key_grid::tests::grid_columns/) | test(/^phone::settings::/)'`; `--test ui -E 'test(/^ui_phone_keys::/)'`: `the_keys_row_opens_the_grid`, `the_grid_lists_the_keys_in_order`, `the_grid_draws_four_columns_at_390_points`, `restore_asks_then_brings_back_the_default_grid` (`saved_ui` then has no `phone_keys`, and the bar shows the defaults), `restore_is_dimmed_while_the_defaults_show`, `cancel_keeps_the_saved_grid`.
- Shots: refresh `phone-settings` (a fifth row); B6 adds the Keys page's views.
- Docs: `docs/native-client.md`; `docs/testing.md` (the `phone-settings` description); `docs/architecture.md` Task Index row "Change a screen of the phone" (adds `keys_view.rs`).

**B6. The key editor and its picker.**

- Files: `apps/native/src/phone/key_grid.rs`, `apps/native/src/phone/settings.rs`, `apps/native/src/phone/keys_view.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/tests/ui/ui_phone_keys.rs`, `apps/native/examples/shot.rs`.
- Change: (1) `KeyDraft`, `DraftStep`, `check_draft`, `DraftError`, `KeyEdit` (no move yet), `apply_key_edit`, and `NamedKey::ALL`, `id`, `label`. (2) `SubPage::KeyEditor` and `SubPage::KeyPicker`; `PhoneRoot.key_editor` (P13). (3) The editor (P15, P18) and the picker; Add key; a tap on a send or inert cell opens its editor, a tap on Paste, Quick or Ctrl does nothing. (4) Shot views `phone-settings-keys` and `phone-settings-key-edit`, with `open_settings_key_editor`.
- Tests, red first: `-E 'test(/^phone::key_grid::tests::(check_draft|draft_of|apply_key_edit|named_key)/) | test(/^phone::settings::/)'`; `--test ui -E 'test(/^ui_phone_keys::/)'` adds `adding_a_ctrl_b_key_appends_it_to_the_bar_and_the_dock` (Add key, Add press, Ctrl, the picker's character `b`, Use, Save; `saved_ui` holds `key-1`; `phone-bar-key-1` sends `02` on the Terminal page and `phone-key-21` sends `02` from the dock).

  Also: `a_key_with_no_presses_is_refused`, `a_press_with_no_key_is_refused`, `ctrl_and_a_digit_is_refused_as_typing_nothing`, `a_blank_label_saves_the_derived_label`, `a_sequence_sends_each_press_in_order` (Esc `:` `w` Enter sends `1b 3a 77 0d`).

  And `editing_a_key_keeps_its_place_and_id`, `removing_a_key_drops_it_from_the_bar`, `paste_quick_and_ctrl_open_no_editor`, `the_picker_sets_a_named_key`, `the_picker_refuses_two_characters`, `back_from_the_picker_keeps_the_draft`, `back_from_the_editor_discards_it`.
- Shots: add `phone-settings-keys` (the default grid) and `phone-settings-key-edit` (the editor on `ctrl-c`: one press, Ctrl lit, key `c`).
- Docs: `docs/native-client.md` (the editor, the picker, the refusals); `docs/testing.md` (the two views); `CLAUDE.md` view list.

**B7. Reorder keys and presses.**

- Files: `apps/native/src/phone/reorder.rs`, `apps/native/src/phone/key_grid.rs`, `apps/native/src/phone/keys_view.rs`, `apps/native/tests/ui/ui_phone_keys.rs`.
- Change: (1) `grid_slot`, `ListId::KeyGrid` and `ListId::KeySteps`. (2) `KeyEdit::Move` and `KeyDraft::move_step`. (3) The drag areas over the grid and over the editor's presses.
- Tests, red first: `-E 'test(/^phone::reorder::tests::grid_slot/) | test(/^phone::key_grid::tests::(move_step|apply_key_edit_moves)/)'`; `--test ui -E 'test(/^ui_phone_keys::a_long_press_drag/)'`: `a_long_press_drag_moves_a_key_and_the_bar_follows`, `a_long_press_drag_moves_paste`, `a_long_press_drag_reorders_a_keys_presses`.
- Shots: none.
- Docs: `docs/native-client.md` (the grid's gesture). Promote this file's durable rules (the stored shapes into `docs/on-disk.md`; the send path P1, the defaults and the editors' rules into `docs/native-client.md` "Phone shape") and delete `docs/plans/phone-settings-lists.md`; CHANGELOG bullet for RT-1756.

## Risks

1. **Sideways scrolling by finger is unmeasured.** Nothing in `apps/native/src` uses `overflow_x_scroll` today, so whether gpui_ios pans a horizontal scroller by touch, and whether a key still takes a tap at the end of a pan, is known only from the specs' synthetic `ScrollEvent`s. B2's specs prove the layout; the owner's device check proves the pan. A failure there needs a new ruling, since decision 6 fixes the bar as one scrolling row.
2. **Long-press drag inside the scrolling Settings body is unmeasured on a device.** The area claims the press at its start (`claims` in `long_press.rs`), but whether the body also scrolls while the claimed finger moves is untested outside the harness. The lists are short, and no edge auto-scroll is designed.
3. **A downgrade loses a newer kind.** This build writes an `Unknown` action, press key or named key back as `unknown`, so a later build's `"kind": "macro"` reads `unknown` after the phone ran this build. The rule in CLAUDE.md ("Saved user data survives every update") covers later builds reading older files, which holds.
4. **The soft keyboard's sticky-Ctrl path still runs `key_action`.** Ctrl armed, then `c` typed on the soft keyboard, reaches `TerminalPane` through its typed filter (`typed_ctrl`, `terminal_view.rs:105-114`), so with a selection on screen it copies instead of interrupting on iOS (`CopyKeys::Ctrl`). Decision 8 covers the bar and dock keys; this path is unchanged, and the orchestrator may book it.
5. **`type_bytes` skips the pane's dead-key handling.** `press` ran `end_dead_key` (`term_view.rs`), which flushes or drops a pending accent; `type_bytes` does not, so a composition pending when a bar key is tapped is not flushed first. If the device check shows a stray accent, the bar's sink gains a call that ends a pending composition first.
6. **The dock's indices change meaning.** Ctrl+C moves from `phone-key-6` to `phone-key-11` and `phone-key-2` is now Ctrl, so specs and any driver script that tap the dock by index move with B1.
7. **Most of the 21 default keys start off screen on the bar.** At 390 pt roughly Esc through → show without a scroll (unmeasured until B2's shot); the Keys page is where the owner puts the keys they use first.
8. **Group titles are free text in driver ids** (`phone-quick-{title}-{index}`). Decision 5's uniqueness keeps the ids distinct, but a title with spaces or punctuation makes an awkward selector.
9. **Text entry in Settings is new ground.** The add-host form is the only phone `TextInput` in Settings, and the scroll inset under a raised keyboard in `phone-settings-body` is unproven; the editors keep the field, the refusal and Save at the top of their sub-page. RT-1827 (iOS text-input notifications in `TextInput` specs) is a soft dependency of the editors' specs.
10. **Sub-pages address items by index.** Only this phone edits these lists and every edit runs `retain_valid` (P23), so a stale index closes its sub-page rather than editing the wrong item.
