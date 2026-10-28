//! Claude Code's chat screen reader and its answer recipes.
//!
//! [`read`] reads the visible rows of a session's rendered screen into what
//! the chat shows: a full-screen overlay first, then a dialog (permission,
//! plan approval, folder trust), then the input box. A Heads up notice does
//! not own the keyboard, so it rides along with the input box's own state.
//! [`recipe`] turns an answer to one of those prompts into the keys that
//! give it, each group with the reading the daemon waits for after it.
//!
//! The parsers key on the text Claude Code 2.1.307 draws (the captures in
//! the daemon's `screen/fixtures/`), never on colour, which the rows do not
//! carry.

use agent_api::{AnswerRecipe, AnswerRefusal, Key, RecipeStep, ScreenExpect};
use protocol::{ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ComposerState};

/// The character a horizontal rule is drawn with.
const RULE_CHAR: char = '─';

/// The character a content box's rules are drawn with.
const BOX_CHAR: char = '╌';

/// The marker on the row the cursor is on, and the input box's prompt.
const CURSOR: char = '❯';

/// The largest body a prompt carries, in bytes.
const BODY_CAP: usize = 4096;

/// The glyphs a turn's spinner cycles through.
const SPINNER_GLYPHS: [char; 7] = ['·', '✢', '*', '✶', '✻', '✽', '✳'];

/// How many non-empty rows above the input box are searched for a spinner.
const SPINNER_ROWS: usize = 24;

/// How many non-empty rows at the bottom are searched for an overlay's keys.
const OVERLAY_ROWS: usize = 4;

/// The key line each overlay shows, and the name the chat gives it.
const OVERLAYS: [(&str, &str); 4] = [
    ("Showing detailed transcript", "transcript"),
    ("Enter to set as default", "model"),
    ("f to fork · Esc to close", "btw"),
    ("Enter to continue · Esc to cancel", "rewind"),
];

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
        .or_else(|| parse_trust(rows, start));
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
    let block_end = (start..footer)
        .rev()
        .find(|&index| !rows[index].trim().is_empty())
        .map_or(footer, |index| index + 1);
    let block_start = (start..block_end)
        .rev()
        .find(|&index| rows[index].trim().is_empty())
        .map_or(start, |index| index + 1);
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

/// The options of a trust dialog: a printed number when the build numbers
/// them, else the row's 1-based position.
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

/// A Heads up notice over the input box: the `✦ Heads up · …` row and its
/// wrapped rows, the expanded title and bullets when shown, and the key row
/// of `N: label` pairs.
fn parse_heads_up(rows: &[String]) -> Option<ChatPrompt> {
    let anchor = rows.iter().rposition(|row| row.contains("✦ Heads up ·"))?;
    let (key_row, options) = (anchor + 1..rows.len())
        .find_map(|index| key_pairs(&rows[index]).map(|options| (index, options)))?;
    let first = rows[anchor].trim();
    let mut subject = first.strip_prefix('✦').unwrap_or(first).trim().to_owned();
    let mut index = anchor + 1;
    while index < key_row && !rows[index].trim().is_empty() {
        subject.push(' ');
        subject.push_str(rows[index].trim());
        index += 1;
    }
    let details: Vec<&str> = rows[index..key_row].iter().map(String::as_str).collect();
    let indent = details
        .iter()
        .filter(|row| !row.trim().is_empty())
        .map(|row| indent_of(row))
        .min()
        .unwrap_or(0);
    Some(prompt(
        ChatPromptKind::HeadsUp,
        subject,
        text_block(&details, indent),
        options,
    ))
}

/// The options a Heads up key row offers (`1: Learn more   0: Dismiss`), or
/// `None` for a row that is not one: at least two pairs, three spaces apart.
fn key_pairs(row: &str) -> Option<Vec<ChatOption>> {
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
    options.filter(|options| options.len() >= 2)
}

/// The input box's state: the rows between the last two rules. Text after
/// the prompt (other than the `Try "…"` placeholder), or more than one row,
/// is a draft; an empty box is working while a spinner shows above it.
fn composer(rows: &[String]) -> ComposerState {
    let rules = rule_indices(rows);
    let [.., top, bottom] = rules[..] else {
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
        !rest.is_empty() && !rest.starts_with("Try \"")
    } else if let Some(rest) = first.strip_prefix('!') {
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

/// Whether a turn's spinner shows among the last non-empty rows of `rows`.
fn spinner_above(rows: &[String]) -> bool {
    rows.iter()
        .rev()
        .filter(|row| !row.trim().is_empty())
        .take(SPINNER_ROWS)
        .any(|row| is_spinner(row))
}

/// Whether `row` is a spinner line: a spinner glyph, then a capitalised verb
/// ending in an ellipsis (`· Cooking… (21s · ↓ 1.1k tokens)`).
fn is_spinner(row: &str) -> bool {
    let mut chars = row.trim_start().chars();
    if !chars.next().is_some_and(|c| SPINNER_GLYPHS.contains(&c)) {
        return false;
    }
    let rest = chars.as_str();
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let word = rest.split_whitespace().next().unwrap_or_default();
    let Some(verb) = word.strip_suffix('…') else {
        return false;
    };
    verb.chars().next().is_some_and(char::is_uppercase)
        && verb
            .chars()
            .all(|c| c.is_alphabetic() || matches!(c, '\'' | '\u{2019}' | '-'))
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
    }
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

/// The index of the first row under the last rule, or 0 with no rule.
fn after_last_rule(rows: &[String]) -> usize {
    rule_indices(rows).last().map_or(0, |&index| index + 1)
}

/// The keys that give `answer` to `prompt`.
pub(crate) fn recipe(
    prompt: &ChatPrompt,
    answer: &ChatAnswerKind,
) -> Result<AnswerRecipe, AnswerRefusal> {
    match prompt.kind {
        ChatPromptKind::Permission
        | ChatPromptKind::Plan
        | ChatPromptKind::Trust
        | ChatPromptKind::HeadsUp => {}
        _ => return Err(AnswerRefusal::Unsupported),
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
    if text.trim().is_empty() {
        return Err(AnswerRefusal::TextEmpty);
    }
    Ok(RecipeStep {
        keys: vec![Key::Text(text.to_owned()), Key::Enter],
        expect: ScreenExpect::Any,
    })
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

/// Choosing option `number`: its digit, or on a trust dialog, where digits
/// do nothing, the arrows to it and Enter.
fn option_recipe(prompt: &ChatPrompt, number: u32) -> Result<AnswerRecipe, AnswerRefusal> {
    if prompt.kind == ChatPromptKind::Trust {
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

/// Moving the trust dialog's cursor from where the reading shows it to
/// option `number`, then Enter.
fn trust_recipe(prompt: &ChatPrompt, number: u32) -> Result<AnswerRecipe, AnswerRefusal> {
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
    let mut steps = Vec::new();
    if target != cursor {
        let key = if target > cursor { Key::Down } else { Key::Up };
        steps.push(RecipeStep {
            keys: vec![key; target.abs_diff(cursor)],
            expect: ScreenExpect::OptionSelected(number),
        });
    }
    steps.push(RecipeStep {
        keys: vec![Key::Enter],
        expect: ScreenExpect::DialogGone,
    });
    Ok(AnswerRecipe { steps })
}

/// Allowing with a message: Tab, with the cursor on option 1, turns it into
/// a field, then the text and Enter.
fn allow_with_text(prompt: &ChatPrompt, text: &str) -> Result<AnswerRecipe, AnswerRefusal> {
    if prompt.kind != ChatPromptKind::Permission || !prompt.can_amend || prompt.cursor != Some(1) {
        return Err(AnswerRefusal::NotThisDialog);
    }
    if text.trim().is_empty() {
        return Err(AnswerRefusal::TextEmpty);
    }
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{read, recipe};
    use agent_api::{AnswerRefusal, Key, RecipeStep, ScreenExpect};
    use protocol::{ChatAnswerKind, ChatPromptKind, ChatScreen, ComposerState};

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

    #[test]
    fn a_bare_bang_box_is_ready_and_a_bang_command_is_a_draft() {
        assert_eq!(read(&boxed(&[], &["!"])).composer, ComposerState::Ready);
        assert_eq!(read(&boxed(&[], &["! ls"])).composer, ComposerState::Draft);
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
}
