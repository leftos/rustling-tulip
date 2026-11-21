//! Claude Code's chat screen reader and its answer recipes.
//!
//! [`read`] reads the visible rows of a session's rendered screen into what
//! the chat shows: a full-screen overlay first (an `AskUserQuestion` step
//! with previews among them), then a dialog (permission, plan approval,
//! folder trust, an `AskUserQuestion` step or its review), then the input
//! box. A Heads up notice does not own the keyboard, so it rides along with
//! the input box's own state.
//! [`recipe`] turns an answer to one of those prompts into the keys that
//! give it, each group with the reading the daemon waits for after it.
//! [`read_view`] reads which conversation is in view (the main one or a
//! subagent's) and which part of the screen holds the keys, and
//! [`next_view_key`] gives the one key that moves towards a wanted view.
//!
//! The parsers key on the text Claude Code 2.1.307 draws (the captures in
//! the daemon's `screen/fixtures/`), never on colour, which the rows do not
//! carry. Claude Code 2.1.319 also draws a named session's name in a rule
//! under a permission dialog, the screen's last row.

use std::cmp::Ordering;
use std::ops::Range;

use agent_api::{
    AnswerRecipe, AnswerRefusal, DraftReading, Key, KeyHolder, RecipeStep, ScreenExpect,
    ScreenWork, ViewGoal, ViewReading, ViewRefusal, ViewStep,
};
use protocol::{
    ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ChatQuestionTab, ChatScreen,
    ComposerState,
};

/// The character a horizontal rule is drawn with.
const RULE_CHAR: char = '─';

/// The character a content box's rules are drawn with.
const BOX_CHAR: char = '╌';

/// The marker on the row the cursor is on, and the input box's prompt.
const CURSOR: char = '❯';

/// The prompt a bash-mode input box draws in place of the cursor.
const BASH_MODE: char = '!';

/// The largest body a prompt carries, in bytes.
const BODY_CAP: usize = 4096;

/// How many non-empty rows above the input box are searched for a spinner.
const SPINNER_ROWS: usize = 24;

/// How many non-empty rows at the bottom are searched for an overlay's keys.
const OVERLAY_ROWS: usize = 4;

/// The key line each overlay shows, and the name the chat gives it. An
/// `AskUserQuestion` step whose options carry previews is one: the chat
/// recognises it but does not answer it.
const OVERLAYS: [(&str, &str); 4] = [
    ("Showing detailed transcript", "transcript"),
    ("f to fork · Esc to close", "btw"),
    ("Enter to continue · Esc to cancel", "rewind"),
    ("· n to add notes ·", "question_preview"),
];

/// The mark of a question tab not yet answered.
const TAB_OPEN: char = '☐';

/// The mark of an answered question tab.
const TAB_DONE: char = '☒';

/// The mark of the fixed `Submit` tab, which is not a question.
const TAB_SUBMIT: char = '✔';

/// A multi-select option's box, unticked and ticked.
const BOX_OFF: &str = "[ ]";
const BOX_ON: &str = "[✔]";

/// The label of the option a question's own answer is typed into; a
/// single-choice question draws it with a trailing period.
const TYPE_SOMETHING: &str = "Type something";

/// The option that ends the turn instead of answering; terminal-only.
const CHAT_ABOUT_THIS: &str = "Chat about this";

/// The unnumbered row a multi-select question is submitted from.
const NEXT_ROW: &str = "Next";

/// The start of a question step's footer.
const QUESTION_FOOTER: &str = "Enter to select";

/// The vim editor mode's marker heading a question's footer in insert mode.
const VIM_INSERT_HEAD: &str = "-- INSERT -- · ";

/// The head of a vim question footer a narrow pane cuts at its wrap: the
/// rest of the footer sits on the rows under it.
const ENTER_TO: &str = "Enter to";

/// The title row of the review step, the row above its options, and the
/// label of its first option.
const REVIEW_TITLE: &str = "Review your answers";
const REVIEW_READY: &str = "Ready to submit your answers?";
const SUBMIT_ANSWERS: &str = "Submit answers";

/// The label start of a permission option that saves an allow rule; the
/// rule follows it.
const ALWAYS_ALLOW: [&str; 2] = [
    "Yes, and don\u{2019}t ask again for: ",
    "Yes, and don't ask again for: ",
];

/// The label of the plan option a plan's feedback is typed into, before
/// any text replaces it.
const PLAN_FEEDBACK_LABEL: &str = "Tell Claude what to change";

/// What a dialog parser found.
enum Parsed {
    /// The whole dialog.
    Prompt(ChatPrompt),
    /// The dialog's header without its footer: a frame still being drawn.
    HalfDrawn,
}

/// What the screen whose visible rows are `rows` shows the chat.
pub(crate) fn read(rows: &[String]) -> ChatScreen {
    if let Some(prompt) = parse_model_picker(rows) {
        return ChatScreen {
            composer: ComposerState::Dialog,
            prompt: Some(prompt),
        };
    }
    if let Some(name) = overlay(rows) {
        return ChatScreen {
            composer: ComposerState::Overlay {
                name: name.to_owned(),
            },
            prompt: None,
        };
    }
    let start = after_last_rule(rows);
    let dialog = parse_permission(rows, start)
        .or_else(|| parse_plan(rows, start))
        .or_else(|| parse_trust(rows, start))
        .or_else(|| parse_mcp_server(rows, start))
        .or_else(|| parse_switch_model(rows, start))
        .or_else(|| parse_question(rows));
    match dialog {
        Some(Parsed::Prompt(prompt)) => ChatScreen {
            composer: ComposerState::Dialog,
            prompt: Some(prompt),
        },
        Some(Parsed::HalfDrawn) => ChatScreen {
            composer: ComposerState::Dialog,
            prompt: None,
        },
        None => ChatScreen {
            composer: composer(rows),
            prompt: parse_heads_up(rows),
        },
    }
}

/// [`read`] of `rows`, `dim` being `rows` with every character not drawn dim
/// replaced by a space: a box holding only the agent's suggested next
/// prompt, drawn dim, is a suggestion rather than a draft, or working while
/// a spinner shows above it.
pub(crate) fn read_styled(rows: &[String], dim: &[String]) -> ChatScreen {
    let mut screen = read(rows);
    if screen.composer == ComposerState::Draft
        && let Some((top, text)) = suggestion(rows, dim)
    {
        screen.composer = if spinner_above(&rows[..top]) {
            ComposerState::Working
        } else {
            ComposerState::Suggestion { text }
        };
    }
    screen
}

/// [`read_view`] of `rows`, `dim` as in [`read_styled`]: a box holding only
/// the agent's dim suggestion shows no first line.
pub(crate) fn read_view_styled(rows: &[String], dim: &[String]) -> ViewReading {
    let mut view = read_view(rows);
    if suggestion(rows, dim).is_some() {
        view.box_first_line = None;
    }
    view
}

/// The rule over the input box and the one under it: the last two rule rows;
/// `None` for a screen showing fewer.
fn box_rules(rows: &[String]) -> Option<(usize, usize)> {
    let rules = rule_indices(rows);
    let [.., top, bottom] = rules[..] else {
        return None;
    };
    Some((top, bottom))
}

/// The draft the input box between the last two rules holds, as the user
/// typed it: `None` for a box holding no text, the agent's placeholder or
/// suggestion, bash mode, or a first row with no cursor on it. `dim` is
/// `rows` with every character not drawn dim replaced by a space, as
/// [`suggestion`] takes it.
///
/// The box's first row loses its prompt and the one space after it, and
/// every later row the box's two-column indent; trailing spaces go from all
/// of them. A row takes the next one with it when the box wrapped it at its
/// inner width (Ink wraps at a word boundary, and a row's wrap flag does not
/// tell that from a newline the user typed); otherwise the two are separate
/// lines, and a blank row is a new empty one.
pub(crate) fn read_draft(rows: &[String], dim: &[String]) -> Option<DraftReading> {
    let (top, bottom) = box_rules(rows)?;
    if suggestion(rows, dim).is_some() {
        return None;
    }
    let boxed = &rows[top + 1..bottom];
    let first = boxed.iter().position(|row| !row.trim().is_empty())?;
    let last = boxed.iter().rposition(|row| !row.trim().is_empty())?;
    let head = draft_first_text(&boxed[first])?;
    if first == last && head.trim().is_empty() {
        return None;
    }
    if is_try_placeholder(head.trim()) || is_agent_placeholder(head.trim()) {
        return None;
    }
    let mut text_rows = Vec::with_capacity(last - first + 1);
    text_rows.push(head.trim_end().to_owned());
    text_rows.extend(
        boxed[first + 1..=last]
            .iter()
            .map(|row| box_row_text(row).to_owned()),
    );
    // The box wraps at its inner width: the rule's width less the prompt's
    // `❯ ` and the box's two columns of right padding.
    let inner = rows[top].chars().count().saturating_sub(4);
    let text = join_draft(&text_rows, inner);
    let height = bottom - top - 1;
    // The box's height is capped by the screen and by the box's own tallest
    // draw; the lower of the two only refuses a clear the box might still
    // hold more of.
    let screen = rows.len();
    let cap = (screen * 3 / 8).min((screen / 2).saturating_sub(5).max(3));
    Some(DraftReading {
        pasted: holds_paste_placeholder(&text.replace('\n', " ")),
        whole: height < cap,
        text,
        rows: height,
    })
}

/// The keys that clear the box holding `draft`: a step per line clear, each
/// one press of its own, since a burst of them is dropped whole. ctrl+u
/// deletes to the start of its line and at a line's start the newline before
/// it, so N lines take 2N-1 presses; the recipe sends 2·rows+2, and a press
/// on the empty box does nothing. No key moves the keys out of the box, so
/// the presses all reach it. A paste or image token is ordinary text on its
/// row and goes with its line. A draft the box shows only the end of is
/// refused.
///
/// # Errors
///
/// [`AnswerRefusal::NotThisDialog`] for a draft whose text the box does not
/// show whole: its keys would clear less than the draft.
pub(crate) fn clear_draft_recipe(draft: &DraftReading) -> Result<AnswerRecipe, AnswerRefusal> {
    if !draft.whole {
        return Err(AnswerRefusal::NotThisDialog);
    }
    Ok(AnswerRecipe {
        steps: (0..2 * draft.rows + 2)
            .map(|_| RecipeStep {
                keys: vec![Key::ClearLine],
                expect: ScreenExpect::Any,
            })
            .collect(),
    })
}

/// The text of the box's first row: the prompt and the one space after it
/// taken off; `None` for a row holding no prompt, bash mode's `!` among them.
fn draft_first_text(row: &str) -> Option<&str> {
    let rest = row.trim_start().strip_prefix(CURSOR)?;
    Some(match rest.chars().next() {
        Some(space) if space.is_whitespace() => &rest[space.len_utf8()..],
        _ => rest,
    })
}

/// A box row after the first as its draft reads it: up to two leading spaces
/// (the box's indent) and its trailing spaces taken off.
fn box_row_text(row: &str) -> &str {
    let trimmed = row.trim_end();
    let unindented = trimmed.strip_prefix(' ').unwrap_or(trimmed);
    unindented.strip_prefix(' ').unwrap_or(unindented)
}

/// The box's body rows joined into the draft's text: a row the box wrapped
/// runs on into the next with one space, and any other pair is two lines.
fn join_draft(text_rows: &[String], inner: usize) -> String {
    let mut text = String::new();
    for (index, row) in text_rows.iter().enumerate() {
        if let Some(previous) = index.checked_sub(1).map(|previous| &text_rows[previous]) {
            text.push(if wraps(previous, row, inner) {
                ' '
            } else {
                '\n'
            });
        }
        text.push_str(row);
    }
    text
}

/// Whether the box wrapped `previous` onto `row`: `row` holds text, starts
/// with no whitespace (the box never wraps onto an indented row), and
/// `previous`'s width, one space and `row`'s first word are over the box's
/// inner width, which a newline the user typed never is.
fn wraps(previous: &str, row: &str, inner: usize) -> bool {
    if row.starts_with(char::is_whitespace) {
        return false;
    }
    let Some(word) = row.split_whitespace().next() else {
        return false;
    };
    previous.chars().count() + 1 + word.chars().count() > inner
}

/// Whether `text` holds a paste's or an image's placeholder — `[Pasted text
/// #1 +40 lines]`, `[Image #1]` — so it is not the whole draft.
fn holds_paste_placeholder(text: &str) -> bool {
    const LEADS: [&str; 2] = ["[Pasted text #", "[Image #"];
    LEADS.iter().any(|lead| {
        text.match_indices(lead).any(|(start, _)| {
            text[start + lead.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    })
}

/// The agent's suggested next prompt in the box between the last two rules,
/// with the index of the rule over the box: the box's one row of text after
/// the prompt, at least two characters, no placeholder, and every character
/// after the first dim in `dim` (`rows` with every character not drawn dim
/// replaced by a space). The first may be dim or not: the agent's own cursor
/// draws that cell.
fn suggestion(rows: &[String], dim: &[String]) -> Option<(usize, String)> {
    let (top, bottom) = box_rules(rows)?;
    let mut body = (top + 1..bottom).filter(|&index| !rows[index].trim().is_empty());
    let index = body.next()?;
    if body.next().is_some() {
        return None;
    }
    let row = &rows[index];
    let rest = row.trim_start().strip_prefix(CURSOR)?;
    let text = rest.trim();
    if text.chars().count() < 2 || is_try_placeholder(text) || is_agent_placeholder(text) {
        return None;
    }
    // From the text's second character on; the first is the cursor's cell.
    let lead = rest.chars().take_while(|c| c.is_whitespace()).count() + 1;
    let skip = row.chars().count() - rest.chars().count() + lead;
    let mut lit = dim.get(index).map_or("", String::as_str).chars().skip(skip);
    let all_dim = rest.chars().skip(lead).all(|c| {
        let shown = lit.next();
        c.is_whitespace() || shown == Some(c)
    });
    all_dim.then(|| (top, text.to_owned()))
}

/// The name of the overlay whose key line is among the bottom rows.
fn overlay(rows: &[String]) -> Option<&'static str> {
    let bottom: Vec<&String> = rows
        .iter()
        .rev()
        .filter(|row| !row.trim().is_empty())
        .take(OVERLAY_ROWS)
        .collect();
    OVERLAYS
        .iter()
        .find(|(keys, _)| bottom.iter().any(|row| row.contains(keys)))
        .map(|&(_, name)| name)
}

/// A permission dialog: the kind row under the dialog's rule, the subject,
/// the content, the `Do you want to …?` question and its numbered options,
/// then the `Esc to cancel` footer.
fn parse_permission(rows: &[String], start: usize) -> Option<Parsed> {
    let question = find(rows, start, |row| {
        row.trim_start().starts_with("Do you want to")
    })?;
    let Some(footer) = find(rows, question + 1, |row| {
        row.trim_start().starts_with("Esc to cancel")
    }) else {
        return Some(Parsed::HalfDrawn);
    };
    let options = collect_options(&rows[question + 1..footer], |_| false);
    if options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    let head = &rows[start..question];
    let kind_row = head.iter().position(|row| !row.trim().is_empty());
    let indent = kind_row.map_or(0, |index| indent_of(&head[index]));
    let after_kind = kind_row.map_or(head.len(), |index| index + 1);
    let subject_row = (after_kind..head.len()).find(|&index| {
        let text = head[index].trim();
        !text.is_empty() && !text.starts_with("Tip:")
    });
    let subject = subject_row.map_or_else(String::new, |index| head[index].trim().to_owned());
    let content_from = subject_row.map_or(after_kind, |index| index + 1);
    let content: Vec<&str> = head[content_from..]
        .iter()
        .map(String::as_str)
        .filter(|row| is_permission_content(row))
        .collect();
    let always_allow_tip = options.iter().find_map(|option| {
        ALWAYS_ALLOW
            .iter()
            .find_map(|prefix| option.label.strip_prefix(prefix))
            .map(str::to_owned)
    });
    Some(Parsed::Prompt(ChatPrompt {
        can_amend: rows[footer].contains("Tab to amend"),
        always_allow_tip,
        ..prompt(
            ChatPromptKind::Permission,
            subject,
            text_block(&content, indent),
            options,
        )
    }))
}

/// Whether a row between a permission dialog's subject and its question is
/// part of what the tool would do, rather than a box rule, an advert or the
/// approval notice.
fn is_permission_content(row: &str) -> bool {
    let text = row.trim();
    !(is_box_rule(row) || text.starts_with("Tip:") || text == "This command requires approval")
}

/// A plan approval: the plan's box, the `Would you like to proceed?` row,
/// the options under it, then the `ctrl+g to edit in Notepad` footer.
fn parse_plan(rows: &[String], start: usize) -> Option<Parsed> {
    let proceed = find(rows, start, |row| {
        row.contains("Would you like to proceed?")
    });
    let ready = find(rows, start, |row| row.trim() == "Ready to code?");
    if proceed.is_none() && ready.is_none() {
        return None;
    }
    let Some(proceed) = proceed else {
        return Some(Parsed::HalfDrawn);
    };
    let Some(footer) = find(rows, proceed + 1, |row| {
        row.contains("ctrl+g to edit in Notepad")
    }) else {
        return Some(Parsed::HalfDrawn);
    };
    let options = collect_options(&rows[proceed + 1..footer], |row| {
        row.trim_start().starts_with("shift+tab to approve")
    });
    if options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    let plan = plan_rows(&rows[..proceed]);
    let indent = plan
        .iter()
        .filter(|row| !row.trim().is_empty())
        .map(|row| indent_of(row))
        .min()
        .unwrap_or(0);
    let subject = plan
        .iter()
        .map(|row| row.trim())
        .find(|text| !text.is_empty())
        .unwrap_or_default()
        .to_owned();
    Some(Parsed::Prompt(prompt(
        ChatPromptKind::Plan,
        subject,
        text_block(&plan, indent),
        options,
    )))
}

/// The rows inside the plan's `╌` box, above `rows`' end. A box whose top
/// rule scrolled off starts at the top of the screen, or under the
/// `Here is Claude's plan:` row when that is still drawn.
fn plan_rows(rows: &[String]) -> Vec<&str> {
    let from = rows
        .iter()
        .rposition(|row| row.trim() == "Here is Claude's plan:")
        .map_or(0, |index| index + 1);
    let rules: Vec<usize> = (from..rows.len())
        .filter(|&index| is_box_rule(&rows[index]))
        .collect();
    let (open, close) = match rules[..] {
        [] => return Vec::new(),
        [close] => (from, close),
        [open, .., close] => (open + 1, close),
    };
    rows[open..close].iter().map(String::as_str).collect()
}

/// A folder trust dialog: `Accessing workspace:` and its path, the notice,
/// the options (numbered or not) and the `Enter to confirm · Esc to cancel`
/// footer.
fn parse_trust(rows: &[String], start: usize) -> Option<Parsed> {
    let anchor = find(rows, start, |row| row.trim() == "Accessing workspace:");
    let footer = find(rows, start, |row| {
        row.contains("Enter to confirm · Esc to cancel")
    });
    let trust_row = find(rows, start, |row| row.contains("Yes, I trust this folder"));
    if anchor.is_none() && (trust_row.is_none() || footer.is_none()) {
        return None;
    }
    let Some(footer) = footer else {
        return Some(Parsed::HalfDrawn);
    };
    let (block_start, block_end) = options_block(rows, start, footer);
    let options = trust_options(&rows[block_start..block_end]);
    if options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    let (subject, body) = trust_text(rows, anchor, block_start);
    Some(Parsed::Prompt(prompt(
        ChatPromptKind::Trust,
        subject,
        body,
        options,
    )))
}

/// The row that opens a new-MCP-server dialog, the server's name after it.
const MCP_SERVER_ANCHOR: &str = "New MCP server found in this project:";

/// A new-MCP-server dialog: `New MCP server found in this project:` and
/// the server's name, the notice, the unnumbered options and the `Enter to
/// confirm · Esc to cancel` footer.
fn parse_mcp_server(rows: &[String], start: usize) -> Option<Parsed> {
    let anchor = find(rows, start, |row| {
        row.trim_start().starts_with(MCP_SERVER_ANCHOR)
    })?;
    let Some(footer) = find(rows, anchor + 1, |row| {
        row.contains("Enter to confirm · Esc to cancel")
    }) else {
        return Some(Parsed::HalfDrawn);
    };
    let (block_start, block_end) = options_block(rows, anchor + 1, footer);
    let options = trust_options(&rows[block_start..block_end]);
    if options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    let name = rows[anchor]
        .trim()
        .strip_prefix(MCP_SERVER_ANCHOR)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let notice: Vec<&str> = rows[anchor + 1..block_start]
        .iter()
        .map(String::as_str)
        .collect();
    Some(Parsed::Prompt(prompt(
        ChatPromptKind::McpServer,
        name,
        text_block(&notice, indent_of(&rows[anchor])),
        options,
    )))
}

/// The title row of the `/model` picker, which is also its subject.
const PICKER_TITLE: &str = "Select model";

/// How many rows under the picker's title its first model row may sit.
const PICKER_GAP: usize = 6;

/// The label of the picker's row that opens the longer model list.
const MORE_MODELS: &str = "More models";

/// The mark before the More models row.
const MORE_MARK: char = '▸';

/// The marks a scrolled picker draws before a row's number.
const SCROLL_MARKS: [char; 2] = ['↑', '↓'];

/// The mark after the name of the model in use.
const CURRENT_TICK: char = '✔';

/// The start of the description of the model in use.
const CURRENT: &str = "Current";

/// The `/model` picker: the `Select model` title, then its numbered model
/// rows and the `▸ More models` row. Each row's name is its label and its
/// second column its description; the effort row's label is the prompt's
/// `adjust`, which with the footer stays out of its identity, so moving the
/// cursor or the effort keeps the prompt.
fn parse_model_picker(rows: &[String]) -> Option<ChatPrompt> {
    let floor = after_last_rule(rows).max(dialog_edge(rows));
    let title = floor
        + rows
            .get(floor..)?
            .iter()
            .rposition(|row| row.trim() == PICKER_TITLE)?;
    let first = (title + 1..rows.len().min(title + 1 + PICKER_GAP))
        .find(|&index| picker_row(&rows[index]).is_some())?;
    let listed = rows[first..]
        .iter()
        .take_while(|row| !row.trim().is_empty())
        .count();
    let mut options: Vec<ChatOption> = Vec::new();
    for row in &rows[first..first + listed] {
        if let Some(option) = picker_row(row) {
            options.push(option);
        } else if let Some(selected) = more_models_row(row) {
            let number = options.last().map_or(1, |last| last.number + 1);
            options.push(ChatOption {
                number,
                label: MORE_MODELS.to_owned(),
                description: None,
                selected,
            });
        }
    }
    Some(ChatPrompt {
        adjust: effort_label(&rows[first + listed..]),
        ..prompt(
            ChatPromptKind::ModelPicker,
            PICKER_TITLE.to_owned(),
            String::new(),
            options,
        )
    })
}

/// The marks the picker's effort row starts with, one per level.
const EFFORT_MARKS: [char; 5] = ['○', '◐', '●', '◉', '◈'];

/// The hint that ends the picker's effort row.
const ADJUST_HINT: &str = "←/→ to adjust";

/// The label of the first effort row (`● High effort ←/→ to adjust`) in
/// `rows`, its mark and hint taken off: `High effort`; none without one.
fn effort_label(rows: &[String]) -> Option<String> {
    rows.iter().find_map(|row| {
        let text = row.trim().strip_suffix(ADJUST_HINT)?;
        let label = text.trim_start_matches(EFFORT_MARKS).trim();
        (!label.is_empty()).then(|| label.to_owned())
    })
}

/// The mark a full-screen dialog's top edge is drawn with.
const DIALOG_EDGE: char = '▔';

/// One past the last row drawn as a dialog's `▔` top edge; 0 when none is.
fn dialog_edge(rows: &[String]) -> usize {
    rows.iter()
        .rposition(|row| row.trim_start().starts_with(DIALOG_EDGE))
        .map_or(0, |index| index + 1)
}

/// `row` as a numbered picker row: the model's name (its tick taken off),
/// and its second column, marked `Current` on the ticked row.
fn picker_row(row: &str) -> Option<ChatOption> {
    let text = row.trim_start();
    let (selected, text) = match text.strip_prefix(CURSOR) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let text = text.trim_start_matches(SCROLL_MARKS).trim_start();
    let option = numbered(text)?;
    let (name, column) = option.label.split_once("  ").unwrap_or((option.label, ""));
    let name = name.trim();
    let column = column.trim();
    let (label, current) = match name.strip_suffix(CURRENT_TICK) {
        Some(rest) => (rest.trim_end(), true),
        None => (name, false),
    };
    let description = match (current, column.is_empty()) {
        (true, true) => Some(CURRENT.to_owned()),
        (true, false) => Some(format!("{CURRENT} · {column}")),
        (false, true) => None,
        (false, false) => Some(column.to_owned()),
    };
    Some(ChatOption {
        number: option.number,
        label: label.to_owned(),
        description,
        selected,
    })
}

/// Whether `row` is the picker's `▸ More models` row, and whether the
/// cursor is on it; `None` for any other row.
fn more_models_row(row: &str) -> Option<bool> {
    let text = row.trim_start();
    let (selected, text) = match text.strip_prefix(CURSOR) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    (text.strip_prefix(MORE_MARK)?.trim() == MORE_MODELS).then_some(selected)
}

/// The title row of the dialog that asks before switching a conversation's
/// model.
const SWITCH_MODEL_TITLE: &str = "Switch model?";

/// The Switch model? dialog the picker's `s` opens after a turn: the title,
/// its explanation, the two options and the `Enter to confirm · Esc to
/// cancel` footer.
fn parse_switch_model(rows: &[String], start: usize) -> Option<Parsed> {
    let anchor = (start..rows.len())
        .rev()
        .find(|&index| rows[index].trim() == SWITCH_MODEL_TITLE)?;
    let Some(footer) = find(rows, anchor + 1, |row| {
        row.contains("Enter to confirm · Esc to cancel")
    }) else {
        return Some(Parsed::HalfDrawn);
    };
    let (block_start, block_end) = options_block(rows, anchor + 1, footer);
    let options = trust_options(&rows[block_start..block_end]);
    if options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    let explanation: Vec<&str> = rows[anchor + 1..block_start]
        .iter()
        .map(String::as_str)
        .collect();
    Some(Parsed::Prompt(prompt(
        ChatPromptKind::Confirm,
        SWITCH_MODEL_TITLE.to_owned(),
        text_block(&explanation, indent_of(&rows[anchor])),
        options,
    )))
}

/// The bounds of the options block over a dialog's `footer`: the run of
/// non-blank rows that ends nearest above it, starting no higher than
/// `from`.
fn options_block(rows: &[String], from: usize, footer: usize) -> (usize, usize) {
    let block_end = (from..footer)
        .rev()
        .find(|&index| !rows[index].trim().is_empty())
        .map_or(footer, |index| index + 1);
    let block_start = (from..block_end)
        .rev()
        .find(|&index| rows[index].trim().is_empty())
        .map_or(from, |index| index + 1);
    (block_start, block_end)
}

/// The options of a trust or new-MCP-server dialog: a printed number when
/// the build numbers them, else the row's 1-based position.
fn trust_options(rows: &[String]) -> Vec<ChatOption> {
    rows.iter()
        .zip(1..)
        .map(|(row, position)| {
            if let Some(option) = numbered(row) {
                return ChatOption {
                    number: option.number,
                    label: option.label.to_owned(),
                    description: None,
                    selected: option.selected,
                };
            }
            let text = row.trim();
            let label = text.strip_prefix(CURSOR);
            ChatOption {
                number: position,
                label: label.unwrap_or(text).trim().to_owned(),
                description: None,
                selected: label.is_some(),
            }
        })
        .collect()
}

/// The trust dialog's subject (the folder's path) and body (the notice),
/// from the rows between `Accessing workspace:` and the options.
fn trust_text(rows: &[String], anchor: Option<usize>, options_start: usize) -> (String, String) {
    let Some(anchor) = anchor else {
        return (String::new(), String::new());
    };
    let path = find(rows, anchor + 1, |row| !row.trim().is_empty()).filter(|&i| i < options_start);
    let Some(path) = path else {
        return (String::new(), String::new());
    };
    let notice: Vec<&str> = rows[path + 1..options_start]
        .iter()
        .map(String::as_str)
        .collect();
    let indent = indent_of(&rows[path]);
    (rows[path].trim().to_owned(), text_block(&notice, indent))
}

/// Where a Heads up notice starts, and the keys its own row carries.
enum HeadsUpAnchor {
    /// `✦ Heads up · …` or `✦ You should know · …`: its key row follows.
    Tagged,
    /// The `✦ Dismissed.` step the notice leads to, its keys on its own row.
    Dismissed(Vec<ChatOption>),
}

/// The lowest row a Heads up notice or its dismissed step starts on, so a
/// quoted notice higher in the transcript never beats a real one.
fn heads_up_anchor(rows: &[String]) -> Option<(usize, HeadsUpAnchor)> {
    rows.iter().enumerate().rev().find_map(|(anchor, row)| {
        let trimmed = row.trim();
        if trimmed.contains("✦ Heads up ·") || trimmed.contains("✦ You should know ·") {
            return Some((anchor, HeadsUpAnchor::Tagged));
        }
        let keys = key_pairs_any(trimmed.strip_prefix("✦ Dismissed.")?)?;
        Some((anchor, HeadsUpAnchor::Dismissed(keys)))
    })
}

/// A Heads up notice over the input box: the `✦ Heads up · …` (or
/// `✦ You should know · …`) row and its wrapped rows, the expanded title and
/// bullets when shown, and the key row of `N: label` pairs; or the
/// `✦ Dismissed.` step the notice leads to, its keys on its own row and the
/// rows it wraps onto.
fn parse_heads_up(rows: &[String]) -> Option<ChatPrompt> {
    let (anchor, found) = heads_up_anchor(rows)?;
    if let HeadsUpAnchor::Dismissed(options) = found {
        return Some(heads_up_prompt(
            rows,
            anchor,
            options,
            protocol::HEADS_UP_DISMISSED_SUBJECT.to_owned(),
            anchor,
        ));
    }
    let (key_row, options) = (anchor + 1..rows.len())
        .find_map(|key_row| key_pairs(&rows[key_row]).map(|options| (key_row, options)))?;
    let (subject, index) = heads_up_subject(rows, anchor, key_row);
    Some(heads_up_prompt(rows, key_row, options, subject, index))
}

/// The notice `subject` with `options`, the rows of its expanded text from
/// `index` to its key row, and the key rows it wraps onto after that.
fn heads_up_prompt(
    rows: &[String],
    key_row: usize,
    mut options: Vec<ChatOption>,
    subject: String,
    index: usize,
) -> ChatPrompt {
    let mut next = key_row + 1;
    while let Some(more) = rows.get(next).and_then(|row| key_pairs_any(row)) {
        options.extend(more);
        next += 1;
    }
    let details: Vec<&str> = rows[index..key_row].iter().map(String::as_str).collect();
    prompt(
        ChatPromptKind::HeadsUp,
        subject,
        row_block(&details),
        options,
    )
}

/// The notice's title (`Heads up · …` without its `✦`), then the non-blank
/// rows under it up to `end`, and the index of the row after them.
fn heads_up_subject(rows: &[String], anchor: usize, end: usize) -> (String, usize) {
    let first = rows[anchor].trim();
    let mut subject = first.strip_prefix('✦').unwrap_or(first).trim().to_owned();
    let mut index = anchor + 1;
    while index < end && !rows[index].trim().is_empty() {
        subject.push(' ');
        subject.push_str(rows[index].trim());
        index += 1;
    }
    (subject, index)
}

/// The options a Heads up key row offers (`1: Learn more   0: Dismiss`), or
/// `None` for a row that is not one: at least two pairs, three spaces apart.
fn key_pairs(row: &str) -> Option<Vec<ChatOption>> {
    key_pairs_any(row).filter(|options| options.len() >= 2)
}

/// The `N: label` pairs in a row (`0: Dismiss`), or `None` for a row that
/// has none: pairs three spaces apart.
fn key_pairs_any(row: &str) -> Option<Vec<ChatOption>> {
    let options: Option<Vec<ChatOption>> = row
        .trim()
        .split("   ")
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let (number, label) = part.trim().split_once(": ")?;
            let label = label.trim();
            (!label.is_empty()).then_some(())?;
            Some(ChatOption {
                number: number.parse().ok()?,
                label: label.to_owned(),
                description: None,
                selected: false,
            })
        })
        .collect();
    options.filter(|options| !options.is_empty())
}

/// An `AskUserQuestion` step: the tab bar, the question row, its options
/// down to the card's inner rule, then the `Enter to select` footer; or the
/// review step when `Review your answers` follows the tab bar. A step whose
/// tab bar is not readable, scrolled off the top of the grid or wrapped at a
/// narrow width, reads through [`parse_question_without_bar`], which reads
/// the review step there from its `Ready to submit your answers?` row.
fn parse_question(rows: &[String]) -> Option<Parsed> {
    let rules = rule_indices(rows);
    let Some(bar) = (0..rows.len())
        .rev()
        .find(|&index| is_tab_bar(rows, &rules, index))
    else {
        return parse_question_without_bar(rows);
    };
    let mut tabs = question_tabs(&rows[bar]);
    let title = find(rows, bar + 1, |row| !row.trim().is_empty());
    if let Some(title) = title
        && rows[title].trim() == REVIEW_TITLE
    {
        return parse_review(rows, title, tabs);
    }
    let Some(footer) = find(rows, bar + 1, |row| question_footer_body(row).is_some()) else {
        return Some(Parsed::HalfDrawn);
    };
    let Some(title) = title.filter(|&title| title < footer) else {
        return Some(Parsed::HalfDrawn);
    };
    let end = rules
        .into_iter()
        .find(|&index| index > title && index < footer)
        .unwrap_or(footer);
    let first_option = (title + 1..end)
        .find(|&index| numbered(&rows[index]).is_some())
        .unwrap_or(end);
    let width = dialog_width(rows, bar, footer);
    let found = question_options(&rows[first_option..end], width);
    if found.options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    if let Some(tab) = tabs.iter_mut().find(|tab| !tab.answered) {
        tab.current = true;
    }
    Some(Parsed::Prompt(ChatPrompt {
        questions: tabs,
        multi: found.multi,
        cursor: found.cursor,
        ..prompt(
            ChatPromptKind::Question,
            question_text(&rows[title..first_option], width),
            String::new(),
            found.options,
        )
    }))
}

/// A question step whose tab bar is not readable, scrolled off the top of
/// the grid or wrapped at a narrow width: its question and numbered options
/// read by [`question_without_bar`]; failing that, the review step, which
/// draws no `Enter to select` footer. Neither read has tabs, which is what
/// tells it apart from a dialog read with its bar.
fn parse_question_without_bar(rows: &[String]) -> Option<Parsed> {
    question_without_bar(rows).or_else(|| review_without_bar(rows))
}

/// A question step whose tab bar is not readable: its numbered options, a
/// single question's footer and the `Chat about this` option every such step
/// draws under its inner rule in view, the question's rows from the top of
/// the dialog's visible part down to its first option.
fn question_without_bar(rows: &[String]) -> Option<Parsed> {
    let footer = single_question_footer(rows)?;
    let first_option = (0..footer).find(|&index| numbered(&rows[index]).is_some())?;
    let end = option_end(rows, first_option, footer);
    if !has_chat_about_this(rows, end, footer) {
        return None;
    }
    let start = question_start(rows, first_option);
    let width = dialog_width(rows, start, footer);
    let found = question_options(&rows[first_option..end], width);
    if found.options.is_empty() {
        return Some(Parsed::HalfDrawn);
    }
    Some(Parsed::Prompt(ChatPrompt {
        questions: Vec::new(),
        multi: found.multi,
        cursor: found.cursor,
        ..prompt(
            ChatPromptKind::Question,
            question_text(&rows[start..first_option], width),
            String::new(),
            found.options,
        )
    }))
}

/// The row a barless dialog's footer starts on, when one shows: `Enter to
/// select … Esc to cancel`, naming neither `Tab` (a multi-question card's
/// footer) nor another Esc action (a select list's `Esc to go back`).
fn single_question_footer(rows: &[String]) -> Option<usize> {
    (0..rows.len())
        .rev()
        .find(|&index| is_single_question_footer(rows, index) && footer_is_last(rows, index))
}

/// A footer's whole text from `index`: the row itself and every non-blank
/// row directly under it, up to the first blank row or the end of the grid,
/// each trimmed and joined with one space, since a narrow pane wraps the
/// footer onto a second row.
fn footer_text(rows: &[String], index: usize) -> String {
    let mut text = String::new();
    for row in &rows[index..] {
        let piece = row.trim();
        if piece.is_empty() {
            break;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(piece);
    }
    text
}

/// Whether the footer starting on `rows[index]` is a single question's:
/// `Enter to select …` naming `Esc to cancel`, with no `Tab` in it, whether
/// it shows on one row or wraps onto the row under it. Headed by the vim
/// mode's `-- INSERT --`, it names no Esc at all.
fn is_single_question_footer(rows: &[String], index: usize) -> bool {
    let text = footer_text(rows, index);
    let Some(body) = question_footer_body(&text) else {
        return false;
    };
    let vim_insert = body.len() < text.len();
    let esc_cancels = body.contains("Esc to cancel") || (vim_insert && !body.contains("Esc to"));
    body.starts_with(QUESTION_FOOTER) && esc_cancels && !body.contains("Tab")
}

/// `footer` without the `-- INSERT -- · ` that heads a question's footer in
/// the vim editor mode while the cursor sits on `Type something`.
fn without_vim_insert(footer: &str) -> &str {
    footer.strip_prefix(VIM_INSERT_HEAD).unwrap_or(footer)
}

/// The body of the question footer `row` heads, its vim marker dropped:
/// `Some` when the row opens the footer on its own, or — headed by the marker
/// in a narrow pane — ends at `Enter to` or `Enter to select`, its tail on
/// the rows under it.
fn question_footer_body(row: &str) -> Option<&str> {
    let row = row.trim_start();
    let body = without_vim_insert(row);
    let vim_insert = body.len() < row.len();
    let opens = body.starts_with(QUESTION_FOOTER);
    (opens || (vim_insert && body.trim_end() == ENTER_TO)).then_some(body)
}

/// Whether nothing but the footer's own wrapped tail, blank rows and rules
/// sit under `footer`: an input box's `❯` row, a bash-mode box's `!` row or a
/// numbered list below it means the footer was quoted in the transcript, not
/// drawn by a dialog.
fn footer_is_last(rows: &[String], footer: usize) -> bool {
    rows[footer + 1..].iter().all(|row| {
        let text = row.trim();
        !text.starts_with(CURSOR) && !text.starts_with(BASH_MODE) && numbered(row).is_none()
    })
}

/// The row a barless dialog's options end on: the first rule after
/// `first_option`, or its footer when no rule shows.
fn option_end(rows: &[String], first_option: usize, footer: usize) -> usize {
    rule_indices(rows)
        .into_iter()
        .find(|&index| index > first_option && index < footer)
        .unwrap_or(footer)
}

/// Whether the rows between a barless dialog's inner rule and its footer
/// hold the numbered `Chat about this` option every `AskUserQuestion` step
/// draws there: a footer quoted in the transcript has none.
fn has_chat_about_this(rows: &[String], end: usize, footer: usize) -> bool {
    (end..footer)
        .any(|index| numbered(&rows[index]).is_some_and(|option| option.label == CHAT_ABOUT_THIS))
}

/// Where a barless dialog's visible part starts: its first `│`-gutter row,
/// else the row after a tab bar still drawn at the top of the grid (its
/// rule having scrolled off), else the grid's first row.
fn question_start(rows: &[String], first_option: usize) -> usize {
    let top = (0..first_option).find(|&index| !rows[index].trim().is_empty());
    let bar = top.filter(|&index| rows[index].trim_start().starts_with([TAB_OPEN, TAB_DONE]));
    let from = bar.map_or(0, |index| index + 1);
    (from..first_option)
        .find(|&index| rows[index].trim_start().starts_with('│'))
        .unwrap_or(from)
}

/// A question wrapped over `rows` (from its title to its first option),
/// each row trimmed, its `│` gutter (a long question's left bar) and the
/// space after it taken off, and the blank ones left out, joined as
/// [`push_row`] joins rows in a dialog `width` columns wide.
fn question_text(rows: &[String], width: usize) -> String {
    let mut text = String::new();
    let mut upper = "";
    for row in rows {
        let trimmed = row.trim();
        let line = trimmed.strip_prefix('│').map_or(trimmed, str::trim_start);
        if line.is_empty() {
            upper = "";
            continue;
        }
        push_row(&mut text, upper, line, width);
        upper = row;
    }
    text
}

/// The width of the dialog whose rows run from `top` to `bottom` (the
/// footer, which is left out): its widest row in cells, trailing spaces
/// aside, from the last rule at or above `top` (a dialog's rules span its
/// width) down.
fn dialog_width(rows: &[String], top: usize, bottom: usize) -> usize {
    let from = rule_indices(rows)
        .into_iter()
        .rev()
        .find(|&index| index <= top)
        .unwrap_or(top);
    rows.get(from..bottom.min(rows.len()))
        .unwrap_or_default()
        .iter()
        .map(|row| row.trim_end().chars().count())
        .max()
        .unwrap_or(0)
}

/// Append `text`, the row under the drawn row `upper`, to `target`: right
/// after it when `upper` ends a path or web address hard-wrapped at the
/// dialog's `width` ([`wraps_mid_token`]), else after one space; with no
/// space in front of the first text.
fn push_row(target: &mut String, upper: &str, text: &str, width: usize) {
    if !target.is_empty() && !wraps_mid_token(upper, text, width) {
        target.push(' ');
    }
    target.push_str(text);
}

/// Whether the drawn row `upper` and the `next` row's text are one path or
/// web address split where the dialog `width` columns wide ran out: `upper`
/// fills the width (within one cell), its last token and `next`'s first word
/// are the two pieces of one ([`split_a_token`]), `next` does not open a list
/// item, and that token with `next`'s first word is longer than the row's
/// text (the width less the row's indent and `│` gutter, within one cell):
/// the agent's wrap splits a word only when it outgrows a whole row, and
/// moves a shorter one whole.
fn wraps_mid_token(upper: &str, next: &str, width: usize) -> bool {
    let upper = upper.trim_end();
    let drawn = upper.chars().count();
    if drawn + 1 < width || opens_a_list(next) {
        return false;
    }
    let (Some(last), Some(first)) = (
        upper.split_whitespace().last(),
        next.split_whitespace().next(),
    ) else {
        return false;
    };
    if !split_a_token(last, first) {
        return false;
    }
    let text = upper.trim_start();
    let text = text.strip_prefix('│').map_or(text, str::trim_start);
    let indent = drawn - text.chars().count();
    last.chars().count() + first.chars().count() + 1 > width.saturating_sub(indent)
}

/// Whether `last`, the drawn row's last token, and `first`, the next row's
/// first word, are one path or web address split across the rows: `last` is
/// one ([`is_path_token`]) or ends a web address's `:/`; else `last` holds a
/// single `/`, `first` carries one too, and `first` does not open a path or
/// address of its own.
fn split_a_token(last: &str, first: &str) -> bool {
    if is_path_token(last) || last.ends_with(":/") {
        return true;
    }
    last.matches('/').count() == 1 && first.contains('/') && !opens_a_path(first)
}

/// Whether `token` reads as a path or web address: after a leading quote or
/// bracket goes, a drive (`C:`) at its start, a `\`, a `://`, a leading `/`,
/// `./`, `../` or `~/`, or two `/`, and it does not end in `.`, `,` or `)`,
/// which end a sentence or an aside instead. A single `/` between two words
/// ("and/or") is prose.
fn is_path_token(token: &str) -> bool {
    if token.ends_with(['.', ',', ')']) {
        return false;
    }
    let bare = token.trim_start_matches(['\'', '"', '`', '(', '[', '<']);
    has_drive(bare)
        || token.contains('\\')
        || token.contains("://")
        || opens_with_a_slash(bare)
        || token.matches('/').count() >= 2
}

/// Whether `token` opens a path or web address: a drive (`C:`) at its start,
/// a `://`, or a leading `/`, `./`, `../` or `~/`.
fn opens_a_path(token: &str) -> bool {
    let bare = token.trim_start_matches(['\'', '"', '`', '(', '[', '<']);
    has_drive(bare) || token.contains("://") || opens_with_a_slash(bare)
}

/// Whether `bare` starts with a drive (`C:`), alone or before a separator.
fn has_drive(bare: &str) -> bool {
    let mut chars = bare.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && chars.next().is_none_or(|c| matches!(c, '\\' | '/'))
}

/// Whether `bare` opens with `/`, `./`, `../` or `~/`.
fn opens_with_a_slash(bare: &str) -> bool {
    ["/", "./", "../", "~/"]
        .iter()
        .any(|prefix| bare.starts_with(prefix))
}

/// Whether `text` opens with a bullet (`-`, `*`, `•`, `·`) or a number
/// (`1.`, `2)`), so it starts an item rather than continuing a token.
fn opens_a_list(text: &str) -> bool {
    let Some(first) = text.split_whitespace().next() else {
        return false;
    };
    if matches!(first, "-" | "*" | "•" | "·") {
        return true;
    }
    first
        .strip_suffix(['.', ')'])
        .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

/// Whether `rows[index]` is a question card's tab bar: `←  ☐ Next fn  ✔
/// Submit  →`, or a single question's bar (` ☐ Colour`), which has no
/// arrows and sits directly under a rule, or under the `──` tail a rule
/// wider than the grid wraps onto the row below it (a dialog drawn
/// indented with a full-width rule).
fn is_tab_bar(rows: &[String], rules: &[usize], index: usize) -> bool {
    let text = rows[index].trim();
    if text.starts_with('←') && text.ends_with('→') {
        return text.contains([TAB_OPEN, TAB_DONE]);
    }
    let under_rule = index.checked_sub(1).is_some_and(|above| {
        rules.contains(&above)
            || (is_rule_tail(&rows[above])
                && above
                    .checked_sub(1)
                    .is_some_and(|rule| rules.contains(&rule)))
    });
    under_rule && text.starts_with([TAB_OPEN, TAB_DONE]) && !text.contains(['←', '→'])
}

/// The question tabs of a tab bar, each label the text after its mark; the
/// fixed `Submit` tab is left out.
fn question_tabs(row: &str) -> Vec<ChatQuestionTab> {
    let marks: Vec<(usize, char)> = row
        .char_indices()
        .filter(|&(_, c)| matches!(c, TAB_OPEN | TAB_DONE | TAB_SUBMIT))
        .collect();
    let ends = marks.iter().skip(1).map(|&(at, _)| at);
    let ends = ends.chain(Some(row.rfind('→').unwrap_or(row.len())));
    marks
        .iter()
        .zip(ends)
        .filter(|&(&(_, mark), _)| mark != TAB_SUBMIT)
        .map(|(&(at, mark), end)| ChatQuestionTab {
            label: row[at + mark.len_utf8()..end].trim().to_owned(),
            answered: mark == TAB_DONE,
            current: false,
        })
        .collect()
}

/// A question step's options, whether they carry boxes, and the option the
/// cursor is on.
struct QuestionOptions {
    options: Vec<ChatOption>,
    multi: bool,
    cursor: Option<u32>,
}

/// The options among a question step's `rows`: each numbered row's label
/// (its box taken off, `Type something.` without its period), the deeper
/// indented rows under it joined as its description ([`push_row`], in a
/// dialog `width` columns wide). `selected` is the tick on a multi-select
/// question and the cursor otherwise. The `Next` row and `Chat about this`
/// are not options.
fn question_options(rows: &[String], width: usize) -> QuestionOptions {
    let mut found = QuestionOptions {
        options: Vec::new(),
        multi: false,
        cursor: None,
    };
    let mut indent = 0;
    let mut upper = "";
    for row in rows {
        if let Some(option) = numbered(row) {
            indent = option.indent;
            upper = "";
            let (ticked, label) = checkbox(option.label);
            found.multi |= ticked.is_some();
            if option.selected {
                found.cursor = Some(option.number);
            }
            let label = match label.strip_suffix('.') {
                Some(TYPE_SOMETHING) => TYPE_SOMETHING,
                _ => label,
            };
            found.options.push(ChatOption {
                number: option.number,
                label: label.to_owned(),
                description: None,
                selected: ticked.unwrap_or(option.selected),
            });
            continue;
        }
        let text = row.trim();
        let unmarked = text.strip_prefix(CURSOR).unwrap_or(text).trim();
        if text.is_empty() || unmarked == NEXT_ROW || indent_of(row) <= indent {
            upper = "";
            continue;
        }
        if let Some(last) = found.options.last_mut() {
            let description = last.description.get_or_insert_with(String::new);
            push_row(description, upper, text, width);
            upper = row;
        }
    }
    found
        .options
        .retain(|option| option.label != CHAT_ABOUT_THIS);
    found
}

/// `label` without a multi-select box, and whether the box is ticked;
/// `None` for a label with no box.
fn checkbox(label: &str) -> (Option<bool>, &str) {
    if let Some(rest) = label.strip_prefix(BOX_ON) {
        return (Some(true), rest.trim_start());
    }
    if let Some(rest) = label.strip_prefix(BOX_OFF) {
        return (Some(false), rest.trim_start());
    }
    (None, label)
}

/// Where a barless review's questions and answers end and its options start:
/// the last `Ready to submit your answers?` row drawn as the review step's
/// own and the row under it ([`ready_bounds`]), when one shows, else the row
/// of its own `1. Submit answers` option, which Claude Code 2.1.330 draws
/// with no ready row above it. A ready row quoted in a reply above the live
/// step fails its bounds, and the option row below it is the one read.
///
/// The option row is the step's own under the same end check
/// ([`options_are_last`]), with `→` answer rows above it.
fn review_ready(rows: &[String]) -> Option<(usize, usize)> {
    let ready = (0..rows.len())
        .rev()
        .find(|&index| rows[index].trim() == REVIEW_READY);
    if let Some(bounds) = ready.and_then(|ready| ready_bounds(rows, ready)) {
        return Some(bounds);
    }
    let option = (0..rows.len()).rev().find(|&index| {
        numbered(&rows[index]).is_some_and(|row| row.number == 1 && row.label == SUBMIT_ANSWERS)
    })?;
    let answered = rows[..option].iter().any(|row| row.trim().starts_with('→'));
    (answered && options_are_last(rows, option)).then_some((option, option))
}

/// The bounds a review anchored on its own `Ready to submit your answers?`
/// row at `ready` reads as: that row and the row under it, when `Submit
/// answers` is the first non-blank row after it, at most two rows below, and
/// its options end the screen. A row quoted in a reply has the input box's
/// `❯` row or a numbered list under its options instead.
fn ready_bounds(rows: &[String], ready: usize) -> Option<(usize, usize)> {
    let first = find(rows, ready + 1, |row| !row.trim().is_empty()).filter(|&i| i <= ready + 2)?;
    if numbered(&rows[first])?.label != SUBMIT_ANSWERS {
        return None;
    }
    options_are_last(rows, first).then_some((ready, ready + 1))
}

/// Whether the options starting at `first` end with nothing under them but
/// blank rows, rules and a footer's wrapped tail.
fn options_are_last(rows: &[String], first: usize) -> bool {
    let end = find(rows, first, |row| row.trim().is_empty()).unwrap_or(rows.len());
    footer_is_last(rows, end - 1)
}

/// The review step read with no readable tab bar, from the row its questions
/// and answers start on ([`review_start`]) down to its options, with no tabs.
fn review_without_bar(rows: &[String]) -> Option<Parsed> {
    let (body_end, options_start) = review_ready(rows)?;
    let start = review_start(rows, body_end);
    Some(review(rows, start..body_end, options_start, Vec::new()))
}

/// Where a barless review's questions and answers start: the row under the
/// `Review your answers` title above `ready` when a wrapped bar leaves it on
/// screen, else the row under the last rule above it, else the grid's first
/// row when the whole dialog has scrolled off the top.
fn review_start(rows: &[String], ready: usize) -> usize {
    let title = (0..ready)
        .rev()
        .find(|&index| rows[index].trim() == REVIEW_TITLE);
    if let Some(title) = title {
        return title + 1;
    }
    rule_indices(rows)
        .into_iter()
        .rev()
        .find(|&index| index < ready)
        .map_or(0, |rule| rule + 1)
}

/// The review step read with its tab bar: the questions and answers under
/// `title`, then its options, which Claude Code 2.1.307 heads with a
/// `Ready to submit your answers?` row and 2.1.330 draws directly under the
/// answers. `None` for a step quoted in a reply, its options not ending the
/// screen: `HalfDrawn` is a live frame whose options are not drawn yet.
fn parse_review(rows: &[String], title: usize, tabs: Vec<ChatQuestionTab>) -> Option<Parsed> {
    let body_start = title + 1;
    let bounds = find(rows, body_start, |row| row.trim() == REVIEW_READY)
        .and_then(|ready| ready_bounds(rows, ready))
        .or_else(|| submit_option(rows, body_start).map(|option| (option, option)));
    let Some((body_end, options_start)) = bounds else {
        let numbered_below = find(rows, body_start, |row| numbered(row).is_some()).is_some();
        return (!numbered_below).then_some(Parsed::HalfDrawn);
    };
    Some(review(rows, body_start..body_end, options_start, tabs))
}

/// The row of the review's `Submit answers` option at or after `from` whose
/// options end the screen: the lowest one, so a reply above the live step
/// quoting another does not answer for it.
fn submit_option(rows: &[String], from: usize) -> Option<usize> {
    (from..rows.len()).rev().find(|&index| {
        numbered(&rows[index]).is_some_and(|option| option.label == SUBMIT_ANSWERS)
            && options_are_last(rows, index)
    })
}

/// The review step's prompt: the questions and answers in `rows[body]`, then
/// its numbered options from `options_start` on, which end at the first
/// blank row. `body` starts at the row under the title for a review read
/// with its bar, and at [`review_start`]'s for one read without it.
fn review(
    rows: &[String],
    body: Range<usize>,
    options_start: usize,
    tabs: Vec<ChatQuestionTab>,
) -> Parsed {
    let Some(first) = find(rows, options_start, |row| numbered(row).is_some()) else {
        return Parsed::HalfDrawn;
    };
    let end = find(rows, first, |row| row.trim().is_empty()).unwrap_or(rows.len());
    let options = collect_options(&rows[first..end], |_| false);
    let width = dialog_width(rows, body.start, end);
    Parsed::Prompt(ChatPrompt {
        questions: tabs,
        ..prompt(
            ChatPromptKind::Review,
            REVIEW_TITLE.to_owned(),
            review_body(&rows[body], width),
            options,
        )
    })
}

/// The review's pairs as lines `<question> -> <answer>`: a `●` row starts
/// a question, a `→` row holds its answer, and any other row continues the
/// one before it ([`push_row`], in a dialog `width` columns wide). A long
/// question's rows carry a `│` gutter, taken off as [`question_text`] takes
/// it.
fn review_body(rows: &[String], width: usize) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut upper = "";
    for row in rows {
        let text = row.trim();
        let text = text.strip_prefix('│').map_or(text, str::trim_start);
        if let Some(question) = text.strip_prefix('●') {
            pairs.push((question.trim().to_owned(), String::new()));
        } else if let Some((question, answer)) = pairs.last_mut() {
            if let Some(given) = text.strip_prefix('→') {
                given.trim().clone_into(answer);
            } else if !text.is_empty() {
                let target = if answer.is_empty() { question } else { answer };
                push_row(target, upper, text, width);
            }
        }
        upper = if text.is_empty() { "" } else { row };
    }
    let lines: Vec<String> = pairs
        .iter()
        .map(|(question, answer)| format!("{question} -> {answer}"))
        .collect();
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    text_block(&lines, 0)
}

/// The input box's state: the rows between the last two rules. Text after
/// the prompt (other than a whole-line `Try "…"` placeholder or an agent's
/// `Message @…` one), or more than one row, is a draft; an empty box is working while a spinner shows above
/// it.
fn composer(rows: &[String]) -> ComposerState {
    let Some((top, bottom)) = box_rules(rows) else {
        return ComposerState::Unknown;
    };
    let body: Vec<&str> = rows[top + 1..bottom]
        .iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty())
        .collect();
    let Some(first) = body.first() else {
        return ComposerState::Unknown;
    };
    let typed = if let Some(rest) = first.strip_prefix(CURSOR) {
        let rest = rest.trim();
        !rest.is_empty() && !is_try_placeholder(rest) && !is_agent_placeholder(rest)
    } else if let Some(rest) = first.strip_prefix(BASH_MODE) {
        !rest.trim().is_empty()
    } else {
        return ComposerState::Unknown;
    };
    if typed || body.len() > 1 {
        ComposerState::Draft
    } else if spinner_above(&rows[..top]) {
        ComposerState::Working
    } else {
        ComposerState::Ready
    }
}

/// Whether `text`, a box's trimmed first line after the prompt, is the
/// empty box's `Try "…"` placeholder: only the whole line counts, so a draft
/// that starts the same way is text.
fn is_try_placeholder(text: &str) -> bool {
    const OPEN: &str = "Try \"";
    text.len() > OPEN.len() && text.starts_with(OPEN) && text.ends_with('"')
}

/// Whether a turn's spinner shows among the last non-empty rows of `rows`.
fn spinner_above(rows: &[String]) -> bool {
    rows.iter()
        .rev()
        .filter(|row| !row.trim().is_empty())
        .take(SPINNER_ROWS)
        .any(|row| crate::rules::is_spinner_line(row))
}

/// One numbered option row: `❯ 2. label`.
struct OptionRow<'a> {
    indent: usize,
    selected: bool,
    number: u32,
    label: &'a str,
}

/// `row` as a numbered option row, when it is one.
fn numbered(row: &str) -> Option<OptionRow<'_>> {
    let indent = indent_of(row);
    let text = row.trim_start();
    let (selected, text) = match text.strip_prefix(CURSOR) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let digits = text.find(|c: char| !c.is_ascii_digit())?;
    let number = text[..digits].parse().ok()?;
    let after = text[digits..].strip_prefix('.')?;
    let label = after.trim();
    if !after.starts_with(char::is_whitespace) || label.is_empty() {
        return None;
    }
    Some(OptionRow {
        indent,
        selected,
        number,
        label,
    })
}

/// The numbered options among `rows`, each wrapped row (indented deeper
/// than its option's number) joined to its label with one space. A row
/// `skip` names is neither an option nor part of one.
fn collect_options(rows: &[String], skip: impl Fn(&str) -> bool) -> Vec<ChatOption> {
    let mut options: Vec<ChatOption> = Vec::new();
    let mut indent = 0;
    for row in rows {
        if let Some(option) = numbered(row) {
            indent = option.indent;
            options.push(ChatOption {
                number: option.number,
                label: option.label.to_owned(),
                description: None,
                selected: option.selected,
            });
            continue;
        }
        let text = row.trim();
        if text.is_empty() || skip(row) || indent_of(row) <= indent {
            continue;
        }
        if let Some(last) = options.last_mut() {
            last.label.push(' ');
            last.label.push_str(text);
        }
    }
    options
}

/// A prompt of `kind` with its cursor taken from the selected option.
fn prompt(
    kind: ChatPromptKind,
    subject: String,
    body: String,
    options: Vec<ChatOption>,
) -> ChatPrompt {
    let cursor = options
        .iter()
        .find(|option| option.selected)
        .map(|option| option.number);
    ChatPrompt {
        id: String::new(),
        kind,
        subject,
        body,
        options,
        questions: Vec::new(),
        can_amend: false,
        always_allow_tip: None,
        multi: false,
        cursor,
        adjust: None,
    }
}

/// `rows` as one text block, its indent that of the least indented row.
fn row_block(rows: &[&str]) -> String {
    let indent = rows
        .iter()
        .filter(|row| !row.trim().is_empty())
        .map(|row| indent_of(row))
        .min()
        .unwrap_or(0);
    text_block(rows, indent)
}

/// `rows` as one text: each row with `indent` leading spaces taken off and
/// its trailing spaces trimmed, the blank rows at either end dropped, capped
/// at [`BODY_CAP`] bytes.
fn text_block(rows: &[&str], indent: usize) -> String {
    let lines: Vec<&str> = rows
        .iter()
        .map(|row| {
            let strip = indent_of(row).min(indent);
            row[strip..].trim_end()
        })
        .collect();
    let first = lines.iter().position(|line| !line.is_empty());
    let last = lines.iter().rposition(|line| !line.is_empty());
    let (Some(first), Some(last)) = (first, last) else {
        return String::new();
    };
    let mut text = lines[first..=last].join("\n");
    if text.len() > BODY_CAP {
        let mut end = BODY_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The index of the first row at or after `from` that `test` accepts.
fn find(rows: &[String], from: usize, test: impl Fn(&str) -> bool) -> Option<usize> {
    (from..rows.len()).find(|&index| test(&rows[index]))
}

/// How many spaces `row` starts with.
fn indent_of(row: &str) -> usize {
    row.len() - row.trim_start_matches(' ').len()
}

/// Whether `row` is a content box's rule: a run of `╌`.
fn is_box_rule(row: &str) -> bool {
    let text = row.trim();
    !text.is_empty() && text.chars().all(|c| c == BOX_CHAR)
}

/// Whether `row` could be the wrapped tail of a rule wider than the grid:
/// nothing but `─` once trimmed (`  ──`).
fn is_rule_tail(row: &str) -> bool {
    let text = row.trim();
    !text.is_empty() && text.chars().all(|c| c == RULE_CHAR)
}

/// The indexes of the rule rows in `rows`: rows that start and end with the
/// rule character and hold it in at least half the widest row's width.
fn rule_indices(rows: &[String]) -> Vec<usize> {
    let widest = rows
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            let text = row.trim();
            let count = row.chars().filter(|&c| c == RULE_CHAR).count();
            text.starts_with(RULE_CHAR) && text.ends_with(RULE_CHAR) && count * 2 >= widest
        })
        .map(|(index, _)| index)
        .collect()
}

/// The index of the first row under the last rule, or 0 with no rule. A
/// labelled rule with only blank rows under it (the one Claude Code 2.1.319
/// draws the session's name in under a permission dialog) closes the screen
/// and is passed over; a bare rule there still starts below it.
fn after_last_rule(rows: &[String]) -> usize {
    rule_indices(rows)
        .iter()
        .rev()
        .find(|&&index| {
            let closes = rows[index + 1..].iter().all(|row| row.trim().is_empty());
            let labelled = !rows[index].trim().trim_matches(RULE_CHAR).trim().is_empty();
            !(closes && labelled)
        })
        .map_or(0, |&index| index + 1)
}

/// The keys that give `answer` to `prompt`.
pub(crate) fn recipe(
    prompt: &ChatPrompt,
    answer: &ChatAnswerKind,
) -> Result<AnswerRecipe, AnswerRefusal> {
    if let ChatAnswerKind::Adjust { raise } = answer {
        return adjust_recipe(prompt, *raise);
    }
    match prompt.kind {
        ChatPromptKind::Question => return question_recipe(prompt, answer),
        ChatPromptKind::Review => return review_recipe(prompt, answer),
        ChatPromptKind::ModelPicker => return picker_recipe(prompt, answer),
        ChatPromptKind::Permission
        | ChatPromptKind::Plan
        | ChatPromptKind::Trust
        | ChatPromptKind::McpServer
        | ChatPromptKind::HeadsUp
        | ChatPromptKind::Confirm => {}
        ChatPromptKind::Unknown => return Err(AnswerRefusal::Unsupported),
    }
    if field_focused(prompt) {
        return match answer {
            ChatAnswerKind::TypeSomething { text } if prompt.kind == ChatPromptKind::Plan => {
                typed_text(text).map(|step| AnswerRecipe { steps: vec![step] })
            }
            _ => Err(AnswerRefusal::NotThisDialog),
        };
    }
    match answer {
        ChatAnswerKind::Option { number } => option_recipe(prompt, *number),
        ChatAnswerKind::AllowWithText { text } => allow_with_text(prompt, text),
        ChatAnswerKind::DenyWithText { text } => deny_with_text(prompt, text),
        ChatAnswerKind::TypeSomething { text } => plan_feedback(prompt, text),
        _ => Err(AnswerRefusal::NotThisDialog),
    }
}

/// The number of a plan's feedback option: the one labelled
/// [`PLAN_FEEDBACK_LABEL`], else the last option when the cursor is on it
/// (its label replaced by the text typed so far).
fn feedback_option(prompt: &ChatPrompt) -> Option<u32> {
    if prompt.kind != ChatPromptKind::Plan {
        return None;
    }
    if let Some(option) = prompt
        .options
        .iter()
        .find(|option| option.label == PLAN_FEEDBACK_LABEL)
    {
        return Some(option.number);
    }
    let last = prompt.options.last()?;
    (prompt.cursor == Some(last.number)).then_some(last.number)
}

/// Whether a text field has the keyboard, so a digit would be typed into
/// it: a plan with the cursor on its feedback option, or a permission
/// dialog whose option 1 was turned into its amend field.
fn field_focused(prompt: &ChatPrompt) -> bool {
    match prompt.kind {
        ChatPromptKind::Plan => prompt.cursor.is_some() && prompt.cursor == feedback_option(prompt),
        ChatPromptKind::Permission => !prompt.can_amend && prompt.cursor == Some(1),
        _ => false,
    }
}

/// The step that types `text` into a focused field and sends it.
fn typed_text(text: &str) -> Result<RecipeStep, AnswerRefusal> {
    check_field_text(text)?;
    Ok(RecipeStep {
        keys: vec![Key::Text(text.to_owned()), Key::Enter],
        expect: ScreenExpect::Any,
    })
}

/// Whether `text` may be typed into a field as keys: not empty, and with no
/// control character, a newline or a tab included, since each would act as
/// a key.
fn check_field_text(text: &str) -> Result<(), AnswerRefusal> {
    if text.trim().is_empty() {
        return Err(AnswerRefusal::TextEmpty);
    }
    if text.chars().any(char::is_control) {
        return Err(AnswerRefusal::ControlCharacter);
    }
    Ok(())
}

/// A recipe of one step.
fn one_step(keys: Vec<Key>, expect: ScreenExpect) -> AnswerRecipe {
    AnswerRecipe {
        steps: vec![RecipeStep { keys, expect }],
    }
}

/// The digit key of option `number` of `prompt`.
fn digit_for(prompt: &ChatPrompt, number: u32) -> Result<Key, AnswerRefusal> {
    if !prompt.options.iter().any(|option| option.number == number) {
        return Err(AnswerRefusal::UnknownOption(number));
    }
    Key::digit(number).ok_or(AnswerRefusal::TooManyOptions)
}

/// Choosing option `number`: its digit, or on a trust, new-MCP-server or
/// confirm dialog, the arrows to it and Enter.
fn option_recipe(prompt: &ChatPrompt, number: u32) -> Result<AnswerRecipe, AnswerRefusal> {
    if matches!(
        prompt.kind,
        ChatPromptKind::Trust | ChatPromptKind::McpServer | ChatPromptKind::Confirm
    ) {
        return trust_recipe(prompt, number);
    }
    if feedback_option(prompt) == Some(number) {
        return Err(AnswerRefusal::NotThisDialog);
    }
    let expect = if prompt.kind == ChatPromptKind::HeadsUp {
        ScreenExpect::Any
    } else {
        ScreenExpect::DialogGone
    };
    Ok(one_step(vec![digit_for(prompt, number)?], expect))
}

/// The key that picks the highlighted model for this session only.
const SESSION_ONLY_KEY: &str = "s";

/// Picking a model row of the `/model` picker for this session alone: the
/// arrows to its row, then `s`, which closes the picker or opens the Switch
/// model? dialog. Never Enter (it saves the default), a digit, or Left and
/// Right (they move the effort, which only [`adjust_recipe`] does). The More
/// models row is refused.
fn picker_recipe(
    prompt: &ChatPrompt,
    answer: &ChatAnswerKind,
) -> Result<AnswerRecipe, AnswerRefusal> {
    let ChatAnswerKind::Option { number } = answer else {
        return Err(AnswerRefusal::NotThisDialog);
    };
    let more_models = prompt
        .options
        .iter()
        .any(|option| option.number == *number && option.label == MORE_MODELS);
    if more_models {
        return Err(AnswerRefusal::NotThisDialog);
    }
    let mut steps: Vec<RecipeStep> = cursor_move(prompt, *number)?.into_iter().collect();
    steps.push(RecipeStep {
        keys: vec![Key::Text(SESSION_ONLY_KEY.to_owned())],
        expect: ScreenExpect::Any,
    });
    Ok(AnswerRecipe { steps })
}

/// Stepping the `/model` picker's effort one level: Right raises it, Left
/// lowers it, and the picker stays up (at either end the key changes
/// nothing). Never Enter. Refused on a picker without an effort row, and on
/// any other prompt, which has no row to adjust.
fn adjust_recipe(prompt: &ChatPrompt, raise: bool) -> Result<AnswerRecipe, AnswerRefusal> {
    if prompt.kind != ChatPromptKind::ModelPicker {
        return Err(AnswerRefusal::Unsupported);
    }
    if prompt.adjust.is_none() {
        return Err(AnswerRefusal::NotThisDialog);
    }
    let key = if raise { Key::Right } else { Key::Left };
    Ok(one_step(vec![key], ScreenExpect::SameDialogMayStay))
}

/// Moving a trust or new-MCP-server dialog's cursor from where the reading
/// shows it to option `number`, then Enter.
fn trust_recipe(prompt: &ChatPrompt, number: u32) -> Result<AnswerRecipe, AnswerRefusal> {
    let mut steps: Vec<RecipeStep> = cursor_move(prompt, number)?.into_iter().collect();
    steps.push(RecipeStep {
        keys: vec![Key::Enter],
        expect: ScreenExpect::DialogGone,
    });
    Ok(AnswerRecipe { steps })
}

/// The arrows that move the cursor from the option the reading shows it on
/// to option `number`, as one step waiting for the cursor there; `None`
/// when it is already there.
fn cursor_move(prompt: &ChatPrompt, number: u32) -> Result<Option<RecipeStep>, AnswerRefusal> {
    let position = |wanted: u32| {
        prompt
            .options
            .iter()
            .position(|option| option.number == wanted)
    };
    let target = position(number).ok_or(AnswerRefusal::UnknownOption(number))?;
    let cursor = prompt
        .cursor
        .and_then(position)
        .ok_or(AnswerRefusal::NotThisDialog)?;
    if target == cursor {
        return Ok(None);
    }
    let key = if target > cursor { Key::Down } else { Key::Up };
    Ok(Some(RecipeStep {
        keys: vec![key; target.abs_diff(cursor)],
        expect: ScreenExpect::OptionSelected(number),
    }))
}

/// The keys that give `answer` to an `AskUserQuestion` step. A digit picks
/// a single-choice option (and moves on) or ticks a multi-select one; the
/// arrows move between questions. While the cursor is on the `Type
/// something` option every printable key goes into its field, so only
/// `Submit` and `TypeSomething` are built then.
fn question_recipe(
    prompt: &ChatPrompt,
    answer: &ChatAnswerKind,
) -> Result<AnswerRecipe, AnswerRefusal> {
    let field_focused = prompt.cursor.is_some() && prompt.cursor == type_something_option(prompt);
    let keeps_field = matches!(
        answer,
        ChatAnswerKind::Submit | ChatAnswerKind::TypeSomething { .. }
    );
    if field_focused && !keeps_field {
        return Err(AnswerRefusal::NotThisDialog);
    }
    match answer {
        ChatAnswerKind::Option { number } if !prompt.multi => {
            if type_something_option(prompt) == Some(*number) {
                return Err(AnswerRefusal::NotThisDialog);
            }
            Ok(one_step(
                vec![digit_for(prompt, *number)?],
                ScreenExpect::Any,
            ))
        }
        ChatAnswerKind::Toggle { number } if prompt.multi => toggle(prompt, *number),
        ChatAnswerKind::Submit if prompt.multi => submit_multi(prompt),
        ChatAnswerKind::TypeSomething { text } => type_something(prompt, text),
        ChatAnswerKind::Back => Ok(one_step(vec![Key::Left], ScreenExpect::Any)),
        ChatAnswerKind::Skip => Ok(one_step(vec![Key::Right], ScreenExpect::Any)),
        _ => Err(AnswerRefusal::NotThisDialog),
    }
}

/// The number of a question's `Type something` option: always its last
/// option, whose label becomes the typed text once there is some.
fn type_something_option(prompt: &ChatPrompt) -> Option<u32> {
    prompt.options.last().map(|option| option.number)
}

/// Ticking or unticking option `number` of a multi-select question.
fn toggle(prompt: &ChatPrompt, number: u32) -> Result<AnswerRecipe, AnswerRefusal> {
    let option = prompt
        .options
        .iter()
        .find(|option| option.number == number)
        .ok_or(AnswerRefusal::UnknownOption(number))?;
    Ok(one_step(
        vec![digit_for(prompt, number)?],
        ScreenExpect::OptionTicked(number, !option.selected),
    ))
}

/// Submitting a multi-select question: Down from the cursor's option past
/// the options below it to the `Next` row, then Enter.
fn submit_multi(prompt: &ChatPrompt) -> Result<AnswerRecipe, AnswerRefusal> {
    let cursor = prompt.cursor.ok_or(AnswerRefusal::NotThisDialog)?;
    let below = prompt
        .options
        .iter()
        .skip_while(|option| option.number != cursor)
        .skip(1)
        .count();
    Ok(AnswerRecipe {
        steps: vec![
            RecipeStep {
                keys: vec![Key::Down; below + 1],
                expect: ScreenExpect::SameDialog,
            },
            RecipeStep {
                keys: vec![Key::Enter],
                expect: ScreenExpect::Any,
            },
        ],
    })
}

/// Answering a question with text: the cursor moved to the `Type something`
/// option, then `i` and Backspace, then the text and Enter.
///
/// The `i` and Backspace go first because the vim editor mode reads raw keys
/// as commands in NORMAL mode, so the text would arrive as vim commands and
/// no letters at all: `i` enters INSERT. Without vim, or in vim's INSERT
/// mode, `i` types the letter and the Backspace takes it back, so the pair
/// leaves the field as it was.
///
/// The two steps expect a settled reading only: in vim's NORMAL mode `i`
/// changes the footer alone, which the parsed screen does not carry, so a
/// same-dialog check would never see the step taken.
fn type_something(prompt: &ChatPrompt, text: &str) -> Result<AnswerRecipe, AnswerRefusal> {
    let typed = typed_text(text)?;
    let option = type_something_option(prompt).ok_or(AnswerRefusal::NotThisDialog)?;
    let mut steps: Vec<RecipeStep> = cursor_move(prompt, option)?.into_iter().collect();
    steps.push(RecipeStep {
        keys: vec![Key::Text("i".to_owned())],
        expect: ScreenExpect::Any,
    });
    steps.push(RecipeStep {
        keys: vec![Key::Backspace],
        expect: ScreenExpect::Any,
    });
    steps.push(typed);
    Ok(AnswerRecipe { steps })
}

/// The review step's options: its digit submits or cancels the answers.
fn review_recipe(
    prompt: &ChatPrompt,
    answer: &ChatAnswerKind,
) -> Result<AnswerRecipe, AnswerRefusal> {
    match answer {
        ChatAnswerKind::Option { number } => Ok(one_step(
            vec![digit_for(prompt, *number)?],
            ScreenExpect::DialogGone,
        )),
        _ => Err(AnswerRefusal::NotThisDialog),
    }
}

/// Allowing with a message: Tab, with the cursor on option 1, turns it into
/// a field, then the text and Enter.
fn allow_with_text(prompt: &ChatPrompt, text: &str) -> Result<AnswerRecipe, AnswerRefusal> {
    if prompt.kind != ChatPromptKind::Permission || !prompt.can_amend || prompt.cursor != Some(1) {
        return Err(AnswerRefusal::NotThisDialog);
    }
    check_field_text(text)?;
    Ok(AnswerRecipe {
        steps: vec![
            RecipeStep {
                keys: vec![Key::Tab],
                expect: ScreenExpect::SameDialog,
            },
            RecipeStep {
                keys: vec![Key::Text(text.to_owned()), Key::Enter],
                expect: ScreenExpect::DialogGone,
            },
        ],
    })
}

/// Denying with a message: the digit of the last option labelled `No`; the
/// message is sent once the input box is back, outside the recipe.
fn deny_with_text(prompt: &ChatPrompt, text: &str) -> Result<AnswerRecipe, AnswerRefusal> {
    if prompt.kind != ChatPromptKind::Permission {
        return Err(AnswerRefusal::NotThisDialog);
    }
    if text.trim().is_empty() {
        return Err(AnswerRefusal::TextEmpty);
    }
    let no = prompt
        .options
        .iter()
        .rev()
        .find(|option| option.label == "No")
        .ok_or(AnswerRefusal::NotThisDialog)?;
    Ok(one_step(
        vec![digit_for(prompt, no.number)?],
        ScreenExpect::DialogGone,
    ))
}

/// A plan's feedback: the feedback option's digit turns it into a field,
/// then the text and Enter.
fn plan_feedback(prompt: &ChatPrompt, text: &str) -> Result<AnswerRecipe, AnswerRefusal> {
    let feedback = feedback_option(prompt).ok_or(AnswerRefusal::NotThisDialog)?;
    let typed = typed_text(text)?;
    Ok(AnswerRecipe {
        steps: vec![
            RecipeStep {
                keys: vec![digit_for(prompt, feedback)?],
                expect: ScreenExpect::SameDialog,
            },
            typed,
        ],
    })
}

/// The hint row the agents panel shows while it holds the keys; what
/// follows it varies.
const PANEL_HINT: &str = "↑/↓ to select";

/// The end of the mode line while the tasks pill holds the keys.
const TASKS_PILL_HINT: &str = "Enter to view tasks";

/// The end of the mode line while a prompt is stashed.
const STASHED: &str = "› stashed";

/// The start of the placeholder an agent's empty box shows
/// (`Message @general-purpose…`).
const AGENT_PLACEHOLDER: &str = "Message @";

/// A panel row's dot: open, and filled on the row in view.
const DOT_OPEN: char = '◯';
const DOT_IN_VIEW: char = '●';

/// The gap that parts a panel row's label from its age and token figures.
const PANEL_GAP: &str = "   ";

/// One row of the agents panel.
struct PanelRow {
    label: String,
    cursor: bool,
    in_view: bool,
}

/// Which conversation the screen whose visible rows are `rows` shows, and
/// where its keys go: the rule over the box, the hint and mode line under
/// it, and the agents panel under those.
pub(crate) fn read_view(rows: &[String]) -> ViewReading {
    let rules = rule_indices(rows);
    let under: &[String] = rules.last().map_or(&[], |&last| &rows[last + 1..]);
    let mode_line = mode_line(under);
    let panel: Vec<PanelRow> = under.iter().filter_map(|row| panel_row(row)).collect();
    let dialog = matches!(
        read(rows).composer,
        ComposerState::Dialog | ComposerState::Overlay { .. }
    );
    let panel_in_view = panel.iter().position(|row| row.in_view);
    // A named session draws its name where an agent's description goes, so
    // the rule's text is an agent only with more to say so: the panel's
    // agent row in view, or with no panel, a dialog in the box's place or
    // the agent's placeholder in the box. Under a main-view dialog the rule
    // still carries the session's own name; the daemon drops a label equal
    // to the session's terminal title.
    let agent_shown = if panel.is_empty() {
        dialog || box_first_row(rows).is_some_and(is_agent_placeholder)
    } else {
        panel_in_view.is_some_and(|row| row > 0)
    };
    ViewReading {
        agent_in_view: agent_in_view(rows, &rules).filter(|_| agent_shown),
        key_holder: key_holder(under, mode_line),
        stashed: mode_line.is_some_and(|line| line.ends_with(STASHED)),
        box_first_line: if dialog { None } else { box_first_line(rows) },
        dialog,
        panel_cursor: panel.iter().position(|row| row.cursor),
        panel_in_view,
        panel: panel.into_iter().map(|row| row.label).collect(),
    }
}

/// The running work the screen whose visible rows are `rows` counts: the
/// agents panel's rows other than `main` that show a running subagent's
/// figures, and the shells the mode line counts. `None` while a dialog is
/// up, while the panel holds the keys (its hint replaces the mode line), or
/// when no mode line shows under the box. A frame with no prompt box at all
/// (the shell's prompt is back) counts none of either.
pub(crate) fn read_work(rows: &[String]) -> Option<ScreenWork> {
    let view = read_view(rows);
    if view.dialog || view.key_holder == KeyHolder::Panel {
        return None;
    }
    let rules = rule_indices(rows);
    let Some(&last) = rules.last() else {
        return Some(ScreenWork {
            subagents: 0,
            shells: 0,
        });
    };
    let under = &rows[last + 1..];
    let mode_line = mode_line(under)?;
    let subagents = under
        .iter()
        .filter(|row| panel_row(row).is_some())
        .skip(1)
        .filter(|row| has_running_figures(row))
        .count();
    Some(ScreenWork {
        subagents: u32::try_from(subagents).unwrap_or(u32::MAX),
        shells: shell_count(mode_line),
    })
}

/// Whether a panel row ends in a running subagent's figures: its age
/// (`5s`, `3m 16s`), then `· ↑ <n> tokens` or `· ↓ <n> tokens`.
fn has_running_figures(row: &str) -> bool {
    let Some((age, tokens)) = row
        .trim_end()
        .strip_suffix(" tokens")
        .and_then(|figures| figures.rsplit_once(" · "))
    else {
        return false;
    };
    let count = tokens
        .strip_prefix("↑ ")
        .or_else(|| tokens.strip_prefix("↓ "));
    let age = age.rsplit(PANEL_GAP).next().unwrap_or_default().trim();
    count.is_some_and(|count| !count.is_empty() && !count.contains(' ')) && is_age(age)
}

/// Whether `text` is an age as the panel writes one: one or more words of
/// digits closed by `h`, `m` or `s`.
fn is_age(text: &str) -> bool {
    !text.is_empty()
        && text.split(' ').all(|word| {
            word.strip_suffix(['h', 'm', 's']).is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
        })
}

/// The shells the mode line's `· N shell ·` or `· N shells ·` item counts;
/// in `1 shell, 1 monitor` only the shell part. Zero when no item counts
/// shells.
fn shell_count(mode_line: &str) -> u32 {
    mode_line
        .split(" · ")
        .find_map(|item| {
            let first = item.split(',').next()?.trim();
            let (count, noun) = first.split_once(' ')?;
            if matches!(noun, "shell" | "shells") {
                count.parse::<u32>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// The description on the rule over the box (`─ Sleeper A ─`): the later
/// of the last two rules that carries one, since a dialog in an agent's
/// view draws that rule last.
fn agent_in_view(rows: &[String], rules: &[usize]) -> Option<String> {
    let last_two = &rules[rules.len().saturating_sub(2)..];
    last_two.iter().rev().find_map(|&index| {
        let text = rows[index].trim().trim_matches(RULE_CHAR).trim();
        (!text.is_empty()).then(|| text.to_owned())
    })
}

/// The mode line: the last row of the block directly under the box, which
/// a status line may head.
fn mode_line(under: &[String]) -> Option<&str> {
    under
        .iter()
        .map(|row| row.trim())
        .take_while(|row| !row.is_empty())
        .last()
}

/// Which part of the screen holds the keys: the panel while its hint
/// shows, the tasks pill while the mode line ends with its hint, else the
/// box.
fn key_holder(under: &[String], mode_line: Option<&str>) -> KeyHolder {
    if under
        .iter()
        .any(|row| row.trim_start().starts_with(PANEL_HINT))
    {
        KeyHolder::Panel
    } else if mode_line.is_some_and(|line| line.ends_with(TASKS_PILL_HINT)) {
        KeyHolder::TasksPill
    } else {
        KeyHolder::Box
    }
}

/// `row` as an agents panel row (`❯ ● general-purpose  Sleeper A   30s · …`),
/// its label cut before the figures.
fn panel_row(row: &str) -> Option<PanelRow> {
    let text = row.trim_start();
    let (cursor, text) = match text.strip_prefix(CURSOR) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let mut chars = text.chars();
    let in_view = match chars.next()? {
        DOT_IN_VIEW => true,
        DOT_OPEN => false,
        _ => return None,
    };
    let rest = chars.as_str();
    if !rest.starts_with(' ') {
        return None;
    }
    let label = rest.trim_start().split(PANEL_GAP).next()?.trim_end();
    (!label.is_empty()).then(|| PanelRow {
        label: label.to_owned(),
        cursor,
        in_view,
    })
}

/// The box's first line of text, its prompt taken off; `None` for an empty
/// box or one showing only a placeholder.
fn box_first_line(rows: &[String]) -> Option<String> {
    let text = box_first_row(rows)?;
    let placeholder = is_try_placeholder(text) || is_agent_placeholder(text);
    (!text.is_empty() && !placeholder).then(|| text.to_owned())
}

/// The box's first non-empty row, trimmed and its prompt taken off.
fn box_first_row(rows: &[String]) -> Option<&str> {
    let (top, bottom) = box_rules(rows)?;
    let first = rows[top + 1..bottom]
        .iter()
        .map(|row| row.trim())
        .find(|row| !row.is_empty())?;
    Some(first.strip_prefix(CURSOR).map_or(first, str::trim))
}

/// Whether `text`, a box's trimmed first line after the prompt, is the
/// placeholder an agent's empty box shows (`Message @general-purpose…`).
fn is_agent_placeholder(text: &str) -> bool {
    text.starts_with(AGENT_PLACEHOLDER) && text.ends_with('…')
}

/// The one key that moves the screen read as `reading` towards `goal`.
/// Only Down, and Up or Enter where the panel or the tasks pill holds the
/// keys, ever come back: never Left, a digit, Tab, text or the stash.
pub(crate) fn next_view_key(goal: ViewGoal, reading: &ViewReading) -> ViewStep {
    if reading.dialog {
        return ViewStep::Refuse(ViewRefusal::DialogUp);
    }
    match goal {
        ViewGoal::Main => step_to_main(reading),
        ViewGoal::Agent { row } => step_to_agent(row, reading),
    }
}

/// The next key back to the main conversation with the box holding the
/// keys.
fn step_to_main(reading: &ViewReading) -> ViewStep {
    let agent = reading.agent_in_view.is_some();
    match reading.key_holder {
        KeyHolder::Box | KeyHolder::TasksPill if agent => ViewStep::Press(Key::Down),
        KeyHolder::Box => ViewStep::Done,
        KeyHolder::TasksPill => ViewStep::Press(Key::Up),
        KeyHolder::Panel if reading.panel_in_view == Some(0) => ViewStep::Press(Key::Up),
        KeyHolder::Panel => panel_step(reading.panel_cursor, 0),
    }
}

/// The next key to the agent on panel row `row` with the box holding the
/// keys.
fn step_to_agent(row: usize, reading: &ViewReading) -> ViewStep {
    if !reading.panel.is_empty() && reading.panel.len() <= row {
        return ViewStep::Refuse(ViewRefusal::RowMissing);
    }
    let row_in_view = reading.panel_in_view == Some(row);
    match reading.key_holder {
        KeyHolder::Box if row_in_view && reading.agent_in_view.is_some() => ViewStep::Done,
        KeyHolder::TasksPill | KeyHolder::Panel if row_in_view => ViewStep::Press(Key::Up),
        KeyHolder::Box | KeyHolder::TasksPill => ViewStep::Press(Key::Down),
        KeyHolder::Panel => panel_step(reading.panel_cursor, row),
    }
}

/// The key that moves the panel's cursor at `cursor` to row `row`, or
/// opens that row once the cursor is on it.
fn panel_step(cursor: Option<usize>, row: usize) -> ViewStep {
    let Some(cursor) = cursor else {
        return ViewStep::Refuse(ViewRefusal::NoPanel);
    };
    ViewStep::Press(match cursor.cmp(&row) {
        Ordering::Equal => Key::Enter,
        Ordering::Less => Key::Down,
        Ordering::Greater => Key::Up,
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        clear_draft_recipe, is_path_token, next_view_key, read, read_draft, read_styled, read_view,
        read_view_styled, recipe, review_body, spinner_above,
    };
    use agent_api::{
        AnswerRefusal, DraftReading, Key, KeyHolder, RecipeStep, ScreenExpect, ViewGoal,
        ViewReading, ViewRefusal, ViewStep,
    };
    use protocol::{
        ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ComposerState,
    };

    /// The `/model` picker and its Switch model? dialog as 2.1.329 draws
    /// them, from the daemon's screen fixtures.
    const PICKER: &str =
        include_str!("../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker.txt");
    const PICKER_ON_SONNET: &str = include_str!(
        "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-on-sonnet.txt"
    );
    const PICKER_MORE_MODELS: &str = include_str!(
        "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-more-models.txt"
    );
    const SWITCH_DIALOG: &str = include_str!(
        "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-switch-dialog.txt"
    );
    const SESSION_ONLY_SWITCH: &str = include_str!(
        "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-session-only-switch.txt"
    );
    /// Every capture that shows the picker with its `s` footer.
    const PICKERS: [&str; 7] = [
        PICKER,
        PICKER_ON_SONNET,
        include_str!(
            "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-after-session-switch.txt"
        ),
        include_str!(
            "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-effort-left.txt"
        ),
        include_str!(
            "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-effort-right.txt"
        ),
        include_str!(
            "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-before-esc.txt"
        ),
        include_str!(
            "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-switch-dialog-go-back.txt"
        ),
    ];

    /// The rows of a capture.
    fn capture(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    /// The prompt a capture shows.
    fn prompt_in(text: &str) -> ChatPrompt {
        read(&capture(text))
            .prompt
            .expect("the capture shows a prompt")
    }

    /// A picker option.
    fn model(number: u32, label: &str, description: Option<&str>, selected: bool) -> ChatOption {
        ChatOption {
            number,
            label: label.to_owned(),
            description: description.map(str::to_owned),
            selected,
        }
    }

    #[test]
    fn the_model_picker_reads_as_a_prompt_with_its_rows() {
        let screen = read(&capture(PICKER));
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.expect("a picker prompt");
        assert_eq!(prompt.kind, ChatPromptKind::ModelPicker);
        assert_eq!(prompt.subject, "Select model");
        assert_eq!(prompt.body, "");
        assert_eq!(prompt.cursor, Some(1));
        assert_eq!(
            prompt.options,
            [
                model(
                    1,
                    "Default (recommended)",
                    Some("Current · Opus 5.5 · Best for everyday, complex tasks"),
                    true
                ),
                model(
                    2,
                    "Opus 5.5",
                    Some("For complex work and everyday tasks"),
                    false
                ),
                model(3, "Fable 5.2", Some("For your toughest challenges"), false),
                model(
                    4,
                    "Sonnet 5.5",
                    Some("Most efficient for simpler tasks"),
                    false
                ),
                model(5, "Haiku 5.5", Some("Fastest for quick answers"), false),
                model(6, "More models", None, false),
            ]
        );
    }

    #[test]
    fn the_picker_cursor_follows_the_highlighted_row() {
        let prompt = prompt_in(PICKER_ON_SONNET);
        assert_eq!(prompt.kind, ChatPromptKind::ModelPicker);
        assert_eq!(prompt.cursor, Some(4));
        let selected: Vec<u32> = prompt
            .options
            .iter()
            .filter(|option| option.selected)
            .map(|option| option.number)
            .collect();
        assert_eq!(selected, [4]);
        assert!(
            prompt.options[0]
                .description
                .as_deref()
                .is_some_and(|text| text.starts_with("Current · "))
        );
    }

    #[test]
    fn more_models_is_option_six() {
        let prompt = prompt_in(PICKER_MORE_MODELS);
        assert_eq!(prompt.kind, ChatPromptKind::ModelPicker);
        assert_eq!(prompt.cursor, Some(6));
        assert_eq!(prompt.options[5], model(6, "More models", None, true));
        assert_eq!(
            prompt.options[1].description.as_deref(),
            Some("Current · For complex work and everyday tasks")
        );
    }

    #[test]
    fn the_switch_model_dialog_reads_as_a_confirm() {
        let screen = read(&capture(SWITCH_DIALOG));
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.expect("a confirm prompt");
        assert_eq!(prompt.kind, ChatPromptKind::Confirm);
        assert_eq!(prompt.subject, "Switch model?");
        assert_eq!(
            prompt.body,
            "Your next response will be slower and use more tokens\n\n\
             This conversation is cached for the current model. Switching to Opus 5.5 means the full history gets re-read."
        );
        let options: Vec<(u32, &str)> = prompt
            .options
            .iter()
            .map(|option| (option.number, option.label.as_str()))
            .collect();
        assert_eq!(
            options,
            [(1, "Yes, switch to Opus 5.5"), (2, "No, go back")]
        );
        assert_eq!(prompt.cursor, Some(1));
    }

    /// The answer that picks option `number`.
    fn pick(number: u32) -> ChatAnswerKind {
        ChatAnswerKind::Option { number }
    }

    #[test]
    fn a_picker_answer_moves_then_presses_s() {
        let prompt = prompt_in(PICKER);
        let steps = recipe(&prompt, &pick(4)).expect("a recipe").steps;
        assert_eq!(
            steps,
            [
                RecipeStep {
                    keys: vec![Key::Down; 3],
                    expect: ScreenExpect::OptionSelected(4),
                },
                RecipeStep {
                    keys: vec![Key::Text("s".to_owned())],
                    expect: ScreenExpect::Any,
                },
            ]
        );
    }

    #[test]
    fn a_picker_answer_never_holds_enter_a_digit_or_left_right() {
        for text in PICKERS {
            let prompt = prompt_in(text);
            assert_eq!(prompt.kind, ChatPromptKind::ModelPicker);
            for number in 1..=5 {
                let steps = recipe(&prompt, &pick(number)).expect("a recipe").steps;
                let keys: Vec<&Key> = steps.iter().flat_map(|step| &step.keys).collect();
                assert!(
                    keys.iter().all(|key| !matches!(
                        key,
                        Key::Enter | Key::Digit(_) | Key::Left | Key::Right
                    )),
                    "option {number} holds {keys:?}"
                );
                assert_eq!(
                    steps.last(),
                    Some(&RecipeStep {
                        keys: vec![Key::Text("s".to_owned())],
                        expect: ScreenExpect::Any,
                    })
                );
            }
        }
    }

    #[test]
    fn the_picker_reads_its_effort_row_as_its_adjust_label() {
        let effort_right = include_str!(
            "../../../daemon/src/screen/fixtures/claude--v2.1.329-model-picker-effort-right.txt"
        );
        for (text, label) in [
            (PICKER, Some("High effort")),
            (PICKER_ON_SONNET, Some("Medium effort (default)")),
            (effort_right, Some("xHigh effort")),
            (PICKER_MORE_MODELS, None),
        ] {
            assert_eq!(prompt_in(text).adjust.as_deref(), label);
        }
    }

    #[test]
    fn an_adjust_answer_holds_one_arrow_and_no_enter() {
        for text in PICKERS {
            let prompt = prompt_in(text);
            for (raise, key) in [(true, Key::Right), (false, Key::Left)] {
                let steps = recipe(&prompt, &ChatAnswerKind::Adjust { raise })
                    .expect("a recipe")
                    .steps;
                assert_eq!(
                    steps,
                    [RecipeStep {
                        keys: vec![key],
                        expect: ScreenExpect::SameDialogMayStay,
                    }]
                );
            }
        }
    }

    #[test]
    fn an_adjust_answer_is_refused_without_an_effort_row_or_off_the_picker() {
        let more_models = prompt_in(PICKER_MORE_MODELS);
        let confirm = prompt_in(SWITCH_DIALOG);
        for raise in [true, false] {
            let adjust = ChatAnswerKind::Adjust { raise };
            assert_eq!(
                recipe(&more_models, &adjust),
                Err(AnswerRefusal::NotThisDialog)
            );
            assert_eq!(recipe(&confirm, &adjust), Err(AnswerRefusal::Unsupported));
        }
    }

    #[test]
    fn more_models_cannot_be_answered() {
        for text in [PICKER, PICKER_MORE_MODELS] {
            let prompt = prompt_in(text);
            assert_eq!(recipe(&prompt, &pick(6)), Err(AnswerRefusal::NotThisDialog));
        }
    }

    #[test]
    fn a_confirm_answer_moves_then_presses_enter() {
        let prompt = prompt_in(SWITCH_DIALOG);
        let steps = recipe(&prompt, &pick(2)).expect("a recipe").steps;
        assert_eq!(
            steps,
            [
                RecipeStep {
                    keys: vec![Key::Down],
                    expect: ScreenExpect::OptionSelected(2),
                },
                RecipeStep {
                    keys: vec![Key::Enter],
                    expect: ScreenExpect::DialogGone,
                },
            ]
        );
    }

    #[test]
    fn a_quoted_picker_above_the_input_box_is_no_prompt() {
        let picker = capture(PICKER);
        let title = picker
            .iter()
            .position(|row| row.trim() == "Select model")
            .expect("the picker's title");
        let mut rows: Vec<String> = picker[title..]
            .iter()
            .map(|row| format!("  {row}"))
            .collect();
        let closed = capture(SESSION_ONLY_SWITCH);
        let rule = closed
            .iter()
            .position(|row| row.starts_with('─'))
            .expect("the input box's rule");
        rows.extend(closed[rule..].iter().cloned());
        let screen = read(&rows);
        assert_eq!(screen.prompt, None);
        assert_eq!(screen.composer, ComposerState::Ready);
    }

    #[test]
    fn a_quoted_switch_title_above_the_dialog_keeps_the_body() {
        let mut rows = vec!["  Switch model?".to_owned(), String::new()];
        rows.extend(capture(SWITCH_DIALOG));
        let prompt = read(&rows).prompt.expect("a confirm prompt");
        assert_eq!(prompt.kind, ChatPromptKind::Confirm);
        assert_eq!(prompt.body, prompt_in(SWITCH_DIALOG).body);
    }

    #[test]
    fn a_closed_picker_reads_no_prompt() {
        let screen = read(&capture(SESSION_ONLY_SWITCH));
        assert_eq!(screen.prompt, None);
        assert_ne!(screen.composer, ComposerState::Dialog);
    }

    #[test]
    fn plan_feedback_finds_its_option_by_label() {
        let mut screen = rows(&[
            "   Ready to code?",
            "",
            "   Here is Claude's plan:",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌",
            "   Add div to calc.py",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌",
        ]);
        screen.push(format!("  {}", "─".repeat(90)));
        screen.extend(rows(&[
            "   Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            "   ❯ 1. Yes, and use auto mode",
            "     2. Yes, and bypass permissions",
            "     3. Yes, manually approve edits",
            "     4. Tell Claude what to change",
            "        shift+tab to approve with this feedback",
            "",
            "   ctrl+g to edit in Notepad · ~\\.claude\\plans\\plan.md",
        ]));
        let prompt = read(&screen).prompt.expect("a plan prompt");
        let answer = ChatAnswerKind::TypeSomething {
            text: "x".to_owned(),
        };
        let steps = recipe(&prompt, &answer).expect("a recipe").steps;
        assert_eq!(
            steps,
            [
                RecipeStep {
                    keys: vec![Key::Digit(4)],
                    expect: ScreenExpect::SameDialog,
                },
                RecipeStep {
                    keys: vec![Key::Text("x".to_owned()), Key::Enter],
                    expect: ScreenExpect::Any,
                },
            ]
        );
    }

    #[test]
    fn allow_with_text_needs_the_cursor_on_option_one() {
        let screen = read(&permission(
            &["   1. Yes", " ❯ 2. No"],
            Some(" Esc to cancel · Tab to amend"),
        ));
        let prompt = screen.prompt.expect("a permission prompt");
        assert!(prompt.can_amend);
        let answer = ChatAnswerKind::AllowWithText {
            text: "x".to_owned(),
        };
        assert_eq!(recipe(&prompt, &answer), Err(AnswerRefusal::NotThisDialog));
    }

    /// The answers that type `text` into a field: a question's Type
    /// something, a plan's feedback and a permission's allow-with-text, each
    /// with a prompt it fits.
    fn text_answers(text: &str) -> Vec<(&'static str, ChatPrompt, ChatAnswerKind)> {
        let mut plan = rows(&[
            "   Ready to code?",
            "",
            "   Here is Claude's plan:",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌",
            "   Add div to calc.py",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌",
        ]);
        plan.push(format!("  {}", "─".repeat(90)));
        plan.extend(rows(&[
            "   Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            "   ❯ 1. Yes, and use auto mode",
            "     2. Yes, manually approve edits",
            "     3. Tell Claude what to change",
            "        shift+tab to approve with this feedback",
            "",
            "   ctrl+g to edit in Notepad · ~\\.claude\\plans\\plan.md",
        ]));
        let plan = read(&plan).prompt.expect("a plan prompt");
        let allow = read(&permission(
            &[" ❯ 1. Yes", "   2. No"],
            Some(" Esc to cancel · Tab to amend"),
        ))
        .prompt
        .expect("a permission prompt");
        let type_something = question(&ask(&["❯ 1. sub", "  2. div", "  3. Type something."]));
        let text = text.to_owned();
        vec![
            (
                "type something",
                type_something,
                ChatAnswerKind::TypeSomething { text: text.clone() },
            ),
            (
                "plan feedback",
                plan,
                ChatAnswerKind::TypeSomething { text: text.clone() },
            ),
            (
                "allow with text",
                allow,
                ChatAnswerKind::AllowWithText { text },
            ),
        ]
    }

    #[test]
    fn typed_text_with_a_control_character_is_refused() {
        for text in ["a\x1b[Bb", "a\rb", "a\nb", "a\tb", "a\x7fb", "a\u{9b}b"] {
            for (name, prompt, answer) in text_answers(text) {
                assert_eq!(
                    recipe(&prompt, &answer),
                    Err(AnswerRefusal::ControlCharacter),
                    "{name}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn plain_and_unicode_typed_text_is_typed() {
        for text in ["use 6*8 instead", "café ✓ — ok"] {
            for (name, prompt, answer) in text_answers(text) {
                let typed = recipe(&prompt, &answer)
                    .map(|recipe| recipe.steps.last().map(|step| step.keys.clone()));
                assert_eq!(
                    typed,
                    Ok(Some(vec![Key::Text(text.to_owned()), Key::Enter])),
                    "{name}: {text:?}"
                );
            }
        }
    }

    /// The rows of a screen 40 columns wide.
    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|&line| line.to_owned()).collect()
    }

    /// A full-width rule.
    fn rule() -> String {
        "─".repeat(40)
    }

    /// An input box holding `body`, under `above`.
    fn boxed(above: &[&str], body: &[&str]) -> Vec<String> {
        let mut screen = rows(above);
        screen.push(rule());
        screen.extend(rows(body));
        screen.push(rule());
        screen.push("  ? for shortcuts".to_owned());
        screen
    }

    /// An input box `width` columns wide holding `body`, on a screen
    /// `screen_rows` rows tall: blank rows above the box's top rule.
    fn draft_box(width: usize, body: &[String], screen_rows: usize) -> Vec<String> {
        let rule = "─".repeat(width);
        let mut screen = vec![String::new(); screen_rows.saturating_sub(body.len() + 3)];
        screen.push(rule.clone());
        screen.extend(body.iter().cloned());
        screen.push(rule);
        screen.push("  ? for shortcuts".to_owned());
        screen
    }

    /// `screen` read as a draft with nothing drawn dim.
    fn draft_of(screen: &[String]) -> Option<DraftReading> {
        read_draft(screen, &vec![String::new(); screen.len()])
    }

    /// A permission dialog with `options` and `footer` (none for a frame
    /// still being drawn).
    fn permission(options: &[&str], footer: Option<&str>) -> Vec<String> {
        let mut screen = rows(&["● Update(calc.py)"]);
        screen.push(rule());
        screen.extend(rows(&[
            " Edit file",
            " calc.py",
            " Do you want to make this edit to calc.py?",
        ]));
        screen.extend(rows(options));
        screen.extend(footer.map(str::to_owned));
        screen
    }

    fn labels(screen: &ChatScreen) -> Vec<(u32, String)> {
        let prompt = screen.prompt.as_ref().expect("a prompt");
        prompt
            .options
            .iter()
            .map(|option| (option.number, option.label.clone()))
            .collect()
    }

    /// A two-option permission dialog with its footer, then `under`.
    fn permission_over(under: &[String]) -> Vec<String> {
        let mut screen = permission(
            &[" ❯ 1. Yes", "   2. No"],
            Some(" Esc to cancel · Tab to amend"),
        );
        screen.extend(under.iter().cloned());
        screen
    }

    #[test]
    fn a_labelled_rule_under_a_dialog_hides_nothing() {
        let named = format!("{} my-session ─", "─".repeat(28));
        let screen = read(&permission_over(&[named, String::new()]));
        assert_eq!(screen.composer, ComposerState::Dialog);
        assert_eq!(
            screen.prompt.map(|prompt| prompt.subject),
            Some("calc.py".to_owned())
        );
    }

    #[test]
    fn a_bare_rule_under_a_dialog_starts_below_it() {
        let screen = read(&permission_over(&[rule(), String::new()]));
        assert_eq!(screen.prompt, None, "nothing under the last rule");
    }

    #[test]
    fn a_wrapped_option_label_is_joined() {
        let screen = read(&permission(
            &[
                " ❯ 1. Yes",
                "   2. Yes, and switch to accept edits (auto-approve file edits",
                "      and common file commands) for this session (shift+tab)",
                "   3. No",
            ],
            Some(" Esc to cancel · Tab to amend"),
        ));
        assert_eq!(screen.composer, ComposerState::Dialog);
        assert_eq!(
            labels(&screen),
            [
                (1, "Yes".to_owned()),
                (
                    2,
                    "Yes, and switch to accept edits (auto-approve file edits and common file \
                     commands) for this session (shift+tab)"
                        .to_owned()
                ),
                (3, "No".to_owned()),
            ]
        );
        let prompt = screen.prompt.expect("a prompt");
        assert_eq!(prompt.kind, ChatPromptKind::Permission);
        assert_eq!(prompt.subject, "calc.py");
        assert_eq!(prompt.cursor, Some(1));
        assert!(prompt.can_amend);
        assert_eq!(prompt.always_allow_tip, None);
        assert_eq!(prompt.id, "");
    }

    #[test]
    fn a_dialog_without_its_footer_is_half_drawn() {
        let half = ChatScreen {
            composer: ComposerState::Dialog,
            prompt: None,
        };
        let options = [" ❯ 1. Yes", "   2. No"];
        assert_eq!(read(&permission(&options, None)), half);
        let mut plan = rows(&["   Ready to code?", "   Here is Claude's plan:"]);
        plan.push(rule());
        plan.extend(rows(&[
            "   Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "   ❯ 1. Yes, and use auto mode",
        ]));
        assert_eq!(read(&plan), half);
        let mut trust = vec![rule()];
        trust.extend(rows(&[
            " Accessing workspace:",
            " C:\\proj",
            " ❯ No, exit",
            "   Yes, I trust this folder",
        ]));
        assert_eq!(read(&trust), half);
    }

    /// A new-MCP-server dialog naming `rt-driver`, the cursor on its third
    /// option.
    fn mcp_server_dialog() -> Vec<String> {
        let mut screen = vec![rule()];
        screen.extend(rows(&[
            "  New MCP server found in this project: rt-driver",
            "",
            "  MCP servers may execute code.",
            "",
            "    Use this MCP server",
            "    Use this and all future MCP servers in this project",
            "  ❯ Continue without using this MCP server",
            "",
            "  Enter to confirm · Esc to cancel",
        ]));
        screen
    }

    #[test]
    fn an_mcp_server_dialog_reads_its_name_options_and_cursor() {
        let screen = read(&mcp_server_dialog());
        assert_eq!(screen.composer, ComposerState::Dialog);
        assert_eq!(
            labels(&screen),
            [
                (1, "Use this MCP server".to_owned()),
                (
                    2,
                    "Use this and all future MCP servers in this project".to_owned()
                ),
                (3, "Continue without using this MCP server".to_owned()),
            ]
        );
        let prompt = screen.prompt.expect("an MCP server prompt");
        assert_eq!(prompt.kind, ChatPromptKind::McpServer);
        assert_eq!(prompt.subject, "rt-driver");
        assert_eq!(prompt.body, "MCP servers may execute code.");
        assert_eq!(prompt.cursor, Some(3));
    }

    #[test]
    fn an_mcp_server_option_moves_the_cursor_then_enters() {
        let prompt = read(&mcp_server_dialog()).prompt.expect("a prompt");
        let pick = |number| recipe(&prompt, &ChatAnswerKind::Option { number }).map(|r| r.steps);
        assert_eq!(
            pick(1),
            Ok(vec![
                RecipeStep {
                    keys: vec![Key::Up, Key::Up],
                    expect: ScreenExpect::OptionSelected(1),
                },
                RecipeStep {
                    keys: vec![Key::Enter],
                    expect: ScreenExpect::DialogGone,
                },
            ])
        );
        assert_eq!(
            pick(3),
            Ok(vec![RecipeStep {
                keys: vec![Key::Enter],
                expect: ScreenExpect::DialogGone,
            }])
        );
    }

    #[test]
    fn a_trust_dialog_still_reads_as_trust_beside_the_mcp_parser() {
        let mut screen = vec![rule()];
        screen.extend(rows(&[
            " Accessing workspace:",
            "",
            " C:\\proj",
            "",
            " ❯ 1. Yes, I trust this folder",
            "   2. No, exit",
            "",
            " Enter to confirm · Esc to cancel",
        ]));
        let prompt = read(&screen).prompt.expect("a trust prompt");
        assert_eq!(prompt.kind, ChatPromptKind::Trust);
        assert_eq!(prompt.subject, "C:\\proj");
    }

    #[test]
    fn mcp_named_elsewhere_is_no_mcp_server_dialog() {
        let transcript = boxed(
            &[
                "● The MCP server rt-driver is set up.",
                "  New MCP server found in this project: rt-driver",
            ],
            &["> "],
        );
        let screen = read(&transcript);
        assert_eq!(screen.prompt, None, "a transcript row is no dialog");
        assert_ne!(screen.composer, ComposerState::Dialog);
    }

    #[test]
    fn an_mcp_server_dialog_without_its_footer_is_half_drawn() {
        let mut screen = mcp_server_dialog();
        screen.truncate(screen.len() - 2);
        assert_eq!(
            read(&screen),
            ChatScreen {
                composer: ComposerState::Dialog,
                prompt: None,
            }
        );
    }

    #[test]
    fn can_amend_is_off_without_tab_to_amend() {
        let screen = read(&permission(
            &[
                " ❯ 1. Yes, and tell Claude what to do next",
                "   2. Yes, and don’t ask again for: python *",
                "   3. No",
            ],
            Some(" Esc to cancel"),
        ));
        let prompt = screen.prompt.expect("a prompt");
        assert!(!prompt.can_amend);
        assert_eq!(
            prompt.options[0].label,
            "Yes, and tell Claude what to do next"
        );
        assert!(prompt.options[0].selected);
        assert_eq!(prompt.always_allow_tip.as_deref(), Some("python *"));
    }

    /// A Heads up notice with the key row `keys`, over an empty box.
    fn heads_up(keys: &str) -> Vec<String> {
        boxed(
            &[
                "                                     [-]",
                "✦ Heads up · A note says the drive gets",
                "  wiped nightly.",
                "",
                keys,
                "",
            ],
            &["❯"],
        )
    }

    #[test]
    fn a_heads_up_reads_two_or_three_key_pairs() {
        let three = read(&heads_up(
            "  1: Learn more   2: Knew this already   0: Dismiss",
        ));
        assert_eq!(three.composer, ComposerState::Ready);
        assert_eq!(
            labels(&three),
            [
                (1, "Learn more".to_owned()),
                (2, "Knew this already".to_owned()),
                (0, "Dismiss".to_owned()),
            ]
        );
        let prompt = three.prompt.expect("a prompt");
        assert_eq!(prompt.kind, ChatPromptKind::HeadsUp);
        assert_eq!(
            prompt.subject,
            "Heads up · A note says the drive gets wiped nightly."
        );
        assert_eq!(prompt.body, "");
        assert_eq!(prompt.cursor, None);
        let two = read(&heads_up("  1: Understood   0: Dismiss"));
        assert_eq!(
            labels(&two),
            [(1, "Understood".to_owned()), (0, "Dismiss".to_owned())]
        );
    }

    /// The rows of `band` over an empty input box.
    fn over_box(band: &[&str]) -> Vec<String> {
        let mut rows: Vec<String> = band.iter().map(|row| (*row).to_owned()).collect();
        rows.push("─".repeat(60));
        rows.push("❯ ".to_owned());
        rows.push("─".repeat(60));
        rows.push("  ⏵⏵ auto mode on (shift+tab to cycle)".to_owned());
        rows
    }

    /// The `(number, label)` of each option the screen in `rows` shows.
    fn labels_of(rows: &[String]) -> Vec<(u32, String)> {
        read(rows)
            .prompt
            .map(|prompt| {
                prompt
                    .options
                    .iter()
                    .map(|option| (option.number, option.label.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_heads_up_dismissed_step_reads_its_keys_at_every_wrap() {
        let keys: Vec<(u32, String)> = [
            (1, "That was helpful"),
            (2, "Not relevant"),
            (3, "Couldn’t understand"),
            (4, "Turn off suggestions"),
            (0, "Dismiss"),
        ]
        .map(|(number, label)| (number, label.to_owned()))
        .to_vec();
        for band in [
            &[
                "✦ Dismissed.   1: That was helpful   2: Not relevant   3: Couldn’t understand   4: Turn off suggestions   0: Dismiss",
            ][..],
            &[
                "✦ Dismissed.   1: That was helpful   2: Not relevant   3: Couldn’t understand   4: Turn off suggestions",
                "  0: Dismiss",
            ][..],
            &[
                "✦ Dismissed.   1: That was helpful   2: Not relevant   3: Couldn’t understand",
                "  4: Turn off suggestions   0: Dismiss",
            ][..],
            &[
                "✦ Dismissed.   1: That was helpful   2: Not relevant",
                "  3: Couldn’t understand   4: Turn off suggestions",
                "  0: Dismiss",
            ][..],
        ] {
            let rows = over_box(band);
            let screen = read(&rows);
            assert_eq!(screen.composer, ComposerState::Ready, "{band:?}");
            let prompt = screen.prompt.expect("the dismissed step is a prompt");
            assert_eq!(prompt.kind, ChatPromptKind::HeadsUp);
            assert_eq!(prompt.subject, "Dismissed.");
            assert_eq!(labels_of(&rows), keys, "{band:?}");
        }
        let after_explained =
            over_box(&["✦ Dismissed.   1: That was helpful   2: Didn’t understand   0: Dismiss"]);
        assert_eq!(
            labels_of(&after_explained),
            [
                (1, "That was helpful".to_owned()),
                (2, "Didn’t understand".to_owned()),
                (0, "Dismiss".to_owned())
            ]
        );
    }

    #[test]
    fn a_heads_up_key_row_wrapped_to_one_pair_keeps_it() {
        let rows = over_box(&[
            "✦ Heads up · A note says the drive gets wiped.",
            "  1: Learn more   2: Knew this already",
            "  0: Dismiss",
        ]);
        assert_eq!(
            labels_of(&rows),
            [
                (1, "Learn more".to_owned()),
                (2, "Knew this already".to_owned()),
                (0, "Dismiss".to_owned())
            ]
        );
    }

    #[test]
    fn a_you_should_know_tag_reads_as_a_heads_up() {
        let rows = over_box(&[
            "✦ You should know · A note says the drive gets wiped.",
            "  1: Learn more   2: Knew this already   0: Dismiss",
        ]);
        let prompt = read(&rows).prompt.expect("a prompt");
        assert_eq!(prompt.kind, ChatPromptKind::HeadsUp);
        assert_eq!(
            prompt.subject,
            "You should know · A note says the drive gets wiped."
        );
    }

    #[test]
    fn a_quoted_notice_above_a_dismissed_step_reads_the_step() {
        let rows = over_box(&[
            "✦ Heads up · quoted text",
            "",
            "✦ Dismissed.   1: That was helpful   2: Not relevant",
            "  0: Dismiss",
        ]);
        let prompt = read(&rows).prompt.expect("the dismissed step is a prompt");
        assert_eq!(prompt.subject, "Dismissed.");
        assert_eq!(
            labels_of(&rows),
            [
                (1, "That was helpful".to_owned()),
                (2, "Not relevant".to_owned()),
                (0, "Dismiss".to_owned())
            ]
        );
    }

    #[test]
    fn a_cut_off_offer_stays_no_prompt() {
        let rows = over_box(&[
            "✦ Heads up · A note says the drive gets wiped and more text follows on this row",
            "  and on this row too",
            "↓ 1 more",
        ]);
        assert!(read(&rows).prompt.is_none());
    }

    #[test]
    fn the_try_placeholder_reads_ready() {
        let screen = read(&boxed(
            &["✻ Baked for 22s"],
            &["❯ Try \"fix typecheck errors\""],
        ));
        assert_eq!(
            screen,
            ChatScreen {
                composer: ComposerState::Ready,
                prompt: None,
            }
        );
        let working = read(&boxed(&["· Cooking… (21s · ↓ 1.1k tokens)"], &["❯"]));
        assert_eq!(working.composer, ComposerState::Working);
        let typed = read(&boxed(&[], &["❯ fix the tests"]));
        assert_eq!(typed.composer, ComposerState::Draft);
    }

    /// The text the suggestion capture's box shows after its prompt.
    const SUGGESTION: &str = "write todo.py";

    /// An input box after a finished turn whose prompt row holds `text`, as
    /// the suggestion captures draw it, and the rows' dim cells: `text`
    /// drawn dim when `dim`, else none.
    fn styled_box(text: &str, dim: bool) -> (Vec<String>, Vec<String>) {
        let line = format!("❯ {text}");
        let screen = boxed(&["✻ Cogitated for 1s · done 10:53 PM"], &[&line]);
        let dim_rows = screen
            .iter()
            .map(|row| {
                if dim && *row == line {
                    format!("  {text}")
                } else {
                    String::new()
                }
            })
            .collect();
        (screen, dim_rows)
    }

    #[test]
    fn a_dim_suggestion_reads_as_a_suggestion_and_no_draft() {
        let (screen, dim) = styled_box(SUGGESTION, true);
        assert_eq!(
            read_styled(&screen, &dim),
            ChatScreen {
                composer: ComposerState::Suggestion {
                    text: SUGGESTION.to_owned()
                },
                prompt: None,
            }
        );
        assert_eq!(read_view_styled(&screen, &dim).box_first_line, None);
    }

    /// `dim` with the character at `column` of its one dim row blanked: a
    /// cell drawn not dim.
    fn undimmed(mut dim: Vec<String>, column: usize) -> Vec<String> {
        let row = dim
            .iter_mut()
            .find(|row| !row.is_empty())
            .expect("the box has a dim row");
        *row = row
            .chars()
            .enumerate()
            .map(|(at, c)| if at == column { ' ' } else { c })
            .collect();
        dim
    }

    #[test]
    fn a_suggestion_under_the_cursor_reads_as_a_suggestion() {
        // The agent's own cursor draws the suggestion's first cell not dim.
        let (screen, dim) = styled_box(SUGGESTION, true);
        let dim = undimmed(dim, 2);
        assert_eq!(
            read_styled(&screen, &dim).composer,
            ComposerState::Suggestion {
                text: SUGGESTION.to_owned()
            }
        );
        assert_eq!(read_view_styled(&screen, &dim).box_first_line, None);
    }

    #[test]
    fn a_later_character_not_drawn_dim_reads_as_a_draft() {
        let (screen, dim) = styled_box(SUGGESTION, true);
        let dim = undimmed(dim, 3);
        assert_eq!(read_styled(&screen, &dim).composer, ComposerState::Draft);
        assert_eq!(
            read_view_styled(&screen, &dim).box_first_line.as_deref(),
            Some(SUGGESTION)
        );
        let (screen, dim) = styled_box("ok", false);
        assert_eq!(read_styled(&screen, &dim).composer, ComposerState::Draft);
    }

    #[test]
    fn text_not_drawn_dim_reads_as_a_draft() {
        for typed in ["hello", "write todo.py and its tests", SUGGESTION] {
            let (screen, dim) = styled_box(typed, false);
            assert_eq!(
                read_styled(&screen, &dim).composer,
                ComposerState::Draft,
                "{typed}"
            );
            assert_eq!(
                read_view_styled(&screen, &dim).box_first_line.as_deref(),
                Some(typed)
            );
        }
    }

    #[test]
    fn the_dim_try_placeholder_still_reads_ready() {
        let (screen, dim) = styled_box("Try \"fix typecheck errors\"", true);
        assert_eq!(read_styled(&screen, &dim).composer, ComposerState::Ready);
        assert_eq!(read_view_styled(&screen, &dim).box_first_line, None);
    }

    #[test]
    fn the_unstyled_reading_of_a_suggestion_is_unchanged() {
        let (screen, _) = styled_box(SUGGESTION, true);
        assert_eq!(read(&screen).composer, ComposerState::Draft);
        assert_eq!(
            read_view(&screen).box_first_line.as_deref(),
            Some(SUGGESTION)
        );
    }

    #[test]
    fn spinner_above_reads_a_task_subject_spinner() {
        let above = rows(&[
            "✽ Settings form: validate the fields, then run the tests… (5m 3s · ↓ 8.3k tokens)",
            "  ⎿  ◼ Settings form: validate the fields, then run the tests",
            "     ✔ Read the form and its tests",
            "     ✔ Add the port check",
            "     ✔ Add the host check; show errors inline",
            "     ✔ Update the form's docs (two pages)",
            "      … +2 completed",
            "",
        ]);
        assert!(spinner_above(&above));
    }

    #[test]
    fn a_bare_bang_box_is_ready_and_a_bang_command_is_a_draft() {
        assert_eq!(read(&boxed(&[], &["!"])).composer, ComposerState::Ready);
        assert_eq!(read(&boxed(&[], &["! ls"])).composer, ComposerState::Draft);
    }

    /// An `AskUserQuestion` step with `options` under a tab bar of two
    /// questions, neither answered, then the rule, `Chat about this` and the
    /// footer.
    fn ask(options: &[&str]) -> Vec<String> {
        let mut screen = vec![rule()];
        screen.extend(rows(&[
            "←  ☐ Next fn  ☐ Tests  ✔ Submit  →",
            "",
            "Which tests?",
            "",
        ]));
        screen.extend(rows(options));
        screen.push(rule());
        screen.extend(rows(&[
            "  9. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]));
        screen
    }

    /// The prompt `screen` shows.
    fn question(screen: &[String]) -> ChatPrompt {
        read(screen).prompt.expect("a question prompt")
    }

    /// A question taller than its grid, as Claude Code 2.1.319 draws it at
    /// 100x24: the rule over its tab bar and the bar itself have scrolled off
    /// the top, so only the question's rows and its options are in view.
    #[test]
    fn a_question_taller_than_the_grid_reads_without_its_bar() {
        let mut screen = rows(&[
            "",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike no?",
            "",
            "❯ 1. An option label of some length 1",
            "     delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo",
            "     charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alph?",
            "  2. An option label of some length 2",
            "     golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo",
            "     foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie del?",
            "  3. An option label of some length 3",
            "     juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel",
            "     india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot g?",
            "  4. An option label of some length 4",
            "     mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo",
            "     lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india j?",
            "  5. Type something.",
        ]);
        screen.push("─".repeat(100));
        screen.extend(rows(&[
            "  6. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]));
        let screen = read(&screen);
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.as_ref().expect("a question prompt");
        assert_eq!(prompt.kind, ChatPromptKind::Question);
        assert!(prompt.questions.is_empty(), "the dialog has no bar to read");
        assert!(
            prompt.subject.ends_with("kilo lima mike no?"),
            "the visible rows are the question's end: {}",
            prompt.subject
        );
        assert_eq!(
            labels(&screen),
            [
                (1, "An option label of some length 1".to_owned()),
                (2, "An option label of some length 2".to_owned()),
                (3, "An option label of some length 3".to_owned()),
                (4, "An option label of some length 4".to_owned()),
                (5, "Type something".to_owned()),
            ]
        );
    }

    /// The same tall question at 40 columns, where Claude Code wraps its
    /// footer onto a second row: the `Esc` ends the first row and `to cancel`
    /// starts the next, and the barless dialog still reads.
    #[test]
    fn a_tall_question_whose_footer_wraps_at_40_columns_reads_without_its_bar() {
        let mut screen = rows(&[
            "",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike no?",
            "",
            "❯ 1. An option label of some length 1",
            "     delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo",
            "     charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alph?",
            "  2. An option label of some length 2",
            "     golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo",
            "     foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie del?",
            "  3. An option label of some length 3",
            "     juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel",
            "     india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot g?",
            "  4. An option label of some length 4",
            "     mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo",
            "     lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india j?",
            "  5. Type something.",
        ]);
        screen.push("─".repeat(100));
        screen.extend(rows(&[
            "  6. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc",
            "to cancel",
        ]));
        let screen = read(&screen);
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.as_ref().expect("a question prompt");
        assert!(prompt.questions.is_empty(), "the dialog has no bar to read");
        assert_eq!(
            labels(&screen),
            [
                (1, "An option label of some length 1".to_owned()),
                (2, "An option label of some length 2".to_owned()),
                (3, "An option label of some length 3".to_owned()),
                (4, "An option label of some length 4".to_owned()),
                (5, "Type something".to_owned()),
            ]
        );
    }

    /// The same tall question at 48 columns, where the first footer row ends
    /// `… · Esc to` and the second brings `cancel`.
    #[test]
    fn a_tall_question_whose_footer_wraps_at_48_columns_reads_without_its_bar() {
        let mut screen = rows(&[
            "",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike no?",
            "",
            "❯ 1. An option label of some length 1",
            "     delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo",
            "     charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alph?",
            "  2. An option label of some length 2",
            "     golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo",
            "     foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie del?",
            "  3. An option label of some length 3",
            "     juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel",
            "     india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot g?",
            "  4. An option label of some length 4",
            "     mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo",
            "     lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india j?",
            "  5. Type something.",
        ]);
        screen.push("─".repeat(100));
        screen.extend(rows(&[
            "  6. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to",
            "cancel",
        ]));
        let screen = read(&screen);
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.as_ref().expect("a question prompt");
        assert!(prompt.questions.is_empty(), "the dialog has no bar to read");
        assert_eq!(
            labels(&screen),
            [
                (1, "An option label of some length 1".to_owned()),
                (2, "An option label of some length 2".to_owned()),
                (3, "An option label of some length 3".to_owned()),
                (4, "An option label of some length 4".to_owned()),
                (5, "Type something".to_owned()),
            ]
        );
    }

    /// A multi-question card's footer wrapped the same way: its first row
    /// keeps `Tab`, so the barless dialog stays unread.
    #[test]
    fn a_tall_multi_question_footer_that_wraps_stays_unread() {
        let mut screen = rows(&[
            "",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha",
            "│ bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike no?",
            "",
            "❯ 1. An option label of some length 1",
            "     delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo",
            "     charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa alph?",
            "  2. An option label of some length 2",
            "     golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo",
            "     foxtrot golf hotel india juliet kilo lima mike november oscar papa alpha bravo charlie del?",
            "  3. An option label of some length 3",
            "     juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel",
            "     india juliet kilo lima mike november oscar papa alpha bravo charlie delta echo foxtrot g?",
            "  4. An option label of some length 4",
            "     mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo",
            "     lima mike november oscar papa alpha bravo charlie delta echo foxtrot golf hotel india j?",
            "  5. Type something.",
        ]);
        screen.push("─".repeat(100));
        screen.extend(rows(&[
            "  6. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc",
            "to cancel",
        ]));
        assert!(
            read(&screen).prompt.is_none(),
            "a wrapped multi-question footer stays unread"
        );
    }

    /// A multi-question card whose bar and the rows above it have scrolled
    /// off the top: its footer still names `Tab`, so it stays unread without
    /// its bar.
    #[test]
    fn a_multi_question_card_without_its_bar_reads_no_prompt() {
        let wide = rule();
        let screen = read(&rows(&[
            "Which function should be added to calc.py next?",
            "",
            "❯ 1. sub",
            "     Subtraction: returns a - b",
            "  2. div",
            "     Division: returns a / b (needs a decision on division by zero)",
            "  3. pow",
            "     Exponentiation: returns a ** b",
            "  4. Type something.",
            wide.as_str(),
            "  5. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]));
        assert!(
            screen.prompt.is_none(),
            "a multi-question card stays unread without its bar"
        );
    }

    /// A reply that quotes a dialog's footer in the transcript, above an
    /// idle input box: a footer alone is not a question.
    #[test]
    fn a_quoted_question_footer_in_an_idle_transcript_reads_no_question() {
        let mut screen = rows(&[
            "A reply that quotes a dialog:",
            "",
            "  1. Numbered point",
            "  2. Another",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]);
        screen.push(rule());
        screen.extend(rows(&["❯", ""]));
        screen.push(rule());
        screen.extend(rows(&["? for shortcuts"]));
        let screen = read(&screen);
        assert!(screen.prompt.is_none(), "a quoted footer is not a dialog");
        assert_eq!(screen.composer, ComposerState::Ready);
    }

    /// The same quoted footer with a bash-mode input box under it: the box
    /// draws `!` where the cursor goes, and is still a box, so the footer was
    /// quoted.
    #[test]
    fn a_quoted_question_footer_above_a_bash_mode_box_reads_no_question() {
        let mut screen = rows(&[
            "❯ quote the dialog footer",
            "",
            "A reply that quotes a dialog:",
            "",
            "  1. First point",
            "  2. Second point",
            "",
        ]);
        screen.push(rule());
        screen.extend(rows(&[
            "  9. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]));
        screen.push(rule());
        screen.extend(rows(&["!", ""]));
        screen.push(rule());
        screen.extend(rows(&["! for shell mode"]));
        let screen = read(&screen);
        assert!(screen.prompt.is_none(), "a quoted footer is not a dialog");
        assert_eq!(screen.composer, ComposerState::Ready);
    }

    /// A select list's footer says `Esc to go back`, not `Esc to cancel`, so
    /// it is not a question.
    #[test]
    fn a_select_list_whose_esc_goes_back_reads_no_question() {
        let screen = read(&rows(&[
            "Select a folder",
            "",
            "❯ 1. repo",
            "  2. scratch",
            "",
            "Enter to select · Esc to go back",
        ]));
        assert!(
            screen.prompt.is_none(),
            "a select list's footer is not a question's"
        );
    }

    /// A single question one row taller than its grid: only the rule over
    /// its tab bar has scrolled off, so the bar is the grid's first row and
    /// is not part of the question's text.
    #[test]
    fn a_question_with_only_its_top_rule_cut_off_reads_its_text() {
        let mut screen = rows(&[
            " ☐ Colour",
            "",
            "Which colour should the badge use?",
            "",
            "❯ 1. Red",
            "  2. Blue",
            "  3. Type something.",
        ]);
        screen.push(rule());
        screen.extend(rows(&[
            "  4. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]));
        let screen = read(&screen);
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.as_ref().expect("a question prompt");
        assert_eq!(prompt.subject, "Which colour should the badge use?");
        assert!(
            prompt.questions.is_empty(),
            "the bar is not read without its rule"
        );
    }

    /// With the vim editor mode on, the cursor on `Type something` heads the
    /// footer with `-- INSERT --` and drops its `Esc to cancel`, as Claude
    /// Code 2.1.322 draws it; the question still reads, with its bar and
    /// without it.
    #[test]
    fn a_question_in_vim_insert_mode_reads_its_options() {
        let dialog = rows(&[
            "Which color?",
            "",
            "  1. Red",
            "     Pick red.",
            "  2. Blue",
            "     Pick blue.",
            "❯ 3. Type something.",
            &rule(),
            "  4. Chat about this",
            "",
            "-- INSERT -- · Enter to select · ↑/↓ to navigate · ctrl+g to edit in Notepad",
        ]);
        let mut with_bar = vec![rule(), " ☐ Color".to_owned(), String::new()];
        with_bar.extend(dialog.iter().cloned());
        for (name, screen) in [("with bar", with_bar), ("without bar", dialog)] {
            let screen = read(&screen);
            assert_eq!(screen.composer, ComposerState::Dialog, "{name}");
            let prompt = screen.prompt.as_ref().expect("a question prompt");
            assert_eq!(prompt.subject, "Which color?", "{name}");
            assert_eq!(
                labels(&screen)[..2],
                [(1, "Red".to_owned()), (2, "Blue".to_owned())],
                "{name}"
            );
        }
    }

    /// A narrow pane wraps the vim insert mode's footer: at 30 columns its
    /// head row ends at `Enter to select`, its tail on the rows under it,
    /// and the question still reads with its options.
    #[test]
    fn a_vim_question_wrapped_at_30_columns_reads_its_options() {
        let dialog = rows(&[
            "Which shape?",
            "",
            "  1. Square",
            "     Pick square.",
            "  2. Circle",
            "     Pick circle.",
            "❯ 3. Type something.",
            "──────────────────────────────",
            "  4. Chat about this",
            "",
            "-- INSERT -- · Enter to select",
            "· ↑/↓ to navigate · ctrl+g to",
            "edit in Notepad",
        ]);
        let mut with_bar = vec![rule(), " ☐ Shape".to_owned(), String::new()];
        with_bar.extend(dialog.iter().cloned());
        for (name, screen) in [("with bar", with_bar), ("without bar", dialog)] {
            let screen = read(&screen);
            assert_eq!(screen.composer, ComposerState::Dialog, "{name}");
            let prompt = screen.prompt.as_ref().expect("a question prompt");
            assert_eq!(prompt.subject, "Which shape?", "{name}");
            assert_eq!(
                labels(&screen)[..2],
                [(1, "Square".to_owned()), (2, "Circle".to_owned())],
                "{name}"
            );
        }
    }

    /// At 24 columns the head row ends at `Enter to`, its `select` opening
    /// the wrapped row under it, and the question still reads.
    #[test]
    fn a_vim_question_wrapped_at_24_columns_reads_its_options() {
        let dialog = rows(&[
            "Which shape?",
            "",
            "  1. Square",
            "     Pick square.",
            "  2. Circle",
            "     Pick circle.",
            "❯ 3. Type something.",
            "────────────────────────",
            "  4. Chat about this",
            "",
            "-- INSERT -- · Enter to",
            "select · ↑/↓ to navigate",
            "· ctrl+g to edit in",
            "Notepad",
        ]);
        let mut with_bar = vec![rule(), " ☐ Shape".to_owned(), String::new()];
        with_bar.extend(dialog.iter().cloned());
        for (name, screen) in [("with bar", with_bar), ("without bar", dialog)] {
            let screen = read(&screen);
            assert_eq!(screen.composer, ComposerState::Dialog, "{name}");
            let prompt = screen.prompt.as_ref().expect("a question prompt");
            assert_eq!(prompt.subject, "Which shape?", "{name}");
            assert_eq!(
                labels(&screen)[..2],
                [(1, "Square".to_owned()), (2, "Circle".to_owned())],
                "{name}"
            );
        }
    }

    #[test]
    fn a_description_wrapped_over_two_rows_is_joined() {
        let screen = read(&ask(&[
            "❯ 1. sub",
            "     Subtraction: returns a - b, and",
            "     raises on overflow",
            "  2. Type something.",
        ]));
        assert_eq!(screen.composer, ComposerState::Dialog);
        let prompt = screen.prompt.expect("a question prompt");
        assert_eq!(prompt.kind, ChatPromptKind::Question);
        assert_eq!(prompt.subject, "Which tests?");
        assert_eq!(prompt.body, "");
        assert_eq!(
            prompt.options[0].description.as_deref(),
            Some("Subtraction: returns a - b, and raises on overflow")
        );
        assert_eq!(prompt.options[1].label, "Type something");
        assert_eq!(prompt.options[1].description, None);
    }

    #[test]
    fn a_question_wrapped_over_two_rows_keeps_every_row() {
        let mut screen = vec![rule()];
        screen.extend(rows(&[
            "←  ☐ Next fn  ☐ Tests  ✔ Submit  →",
            "",
            "Which tests should run before the release goes out to the",
            "users on the beta channel?",
            "",
            "❯ 1. sub",
            "     Subtraction",
            "  2. Type something.",
        ]));
        screen.push(rule());
        screen.extend(rows(&[
            "  9. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]));
        let prompt = question(&screen);
        assert_eq!(
            prompt.subject,
            "Which tests should run before the release goes out to the users on the beta channel?"
        );
        assert_eq!(
            labels(&read(&screen)),
            [(1, "sub".to_owned()), (2, "Type something".to_owned())]
        );
        assert_eq!(
            prompt.options[0].description.as_deref(),
            Some("Subtraction")
        );
    }

    /// A question card `width` columns wide: its rule and tab bar, the
    /// `question` rows, the `options` rows, its inner rule and its footer.
    fn card(width: usize, question: &[&str], options: &[&str]) -> Vec<String> {
        for row in question.iter().chain(options) {
            assert!(row.chars().count() <= width, "{row:?} fits the card");
        }
        let rule = "─".repeat(width);
        let mut screen = vec![rule.clone()];
        screen.extend(rows(&["←  ☐ Next fn  ☐ Tests  ✔ Submit  →", ""]));
        screen.extend(rows(question));
        screen.push(String::new());
        screen.extend(rows(options));
        screen.push(rule);
        screen.extend(rows(&[
            "  9. Chat about this",
            "",
            "Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]));
        screen
    }

    #[test]
    fn a_path_wrapped_mid_segment_joins_without_a_space() {
        let upper = "See X:\\dev\\rustling-tulip\\city-142-ui-mo";
        assert_eq!(upper.chars().count(), 40, "the row fills the card");
        let screen = card(
            40,
            &[upper, "cks.tmp\\x.md now?"],
            &["❯ 1. Yes", "  2. Type something."],
        );
        assert_eq!(
            question(&screen).subject,
            "See X:\\dev\\rustling-tulip\\city-142-ui-mocks.tmp\\x.md now?"
        );
    }

    #[test]
    fn a_url_wrapped_at_the_row_end_joins_without_a_space() {
        let upper = "see https://example.com/very/long/pa";
        let width = upper.chars().count();
        let screen = card(
            width,
            &[upper, "th/page.html"],
            &["❯ 1. Yes", "  2. Type something."],
        );
        assert_eq!(
            question(&screen).subject,
            "see https://example.com/very/long/path/page.html"
        );
    }

    #[test]
    fn a_path_on_a_short_row_keeps_the_space() {
        let options = ["❯ 1. Yes", "  2. Type something."];
        let full = "Then open the old notes at C:\\a\\b.md and";
        assert_eq!(full.chars().count(), 40, "the row fills the card");
        for (upper, joined) in [
            ("Open C:\\a\\b.md and", "Open C:\\a\\b.md and then save"),
            (full, "Then open the old notes at C:\\a\\b.md and then save"),
            ("Keep C:\\a\\b.md", "Keep C:\\a\\b.md then save"),
        ] {
            let screen = card(40, &[upper, "then save"], &options);
            assert_eq!(question(&screen).subject, joined, "{upper:?}");
        }
    }

    #[test]
    fn a_short_path_ending_a_full_row_keeps_its_space() {
        let upper = "Should I archive the notes at C:\\a\\b.md";
        assert_eq!(
            upper.chars().count(),
            39,
            "the row fills the card within one cell"
        );
        let screen = card(40, &[upper, "now?"], &["❯ 1. Yes", "  2. Type something."]);
        assert_eq!(
            question(&screen).subject,
            "Should I archive the notes at C:\\a\\b.md now?"
        );
    }

    #[test]
    fn a_review_row_ending_in_a_short_path_keeps_its_space() {
        let upper = " ● Should I archive the old notes in X:\\dev\\a\\notes.md";
        assert_eq!(upper.chars().count(), 54, "the row fills the dialog");
        let body = review_body(&rows(&[upper, "   first?", "   → Yes"]), 54);
        assert_eq!(
            body,
            "Should I archive the old notes in X:\\dev\\a\\notes.md first? -> Yes"
        );
    }

    #[test]
    fn an_option_description_wrapped_mid_path_joins_without_a_space() {
        let upper = "     See X:\\dev\\rustling-tulip\\city-1-mo";
        assert_eq!(upper.chars().count(), 40, "the row fills the card");
        let screen = read(&ask(&[
            "❯ 1. sub",
            upper,
            "     cks.tmp\\x.md",
            "  2. Type something.",
        ]));
        let prompt = screen.prompt.expect("a question prompt");
        assert_eq!(
            prompt.options[0].description.as_deref(),
            Some("See X:\\dev\\rustling-tulip\\city-1-mocks.tmp\\x.md")
        );
    }

    #[test]
    fn a_full_width_row_ending_in_and_or_keeps_its_space() {
        let body = review_body(&rows(&[" ● see and/or", "   more text"]), 11);
        assert_eq!(body, "see and/or more text ->");
    }

    #[test]
    fn is_path_token_reads_paths_and_addresses_but_not_prose() {
        for path in [
            "/usr/bin",
            "./a",
            "../a",
            "~/a",
            "a/b/c",
            "C:\\x",
            "https://x.y/z",
            "docs\\a",
        ] {
            assert!(is_path_token(path), "{path:?} is a path or web address");
        }
        for prose in ["and/or", "yes/no", "a/b", "path/."] {
            assert!(!is_path_token(prose), "{prose:?} is prose");
        }
    }

    #[test]
    fn a_long_relative_path_split_after_its_first_segment_rejoins() {
        let upper = "Should I archive the old notes in docs/p";
        assert_eq!(upper.chars().count(), 40, "the row fills the card");
        let screen = card(
            41,
            &[upper, "lans/archive/native-client-parity.md now?"],
            &["❯ 1. Yes", "  2. Type something."],
        );
        assert_eq!(
            question(&screen).subject,
            "Should I archive the old notes in docs/plans/archive/native-client-parity.md now?"
        );
    }

    #[test]
    fn a_url_split_inside_its_scheme_rejoins() {
        let body = review_body(&rows(&[" ● see https:/", "   /example.com/a/b/c"]), 15);
        assert_eq!(body, "see https://example.com/a/b/c ->");
    }

    #[test]
    fn chat_about_this_is_not_an_option() {
        let screen = read(&ask(&["❯ 1. sub", "  2. div", "  3. Type something."]));
        assert_eq!(
            labels(&screen),
            [
                (1, "sub".to_owned()),
                (2, "div".to_owned()),
                (3, "Type something".to_owned()),
            ]
        );
    }

    #[test]
    fn a_lone_short_rule_row_is_not_a_rule() {
        let wide = format!("  {}", rule());
        let mut screen = vec![wide.clone()];
        screen.extend(rows(&[
            "  ──",
            "  ←  ☐ Next fn  ☒ Tests  ✔ Submit  →",
            "",
            "  Which function?",
            "",
            "  ❯ 1. sub",
            "       Subtraction",
            "    2. Type something.",
        ]));
        screen.push(wide);
        screen.extend(rows(&[
            "  ──",
            "    3. Chat about this",
            "",
            "  Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]));
        let screen = read(&screen);
        assert_eq!(
            labels(&screen),
            [(1, "sub".to_owned()), (2, "Type something".to_owned())]
        );
        let prompt = screen.prompt.expect("a question prompt");
        assert_eq!(prompt.subject, "Which function?");
        assert_eq!(
            prompt.options[0].description.as_deref(),
            Some("Subtraction")
        );
        assert_eq!(prompt.cursor, Some(1));
        let tabs: Vec<(&str, bool, bool)> = prompt
            .questions
            .iter()
            .map(|tab| (tab.label.as_str(), tab.answered, tab.current))
            .collect();
        assert_eq!(tabs, [("Next fn", false, true), ("Tests", true, false)]);
    }

    #[test]
    fn a_multi_select_with_the_cursor_on_next_has_no_cursor() {
        let screen = read(&ask(&[
            "  1. [✔] add",
            "         Unit tests for add",
            "  2. [ ] mul",
            "  3. [✔] fuzz tests",
            "❯    Next",
        ]));
        assert_eq!(
            labels(&screen),
            [
                (1, "add".to_owned()),
                (2, "mul".to_owned()),
                (3, "fuzz tests".to_owned()),
            ]
        );
        let prompt = screen.prompt.expect("a question prompt");
        assert!(prompt.multi);
        assert_eq!(prompt.cursor, None);
        let ticked: Vec<bool> = prompt
            .options
            .iter()
            .map(|option| option.selected)
            .collect();
        assert_eq!(ticked, [true, false, true]);
        assert_eq!(
            prompt.options[0].description.as_deref(),
            Some("Unit tests for add")
        );
    }

    #[test]
    fn submit_moves_down_from_the_cursor_to_next() {
        let prompt = question(&ask(&[
            "  1. [ ] a",
            "❯ 2. [ ] b",
            "  3. [ ] c",
            "  4. [ ] d",
            "  5. [ ] Type something",
            "     Next",
        ]));
        let steps = recipe(&prompt, &ChatAnswerKind::Submit)
            .expect("a recipe")
            .steps;
        assert_eq!(
            steps,
            [
                RecipeStep {
                    keys: vec![Key::Down; 4],
                    expect: ScreenExpect::SameDialog,
                },
                RecipeStep {
                    keys: vec![Key::Enter],
                    expect: ScreenExpect::Any,
                },
            ]
        );
    }

    #[test]
    fn type_something_with_the_cursor_on_its_row_only_types() {
        let prompt = question(&ask(&["  1. sub", "  2. div", "❯ 3. Type something."]));
        let answer = ChatAnswerKind::TypeSomething {
            text: "x".to_owned(),
        };
        let steps = recipe(&prompt, &answer).expect("a recipe").steps;
        assert_eq!(
            steps,
            [
                RecipeStep {
                    keys: vec![Key::Text("i".to_owned())],
                    expect: ScreenExpect::Any,
                },
                RecipeStep {
                    keys: vec![Key::Backspace],
                    expect: ScreenExpect::Any,
                },
                RecipeStep {
                    keys: vec![Key::Text("x".to_owned()), Key::Enter],
                    expect: ScreenExpect::Any,
                },
            ]
        );
    }

    /// A vim editor mode reads raw keys as commands in NORMAL mode, so the
    /// answer types `i` and a Backspace to enter INSERT before the text.
    #[test]
    fn a_typed_answer_enters_vims_insert_mode_first() {
        let prompt = question(&ask(&["❯ 1. sub", "  2. div", "  3. Type something."]));
        let answer = ChatAnswerKind::TypeSomething {
            text: "x".to_owned(),
        };
        let steps = recipe(&prompt, &answer).expect("a recipe").steps;
        assert_eq!(
            steps,
            [
                RecipeStep {
                    keys: vec![Key::Down, Key::Down],
                    expect: ScreenExpect::OptionSelected(3),
                },
                RecipeStep {
                    keys: vec![Key::Text("i".to_owned())],
                    expect: ScreenExpect::Any,
                },
                RecipeStep {
                    keys: vec![Key::Backspace],
                    expect: ScreenExpect::Any,
                },
                RecipeStep {
                    keys: vec![Key::Text("x".to_owned()), Key::Enter],
                    expect: ScreenExpect::Any,
                },
            ]
        );
    }

    #[test]
    fn a_digit_is_refused_while_the_cursor_is_on_type_something() {
        let prompt = question(&ask(&["  1. sub", "  2. div", "❯ 3. Type something."]));
        assert_eq!(
            recipe(&prompt, &ChatAnswerKind::Option { number: 2 }),
            Err(AnswerRefusal::NotThisDialog)
        );
    }

    #[test]
    fn a_thousand_empty_rows_read_as_unknown() {
        let empty = vec![String::new(); 1000];
        assert_eq!(
            read(&empty),
            ChatScreen {
                composer: ComposerState::Unknown,
                prompt: None,
            }
        );
    }

    /// A reading with no dialog and `panel_len` panel rows (`main` first).
    fn view(
        key_holder: KeyHolder,
        agent: bool,
        panel_cursor: Option<usize>,
        panel_in_view: Option<usize>,
        panel_len: usize,
    ) -> ViewReading {
        let panel = (0..panel_len)
            .map(|row| {
                if row == 0 {
                    "main".to_owned()
                } else {
                    format!("general-purpose  Agent {row}")
                }
            })
            .collect();
        ViewReading {
            agent_in_view: agent.then(|| "Agent 1".to_owned()),
            key_holder,
            stashed: false,
            box_first_line: None,
            dialog: false,
            panel,
            panel_cursor,
            panel_in_view,
        }
    }

    /// One case of a goal: `(holder, agent, cursor, in view, panel length,
    /// step)`.
    type StepCase = (
        KeyHolder,
        bool,
        Option<usize>,
        Option<usize>,
        usize,
        ViewStep,
    );

    /// Checks one goal's cases.
    fn check_steps(goal: ViewGoal, cases: &[StepCase]) {
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(holder, agent, cursor, in_view, len, want)| {
                let reading = view(*holder, *agent, *cursor, *in_view, *len);
                let got = next_view_key(goal, &reading);
                (got != *want).then(|| format!("{reading:?}: wanted {want:?}, got {got:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{goal:?}:\n{}", wrong.join("\n"));
    }

    #[test]
    fn view_key_refuses_while_a_dialog_is_up() {
        for goal in [ViewGoal::Main, ViewGoal::Agent { row: 1 }] {
            for holder in [KeyHolder::Box, KeyHolder::Panel, KeyHolder::TasksPill] {
                let reading = ViewReading {
                    dialog: true,
                    ..view(holder, true, Some(1), Some(1), 2)
                };
                assert_eq!(
                    next_view_key(goal, &reading),
                    ViewStep::Refuse(ViewRefusal::DialogUp),
                    "{goal:?} {holder:?}"
                );
            }
        }
    }

    #[test]
    fn view_key_returns_to_main() {
        use KeyHolder::{Box, Panel, TasksPill};
        use ViewStep::{Done, Press, Refuse};
        check_steps(
            ViewGoal::Main,
            &[
                (Box, false, None, Some(0), 2, Done),
                (Box, true, None, Some(1), 2, Press(Key::Down)),
                (TasksPill, false, None, Some(0), 2, Press(Key::Up)),
                (TasksPill, true, None, Some(1), 2, Press(Key::Down)),
                (Panel, false, Some(0), Some(0), 2, Press(Key::Up)),
                (Panel, true, Some(1), Some(0), 2, Press(Key::Up)),
                (Panel, true, Some(0), Some(1), 2, Press(Key::Enter)),
                (Panel, true, Some(2), Some(1), 3, Press(Key::Up)),
                (Panel, true, None, Some(1), 2, Refuse(ViewRefusal::NoPanel)),
            ],
        );
    }

    #[test]
    fn view_key_opens_an_agent() {
        use KeyHolder::{Box, Panel, TasksPill};
        use ViewStep::{Done, Press, Refuse};
        check_steps(
            ViewGoal::Agent { row: 1 },
            &[
                (
                    Box,
                    false,
                    None,
                    Some(0),
                    1,
                    Refuse(ViewRefusal::RowMissing),
                ),
                (
                    Panel,
                    false,
                    Some(0),
                    Some(0),
                    1,
                    Refuse(ViewRefusal::RowMissing),
                ),
                (Box, false, None, None, 0, Press(Key::Down)),
                (Box, true, None, Some(1), 2, Done),
                (Box, false, None, Some(1), 2, Press(Key::Down)),
                (Box, true, None, Some(2), 3, Press(Key::Down)),
                (Box, false, None, Some(0), 2, Press(Key::Down)),
                (TasksPill, true, None, Some(1), 2, Press(Key::Up)),
                (TasksPill, false, None, Some(0), 2, Press(Key::Down)),
                (Panel, true, Some(1), Some(1), 2, Press(Key::Up)),
                (Panel, false, Some(1), Some(0), 2, Press(Key::Enter)),
                (Panel, false, Some(0), Some(0), 2, Press(Key::Down)),
                (Panel, true, Some(2), Some(2), 3, Press(Key::Up)),
                (Panel, true, Some(2), Some(0), 3, Press(Key::Up)),
                (Panel, false, None, Some(0), 2, Refuse(ViewRefusal::NoPanel)),
            ],
        );
    }

    /// Why `step` breaks the key bounds for a reading whose keys `holder`
    /// holds, or `None` when it keeps them.
    fn out_of_bounds(step: &ViewStep, holder: KeyHolder) -> Option<&'static str> {
        let ViewStep::Press(key) = step else {
            return None;
        };
        match key {
            Key::Left | Key::Stash | Key::Digit(_) | Key::Tab | Key::Text(_) => {
                Some("a key no view step sends")
            }
            Key::Enter if holder != KeyHolder::Panel => Some("Enter outside the panel"),
            Key::Up if holder == KeyHolder::Box => Some("Up from the box"),
            _ => None,
        }
    }

    #[test]
    fn view_keys_never_leave_their_bounds() {
        let positions = [None, Some(0), Some(1), Some(2), Some(3)];
        let mut wrong = Vec::new();
        for goal in [
            ViewGoal::Main,
            ViewGoal::Agent { row: 1 },
            ViewGoal::Agent { row: 2 },
        ] {
            for holder in [KeyHolder::Box, KeyHolder::Panel, KeyHolder::TasksPill] {
                for agent in [false, true] {
                    for cursor in positions {
                        for in_view in positions {
                            for len in 0..=4 {
                                let reading = view(holder, agent, cursor, in_view, len);
                                let step = next_view_key(goal, &reading);
                                if let Some(why) = out_of_bounds(&step, holder) {
                                    wrong.push(format!("{goal:?} {reading:?} -> {step:?}: {why}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_draft_of_two_lines_keeps_its_break() {
        let screen = draft_box(
            40,
            &rows(&["❯ add a done command", "  then a list command"]),
            5,
        );
        let draft = draft_of(&screen).expect("a draft");
        assert_eq!(draft.text, "add a done command\nthen a list command");
        assert_eq!(draft.rows, 2);
    }

    #[test]
    fn a_row_one_over_the_wrap_width_runs_on() {
        let long = "x".repeat(21);
        let first = format!("❯ {long}");
        let screen = draft_box(30, &rows(&[first.as_str(), "  items more"]), 5);
        assert_eq!(
            draft_of(&screen).expect("a draft").text,
            format!("{long} items more")
        );
    }

    #[test]
    fn a_row_at_the_wrap_width_is_a_new_line() {
        let long = "x".repeat(21);
        let first = format!("❯ {long}");
        let screen = draft_box(30, &rows(&[first.as_str(), "  item more"]), 5);
        assert_eq!(
            draft_of(&screen).expect("a draft").text,
            format!("{long}\nitem more")
        );
    }

    #[test]
    fn a_blank_row_inside_a_draft_stays_blank() {
        let screen = draft_box(40, &rows(&["❯ a", "", "  b"]), 6);
        let draft = draft_of(&screen).expect("a draft");
        assert_eq!(draft.text, "a\n\nb");
        assert_eq!(draft.rows, 3);
    }

    #[test]
    fn a_draft_keeps_the_users_own_indentation() {
        let screen = draft_box(40, &rows(&["❯ fn x() {", "      y"]), 5);
        assert_eq!(draft_of(&screen).expect("a draft").text, "fn x() {\n    y");
    }

    #[test]
    fn a_placeholder_is_no_draft() {
        let screen = draft_box(40, &rows(&["❯ Try \"fix the tests\""]), 4);
        assert_eq!(draft_of(&screen), None);
    }

    #[test]
    fn an_empty_box_is_no_draft() {
        let screen = draft_box(40, &rows(&["❯"]), 4);
        assert_eq!(draft_of(&screen), None);
    }

    #[test]
    fn bash_mode_is_no_draft() {
        let screen = draft_box(40, &rows(&["! ls -la"]), 4);
        assert_eq!(draft_of(&screen), None);
    }

    #[test]
    fn an_image_placeholder_reads_as_pasted() {
        let screen = draft_box(40, &rows(&["❯ [Image #1] what colour"]), 4);
        assert!(draft_of(&screen).expect("a draft").pasted);
    }

    #[test]
    fn a_tall_box_may_show_only_its_end() {
        // 40 rows of screen: the box may show 15 of them.
        assert!(!whole_of(&draft_lines(15), 40));
        assert!(whole_of(&draft_lines(14), 40));
        // Its last row blank still counts towards the box's height.
        let mut blank_tail = draft_lines(14);
        blank_tail.push(String::new());
        assert_eq!(rows_of(&blank_tail, 40), 15);
        assert!(!whole_of(&blank_tail, 40));
        // 24 rows: the box's own cap, 7, is the lower of the two.
        assert!(!whole_of(&draft_lines(7), 24));
        assert!(whole_of(&draft_lines(6), 24));
        // 10 rows: the cap never drops under 3.
        assert!(!whole_of(&draft_lines(3), 10));
        assert!(whole_of(&draft_lines(2), 10));
    }

    /// A box's rows holding `text_rows` lines of text, the cursor on the
    /// first.
    fn draft_lines(text_rows: usize) -> Vec<String> {
        std::iter::once("❯ line 1".to_owned())
            .chain((2..=text_rows).map(|n| format!("  line {n}")))
            .collect()
    }

    /// Whether the box `body` holds in a screen `screen_rows` tall reads as
    /// whole.
    fn whole_of(body: &[String], screen_rows: usize) -> bool {
        draft_of(&draft_box(40, body, screen_rows))
            .expect("a draft")
            .whole
    }

    /// How many rows the box `body` holds in a screen `screen_rows` tall
    /// reads as taking.
    fn rows_of(body: &[String], screen_rows: usize) -> usize {
        draft_of(&draft_box(40, body, screen_rows))
            .expect("a draft")
            .rows
    }

    #[test]
    fn clearing_three_rows_sends_eight_line_clears() {
        let draft = DraftReading {
            text: "a\nb\nc".to_owned(),
            pasted: false,
            whole: true,
            rows: 3,
        };
        let steps = clear_draft_recipe(&draft).expect("a recipe").steps;
        assert_eq!(steps.len(), 8);
        for step in &steps {
            assert_eq!(step.keys, vec![Key::ClearLine]);
            assert_eq!(step.expect, ScreenExpect::Any);
        }
    }

    #[test]
    fn an_indented_row_is_never_a_wrap() {
        let screen = draft_box(30, &rows(&["❯ check the condition:", "      return"]), 5);
        assert_eq!(
            draft_of(&screen).expect("a draft").text,
            "check the condition:\n    return"
        );
    }

    #[test]
    fn a_paste_split_across_rows_reads_as_pasted() {
        let screen = draft_box(44, &rows(&["❯ fix [Pasted", "  text #1 +4 lines] now"]), 5);
        let draft = draft_of(&screen).expect("a draft");
        assert_eq!(draft.text, "fix [Pasted\ntext #1 +4 lines] now");
        assert!(draft.pasted);
    }

    #[test]
    fn only_a_draft_the_box_cannot_show_whole_is_refused_a_clear() {
        let pasted = DraftReading {
            text: String::new(),
            pasted: true,
            whole: true,
            rows: 1,
        };
        assert_eq!(
            clear_draft_recipe(&pasted).map(|recipe| recipe.steps.len()),
            Ok(4)
        );
        let scrolled = DraftReading {
            text: String::new(),
            pasted: false,
            whole: false,
            rows: 15,
        };
        assert_eq!(
            clear_draft_recipe(&scrolled),
            Err(AnswerRefusal::NotThisDialog)
        );
    }

    #[test]
    fn a_draft_holding_pastes_and_an_image_is_cleared_line_by_line() {
        // Claude Code 2.1.326 drew these; five ctrl+u presses emptied the first,
        // three the second: a paste or image token goes with its line.
        let pasted = draft_box(
            120,
            &rows(&[
                "❯ typed line one",
                "  [Pasted text #4 +9 lines]",
                "  third [Pasted text #5 +9 lines] tail",
            ]),
            32,
        );
        let image = draft_box(
            120,
            &rows(&[
                "❯ look at [Image #7] and this",
                "  [Pasted text #8 +7 lines] end",
            ]),
            32,
        );
        for (screen, presses) in [(pasted, 8), (image, 6)] {
            let draft = draft_of(&screen).expect("a draft");
            assert!(draft.pasted && draft.whole, "{draft:?}");
            let steps = clear_draft_recipe(&draft)
                .expect("a pasted draft has a recipe")
                .steps;
            assert_eq!(steps.len(), presses);
            assert!(steps.iter().all(|step| step.keys == vec![Key::ClearLine]));
        }
    }
}
