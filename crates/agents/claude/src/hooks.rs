//! Claude's hooks: the `--settings` document an interactive spawn carries and
//! the argv that names it, the edit that adds or removes the same hooks in
//! Claude Code's user settings, and the mapping of the events they report.

use crate::ClaudeOptions;
use agent_api::{
    AgentError, HookFile, HookPlan, HookReport, HookShim, UserHooksState, hook_vocabulary,
};
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::Path;

/// What one of Claude's hook payloads says about its session: the shared
/// hook vocabulary's report, plus the permission mismatch a prompt reveals.
pub(crate) fn map_hook_event(
    body: &serde_json::Value,
    spawn: Option<&ClaudeOptions>,
) -> Option<HookReport> {
    let mut report = hook_vocabulary::map(body)?;
    let details = hook_vocabulary::details(body);
    if details.event == "UserPromptSubmit" {
        report.permission_mismatch = mode_mismatch(spawn, details.permission_mode.as_deref());
    }
    Some(report)
}

/// The spawn's permission mode and the hook's report, when they differ.
///
/// Only a spawn that passed `--permission-mode` states an expected mode: a
/// spawn with no flag has nothing to compare against. The CLI's `"auto"` is a
/// mode the daemon never passes, so it never mismatches.
fn mode_mismatch(
    spawn: Option<&ClaudeOptions>,
    reported: Option<&str>,
) -> Option<(String, String)> {
    let mode = spawn?.permission_mode?;
    let reported = reported?;
    if reported == "auto" || reported == mode.as_cli_arg() {
        return None;
    }
    Some((mode.as_cli_arg().to_owned(), reported.to_owned()))
}

/// The events Claude's hooks report, each with the matcher its group names:
/// only the two matcher-scoped events name one, the rest run for every event
/// of their kind. Both the `--settings` document and the user settings entries
/// are built from this table.
const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    ("PermissionRequest", None),
    ("Elicitation", None),
    ("Stop", None),
    ("StopFailure", None),
    ("SessionEnd", None),
    ("MessageDisplay", None),
    ("PreToolUse", Some("AskUserQuestion|ExitPlanMode")),
    (
        "Notification",
        Some("permission_prompt|elicitation_dialog|elicitation_url_dialog|idle_prompt"),
    ),
];

/// The name the plan's settings file lands under inside the session's own
/// directory.
const HOOK_SETTINGS_FILE: &str = "hook-settings.json";

/// Claude's hook plan: one settings file holding the injected hooks, passed as
/// `--settings {file:hook-settings.json}` so the daemon writes it into the
/// session's own directory and substitutes its path.
pub(crate) fn hook_plan(shim: &HookShim<'_>) -> HookPlan {
    HookPlan {
        files: vec![HookFile {
            name: HOOK_SETTINGS_FILE.to_string(),
            contents: build_hook_settings(shim.tracer_exe, shim.session_id, shim.config_dir),
        }],
        args: vec![
            "--settings".to_string(),
            format!("{{file:{HOOK_SETTINGS_FILE}}}"),
        ],
    }
}

/// The contents of the `--settings` file an interactive Claude spawn gets: a
/// JSON object holding only a `hooks` key, so every other settings key keeps
/// the user's value and the injected hooks add to the user's and the repo's
/// hooks instead of replacing them.
///
/// Every event of the design's table gets one command hook in exec form
/// (`command` plus `args`, never a shell string) running the session's cached
/// tracer's `hook` shim, marked `async` so a hook never blocks the agent's
/// turn. `tracer_exe` is the cached `rt-tracer.exe` the tracer itself runs from,
/// which stays valid for the session's lifetime, and `config_dir` is the
/// daemon's config dir, where the shim reads `daemon.json` for the current port
/// and auth token.
///
/// Every key is written in sorted order, so the document reads the same
/// whether or not `serde_json` keeps insertion order.
#[must_use]
fn build_hook_settings(tracer_exe: &Path, session_id: &str, config_dir: &Path) -> String {
    let command = tracer_exe.to_string_lossy().into_owned();
    let config_dir = config_dir.to_string_lossy().into_owned();
    let hook = serde_json::json!({
        "args": ["hook", "--session", session_id, "--config-dir", config_dir],
        "async": true,
        "command": command,
        "type": "command",
    });
    let mut entries: Vec<(&str, serde_json::Value)> = HOOK_EVENTS
        .iter()
        .map(|&(event, matcher)| {
            let mut group = serde_json::Map::new();
            group.insert("hooks".to_string(), serde_json::json!([hook.clone()]));
            if let Some(matcher) = matcher {
                group.insert("matcher".to_string(), matcher.into());
            }
            (event, serde_json::json!([group]))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let hooks: serde_json::Map<String, serde_json::Value> = entries
        .into_iter()
        .map(|(event, groups)| (event.to_string(), groups))
        .collect();
    let document = serde_json::json!({ "hooks": hooks });
    serde_json::to_string_pretty(&document).unwrap_or_else(|_| document.to_string())
}

/// The args every user settings entry passes the shim, which with the shim's
/// path mark the entry as the daemon's.
const FROM_ENV_ARGS: [&str; 2] = ["hook", "--from-env"];

/// What a hook handler in Claude Code's user settings is to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Ours, running the current shim.
    Ours,
    /// Ours by its args, running a shim at some other path.
    Stale,
    /// The user's.
    Other,
}

/// What `handler` is to the daemon, whose shim is `shim`: a handler whose
/// `args` are exactly [`FROM_ENV_ARGS`] is ours when its `command` is the
/// shim's path (compared without case on Windows), and stale when it is
/// another path to a file named like the shim. Any other handler, one with
/// no `command` included, is the user's.
fn mark(handler: &Value, shim: &str) -> Mark {
    let ours_by_args = handler
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| {
            args.len() == FROM_ENV_ARGS.len()
                && args
                    .iter()
                    .zip(FROM_ENV_ARGS)
                    .all(|(arg, ours)| arg == ours)
        });
    if !ours_by_args {
        return Mark::Other;
    }
    match handler.get("command").and_then(Value::as_str) {
        Some(command) if same_path(command, shim) => Mark::Ours,
        Some(command) if is_shim_name(command) => Mark::Stale,
        _ => Mark::Other,
    }
}

/// The file names a shim the daemon installed carries, on any platform.
const SHIM_NAMES: [&str; 2] = ["rt-hook", "rt-hook.exe"];

/// Whether `command`'s last path component, split at `/` or `\`, is one of
/// [`SHIM_NAMES`] (without case on Windows).
fn is_shim_name(command: &str) -> bool {
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    SHIM_NAMES
        .iter()
        .any(|shim_name| same_path(name, shim_name))
}

/// Whether two shim paths name the same file: without case on Windows,
/// exactly elsewhere.
fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// How one event's groups hold an entry of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Exactly one current entry, in a group with the table's matcher.
    Current,
    /// Exactly one stale entry, in a group with the table's matcher.
    Repoint,
    /// Any other shape holding an entry of ours: a duplicate, a wrong
    /// matcher, or a stale entry beside a current one.
    Rebuild,
    /// No entry of ours.
    None,
}

/// Whether `group` carries the matcher the table names for its event: an
/// absent matcher fits the table's `None`, and one that is not a string fits
/// nothing.
fn matcher_fits(group: &Value, matcher: Option<&str>) -> bool {
    match group.get("matcher") {
        None => matcher.is_none(),
        Some(value) => value.as_str().is_some_and(|found| Some(found) == matcher),
    }
}

/// How `groups`, one event's groups, hold an entry of ours, whose group needs
/// the table's `matcher` to count.
fn fit(groups: &[Value], shim: &str, matcher: Option<&str>) -> Fit {
    let mut held = None;
    let mut found = 0_usize;
    for group in groups {
        let fits = matcher_fits(group, matcher);
        let hooks = group.get("hooks").and_then(Value::as_array);
        for handler in hooks.into_iter().flatten() {
            let marked = mark(handler, shim);
            if marked != Mark::Other {
                found += 1;
                held = Some((marked, fits));
            }
        }
    }
    match held {
        Some((Mark::Ours, true)) if found == 1 => Fit::Current,
        Some((Mark::Stale, true)) if found == 1 => Fit::Repoint,
        Some(_) => Fit::Rebuild,
        None => Fit::None,
    }
}

/// Whether any group in `groups` holds a handler of ours, current or stale.
fn holds_ours(groups: &[Value], shim: &str) -> bool {
    groups.iter().any(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|handler| mark(handler, shim) != Mark::Other)
            })
    })
}

/// What taking every handler of ours out of one event's groups left.
struct Pruned {
    /// A handler of ours was taken out.
    changed: bool,
    /// A group went, leaving the event's array empty.
    emptied: bool,
}

/// Take every handler of ours out of `groups`, current or stale, dropping a
/// group the removal emptied; the user's handlers stay.
fn prune_ours(groups: &mut Vec<Value>, shim: &str) -> Pruned {
    let mut changed = false;
    let mut emptied_groups = Vec::with_capacity(groups.len());
    for group in groups.iter_mut() {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            emptied_groups.push(false);
            continue;
        };
        let before = hooks.len();
        hooks.retain(|handler| mark(handler, shim) == Mark::Other);
        let removed = hooks.len() != before;
        changed |= removed;
        emptied_groups.push(removed && hooks.is_empty());
    }
    let mut emptied = emptied_groups.into_iter();
    let before = groups.len();
    groups.retain(|_| !emptied.next().unwrap_or(false));
    Pruned {
        changed,
        emptied: groups.len() != before && groups.is_empty(),
    }
}

/// Set every stale handler in `groups` to run `shim` where it stands.
fn repoint(groups: &mut [Value], shim: &str) {
    for group in groups.iter_mut() {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for handler in hooks {
            if mark(handler, shim) == Mark::Stale
                && let Value::Object(handler) = handler
            {
                handler.insert("command".to_owned(), Value::from(shim));
            }
        }
    }
}

/// Take every handler of ours out of `groups` and append one fresh group
/// carrying `matcher` and our entry for `shim`.
fn rebuild(groups: &mut Vec<Value>, matcher: Option<&str>, shim: &str) {
    prune_ours(groups, shim);
    let mut group = Map::new();
    if let Some(matcher) = matcher {
        group.insert("matcher".to_owned(), Value::from(matcher));
    }
    group.insert("hooks".to_owned(), Value::Array(vec![user_entry(shim)]));
    groups.push(Value::Object(group));
}

/// The entry the user settings carry for `shim` in every event.
fn user_entry(shim: &str) -> Value {
    serde_json::json!({
        "type": "command",
        "command": shim,
        "args": FROM_ENV_ARGS,
        "async": true,
    })
}

/// How a settings file is laid out, so an edit writes it back the same way.
struct Layout {
    /// One level of indentation.
    indent: String,
    /// Lines end in CRLF.
    crlf: bool,
    /// The text starts with a UTF-8 byte order mark, written back on render.
    bom: bool,
    /// The text ends with a line ending.
    trailing_newline: bool,
}

impl Layout {
    /// A new file's layout: two spaces, LF, no byte order mark, a trailing
    /// newline.
    fn new_file() -> Self {
        Self {
            indent: "  ".to_owned(),
            crlf: false,
            bom: false,
            trailing_newline: true,
        }
    }

    /// `text`'s layout: the indent of its first indented line (two spaces
    /// when none is), CRLF when it holds one, whether it starts with a byte
    /// order mark, and whether it ends with a newline.
    fn of(text: &str) -> Self {
        let body = strip_bom(text);
        let indent = body
            .lines()
            .find_map(|line| {
                let rest = line.trim_start_matches([' ', '\t']);
                let indent = line.strip_suffix(rest).unwrap_or_default();
                (!indent.is_empty() && !rest.is_empty()).then(|| indent.to_owned())
            })
            .unwrap_or_else(|| "  ".to_owned());
        Self {
            indent,
            crlf: body.contains("\r\n"),
            bom: text.starts_with(BOM),
            trailing_newline: body.ends_with('\n'),
        }
    }

    /// `document` pretty-printed in this layout.
    fn render(&self, document: &Value) -> Result<String, AgentError> {
        let mut out = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(self.indent.as_bytes());
        let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
        document.serialize(&mut serializer).map_err(|err| {
            AgentError::new(format!(
                "Claude Code's settings could not be written: {err}"
            ))
        })?;
        let mut text = String::from_utf8(out).map_err(|err| {
            AgentError::new(format!(
                "Claude Code's settings could not be written: {err}"
            ))
        })?;
        if self.bom {
            text.insert(0, BOM);
        }
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        if self.trailing_newline {
            text.push_str(if self.crlf { "\r\n" } else { "\n" });
        }
        Ok(text)
    }
}

/// The UTF-8 byte order mark some editors write at the start of a settings
/// file, which JSON does not carry.
const BOM: char = '\u{feff}';

/// `text` without a leading byte order mark.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix(BOM).unwrap_or(text)
}

/// The settings text's top-level object, refused when the text is not JSON,
/// does not hold an object, or holds a `hooks` section that is not an object
/// of arrays. A leading byte order mark is read past.
fn parse_settings(text: &str) -> Result<Map<String, Value>, AgentError> {
    let value: Value = serde_json::from_str(strip_bom(text)).map_err(|err| {
        AgentError::new(format!(
            "Claude Code's settings file is not valid JSON: {err}"
        ))
    })?;
    let Value::Object(root) = value else {
        return Err(AgentError::new(
            "Claude Code's settings file does not hold a JSON object",
        ));
    };
    match root.get("hooks") {
        None => Ok(root),
        Some(Value::Object(events)) if events.values().all(Value::is_array) => Ok(root),
        Some(_) => Err(AgentError::new(
            "Claude Code's settings file: the hooks section is not in the shape Claude Code writes",
        )),
    }
}

/// Bring every event of [`HOOK_EVENTS`] in `events` to the table, and take
/// ours out of every event the table no longer names; whether anything moved.
fn fit_table(events: &mut Map<String, Value>, shim: &str) -> bool {
    let mut changed = false;
    for &(event, matcher) in HOOK_EVENTS {
        let target = events
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = target else {
            continue;
        };
        match fit(groups, shim, matcher) {
            Fit::Current => {}
            Fit::Repoint => {
                repoint(groups, shim);
                changed = true;
            }
            Fit::None | Fit::Rebuild => {
                rebuild(groups, matcher, shim);
                changed = true;
            }
        }
    }
    let mut emptied_events = Vec::new();
    for (event, groups) in events.iter_mut() {
        if HOOK_EVENTS.iter().any(|&(name, _)| name == event) {
            continue;
        }
        let Value::Array(groups) = groups else {
            continue;
        };
        let pruned = prune_ours(groups, shim);
        changed |= pruned.changed;
        if pruned.emptied {
            emptied_events.push(event.clone());
        }
    }
    events.retain(|event, _| !emptied_events.contains(event));
    changed
}

/// `existing`'s settings text holding one entry running `shim` in every event
/// of [`HOOK_EVENTS`], each in a group carrying the table's matcher;
/// `existing` is `None` when there is no file.
///
/// An event that holds exactly one handler of ours in a group with the
/// table's matcher is left alone, a stale one having its `command` set to
/// `shim` where it stands. Every other shape — a duplicate, a wrong matcher,
/// none of ours — has every handler of ours taken out of it, dropping a group
/// that removal emptied, and one fresh group with the table's matcher
/// appended. Handlers of ours in an event the table no longer names are
/// pruned the same way. Nothing else moves, and text that needs no change
/// comes back byte for byte.
pub(crate) fn install_user_hooks(
    existing: Option<&str>,
    shim: &Path,
) -> Result<String, AgentError> {
    let (mut root, layout) = match existing {
        Some(text) => (parse_settings(text)?, Layout::of(text)),
        None => (Map::new(), Layout::new_file()),
    };
    let shim = shim.to_string_lossy();
    let mut changed = existing.is_none();
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(events) = hooks {
        changed |= fit_table(events, &shim);
    }
    match existing {
        Some(text) if !changed => Ok(text.to_owned()),
        _ => layout.render(&Value::Object(root)),
    }
}

/// `existing`'s settings text with every handler of ours gone, current or
/// stale; a group, event or `hooks` section emptied by that goes too. Text
/// with none of ours comes back byte for byte.
pub(crate) fn remove_user_hooks(existing: &str, shim: &Path) -> Result<String, AgentError> {
    let mut root = parse_settings(existing)?;
    let shim = shim.to_string_lossy();
    let mut changed = false;
    let mut hooks_emptied = false;
    if let Some(Value::Object(events)) = root.get_mut("hooks") {
        let mut emptied_events = Vec::new();
        for (event, groups) in events.iter_mut() {
            let Value::Array(groups) = groups else {
                continue;
            };
            let pruned = prune_ours(groups, &shim);
            changed |= pruned.changed;
            if pruned.emptied {
                emptied_events.push(event.clone());
            }
        }
        let before = events.len();
        events.retain(|event, _| !emptied_events.contains(event));
        hooks_emptied = events.len() != before && events.is_empty();
    }
    if !changed {
        return Ok(existing.to_owned());
    }
    if hooks_emptied {
        root.retain(|key, _| key != "hooks");
    }
    Layout::of(existing).render(&Value::Object(root))
}

/// The state `fits`, one read per table event, and an install outside the
/// table describe.
fn state_of(fits: &[Fit], outside: bool) -> UserHooksState {
    if outside {
        UserHooksState::Stale
    } else if fits.iter().all(|fit| *fit == Fit::None) {
        UserHooksState::Off
    } else if fits.iter().all(|fit| *fit == Fit::Current) {
        UserHooksState::On
    } else if fits.iter().all(|fit| *fit != Fit::None) {
        UserHooksState::Stale
    } else {
        UserHooksState::Partial
    }
}

/// Whether `events` holds a handler of ours in an event the table no longer
/// names.
fn outside_table(events: &Map<String, Value>, shim: &str) -> bool {
    events.iter().any(|(event, groups)| {
        !HOOK_EVENTS.iter().any(|&(name, _)| name == event)
            && groups
                .as_array()
                .is_some_and(|groups| holds_ours(groups, shim))
    })
}

/// How far `existing`'s settings text holds our entries running `shim`;
/// `existing` is `None` when there is no file.
///
/// An event reads current only when it holds exactly one current handler of
/// ours in a group with the table's matcher; a stale handler, a duplicate or
/// a wrong matcher reads stale. `On` needs every event of the table current,
/// `Stale` needs every event holding ours, and a handler of ours in an event
/// outside the table makes the whole state `Stale`; `Off` is none of ours
/// anywhere, and `Partial` anything else.
pub(crate) fn user_hooks_state(
    existing: Option<&str>,
    shim: &Path,
) -> Result<UserHooksState, AgentError> {
    let Some(text) = existing else {
        return Ok(UserHooksState::Off);
    };
    let root = parse_settings(text)?;
    let shim = shim.to_string_lossy();
    let events = root.get("hooks").and_then(Value::as_object);
    let fits: Vec<Fit> = HOOK_EVENTS
        .iter()
        .map(|&(event, matcher)| {
            events
                .and_then(|events| events.get(event))
                .and_then(Value::as_array)
                .map_or(Fit::None, |groups| fit(groups, &shim, matcher))
        })
        .collect();
    let outside = events.is_some_and(|events| outside_table(events, &shim));
    Ok(state_of(&fits, outside))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::options::PermissionMode;
    use agent_api::UserHooksState;
    use serde_json::{Value, json};

    /// A spawn that passed `mode` as `--permission-mode`, or none.
    fn spawn_with(mode: Option<PermissionMode>) -> ClaudeOptions {
        ClaudeOptions {
            permission_mode: mode,
            ..ClaudeOptions::default()
        }
    }

    /// The mismatch `spawn` and a reported mode make.
    fn permission_mismatch(
        spawn: &ClaudeOptions,
        reported: Option<&str>,
    ) -> Option<(String, String)> {
        mode_mismatch(Some(spawn), reported)
    }

    /// The hook payload `body`, as the route hands it to the plugin.
    fn details(body: &Value) -> Value {
        body.clone()
    }

    /// The permission mismatch the report for `body` carries, for a session
    /// spawned as `spawn`.
    fn apply_details(spawn: &mut ClaudeOptions, body: &Value) -> Option<(String, String)> {
        map_hook_event(body, Some(spawn)).and_then(|report| report.permission_mismatch)
    }

    /// The prompt route answers the pair `apply_signal` warns about, from the
    /// payload and the spawn's flag.
    #[test]
    fn user_prompt_submit_answers_the_permission_mismatch() {
        let mut rec = spawn_with(Some(PermissionMode::Plan));
        let differing = json!({
            "hook_event_name": "UserPromptSubmit",
            "permission_mode": "default",
        });
        assert_eq!(
            apply_details(&mut rec, &details(&differing)),
            Some(("plan".to_owned(), "default".to_owned()))
        );
        let matching = json!({
            "hook_event_name": "UserPromptSubmit",
            "permission_mode": "plan",
        });
        assert_eq!(apply_details(&mut rec, &details(&matching)), None);
    }

    #[test]
    fn permission_mismatch_reports_a_different_mode() {
        let spawn = spawn_with(Some(PermissionMode::Plan));
        assert_eq!(
            permission_mismatch(&spawn, Some("default")),
            Some(("plan".to_owned(), "default".to_owned()))
        );
        assert_eq!(permission_mismatch(&spawn, Some("plan")), None);
    }

    #[test]
    fn permission_mismatch_ignores_auto_and_missing_reports() {
        let spawn = spawn_with(Some(PermissionMode::Plan));
        assert_eq!(permission_mismatch(&spawn, Some("auto")), None);
        assert_eq!(permission_mismatch(&spawn, None), None);
    }

    #[test]
    fn permission_mismatch_needs_a_flagged_spawn() {
        assert_eq!(
            permission_mismatch(&spawn_with(None), Some("plan")),
            None,
            "no --permission-mode means nothing to compare against"
        );
        assert_eq!(
            permission_mismatch(
                &spawn_with(Some(PermissionMode::BypassPermissions)),
                Some("bypassPermissions")
            ),
            None,
            "a bypass spawn reporting bypassPermissions matches"
        );
    }

    const SESSION_ID: &str = "0b6f4c7e-1111-4222-8333-944455556666";
    const TRACER_EXE: &str = "C:/rt/binaries/rt-tracer-0123456789abcdef.exe";
    const CONFIG_DIR: &str = "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config";

    fn shim() -> HookShim<'static> {
        HookShim {
            tracer_exe: Path::new(TRACER_EXE),
            session_id: SESSION_ID,
            config_dir: Path::new(CONFIG_DIR),
        }
    }

    /// The parsed file the plan's one file holds for [`SESSION_ID`].
    fn hook_document() -> serde_json::Value {
        let json = hook_plan(&shim()).files[0].contents.clone();
        serde_json::from_str(&json).expect("the hook settings file is JSON")
    }

    /// One event's array of matcher groups in `document`.
    fn groups<'a>(document: &'a serde_json::Value, event: &str) -> &'a Vec<serde_json::Value> {
        document["hooks"][event]
            .as_array()
            .expect("an event's hooks are an array")
    }

    /// The `--settings` document [`build_hook_settings`] wrote for [`TRACER_EXE`],
    /// [`SESSION_ID`] and [`CONFIG_DIR`] before its events moved into one table.
    const HOOK_SETTINGS_BEFORE_THE_TABLE: &str = r#"{
  "hooks": {
    "Elicitation": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "MessageDisplay": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ],
        "matcher": "permission_prompt|elicitation_dialog|elicitation_url_dialog|idle_prompt"
      }
    ],
    "PermissionRequest": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "PostToolUseFailure": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ],
        "matcher": "AskUserQuestion|ExitPlanMode"
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "StopFailure": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "args": [
              "hook",
              "--session",
              "0b6f4c7e-1111-4222-8333-944455556666",
              "--config-dir",
              "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config"
            ],
            "async": true,
            "command": "C:/rt/binaries/rt-tracer-0123456789abcdef.exe",
            "type": "command"
          }
        ]
      }
    ]
  }
}"#;

    #[test]
    fn hook_settings_document_is_unchanged_by_the_table() {
        assert_eq!(
            build_hook_settings(Path::new(TRACER_EXE), SESSION_ID, Path::new(CONFIG_DIR)),
            HOOK_SETTINGS_BEFORE_THE_TABLE
        );
    }

    #[test]
    fn hook_plan_writes_one_settings_file_and_passes_it_by_name() {
        let plan = hook_plan(&shim());
        assert_eq!(plan.files.len(), 1, "one settings file");
        assert_eq!(plan.files[0].name, "hook-settings.json");
        assert_eq!(
            plan.args,
            vec![
                "--settings".to_string(),
                "{file:hook-settings.json}".to_string(),
            ],
            "the argv names the file by its placeholder"
        );
        assert_eq!(
            plan.files[0].contents,
            build_hook_settings(Path::new(TRACER_EXE), SESSION_ID, Path::new(CONFIG_DIR)),
            "the file holds the injected hooks document"
        );
    }

    #[test]
    fn hook_settings_list_every_event_and_matcher() {
        let document = hook_document();
        let hooks = document["hooks"].as_object().expect("a hooks object");
        let mut events: Vec<&str> = hooks.keys().map(String::as_str).collect();
        events.sort_unstable();
        assert_eq!(
            events,
            [
                "Elicitation",
                "MessageDisplay",
                "Notification",
                "PermissionRequest",
                "PostToolUse",
                "PostToolUseFailure",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "StopFailure",
                "UserPromptSubmit",
            ]
        );
        assert_eq!(
            groups(&document, "PreToolUse")[0]["matcher"].as_str(),
            Some("AskUserQuestion|ExitPlanMode")
        );
        assert_eq!(
            groups(&document, "Notification")[0]["matcher"].as_str(),
            Some("permission_prompt|elicitation_dialog|elicitation_url_dialog|idle_prompt")
        );
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "Elicitation",
            "Stop",
            "StopFailure",
            "SessionEnd",
            "MessageDisplay",
        ] {
            assert!(
                groups(&document, event)[0].get("matcher").is_none(),
                "{event} runs for every input and names no matcher"
            );
        }
    }

    #[test]
    fn hook_settings_run_the_cached_tracer_in_exec_form() {
        let document = hook_document();
        let hooks = document["hooks"].as_object().expect("a hooks object");
        for event in hooks.keys() {
            let event_groups = groups(&document, event);
            assert_eq!(event_groups.len(), 1, "{event} has one matcher group");
            let entries = event_groups[0]["hooks"]
                .as_array()
                .expect("a group's hooks are an array");
            assert_eq!(entries.len(), 1, "{event} has one hook");
            let hook = &entries[0];
            assert_eq!(hook["type"], "command", "{event}");
            assert_eq!(hook["command"].as_str(), Some(TRACER_EXE), "{event}");
            assert_eq!(
                hook["args"],
                serde_json::json!(["hook", "--session", SESSION_ID, "--config-dir", CONFIG_DIR,]),
                "{event}"
            );
            // Exec form: the command is the exe alone, never a shell string
            // with the arguments folded into it.
            assert!(
                !hook["command"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("--session"),
                "{event} must not build a shell command string"
            );
            assert!(hook.get("shell").is_none(), "{event} must not name a shell");
        }
    }

    #[test]
    fn hook_settings_are_async() {
        let document = hook_document();
        let hooks = document["hooks"].as_object().expect("a hooks object");
        for event in hooks.keys() {
            assert_eq!(
                groups(&document, event)[0]["hooks"][0]["async"],
                serde_json::json!(true),
                "{event} must run in the background, not block the turn"
            );
        }
    }

    /// The `MessageDisplay` body Claude Code 2.1.314 sent for a three-line
    /// answer, its transcript path's user folder redacted.
    const MESSAGE_DISPLAY: &str = include_str!("../tests/fixtures/hooks/message_display.json");

    fn message_display_body() -> Value {
        serde_json::from_str(MESSAGE_DISPLAY).expect("the fixture is JSON")
    }

    #[test]
    fn captured_message_display_maps_to_a_live_line() {
        let line = agent_api::Agent::map_live_line(&crate::ClaudePlugin, &message_display_body())
            .expect("the captured body carries a live line");
        assert_eq!(
            line,
            agent_api::LiveLine {
                text: "Red — the colour of fire and ripe strawberries\n\
                       Yellow — bright like a summer sun\n\
                       Blue — calm as a clear sky"
                    .to_owned(),
                index: 0,
                message_key: Some("ea5975fb-ea6c-4b0c-a588-87e760f6e3cc".to_owned()),
                prompt_key: Some("7e18acba-1e68-409c-9232-4b0d7764938a".to_owned()),
            }
        );
    }

    #[test]
    fn captured_message_display_reports_no_status() {
        assert_eq!(map_hook_event(&message_display_body(), None), None);
    }

    #[test]
    fn hook_settings_name_only_a_hooks_object() {
        let document = hook_document();
        let object = document.as_object().expect("a JSON object");
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            ["hooks"],
            "every other settings key must keep the user's value"
        );
    }

    /// The fixed shim path a user settings entry runs.
    const SHIM: &str = "C:/rt/data/hook-shim/rt-hook.exe";
    /// A shim path an older install wrote.
    const OLD_SHIM: &str = "C:/old/data/hook-shim/rt-hook.exe";

    /// A user settings file with the user's own `Stop` and `PreToolUse`
    /// groups and top-level keys in no sorted order.
    const USER_FILE: &str = r#"{
  "theme": "dark",
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "notify-send done"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "audit.sh"
          }
        ]
      }
    ]
  },
  "model": "opus",
  "env": {
    "Z": "1",
    "A": "2"
  }
}
"#;

    /// The entry a user settings install writes for `shim`.
    fn user_entry(shim: &str) -> Value {
        json!({
            "type": "command",
            "command": shim,
            "args": ["hook", "--from-env"],
            "async": true,
        })
    }

    fn install(existing: Option<&str>, shim: &str) -> String {
        install_user_hooks(existing, Path::new(shim)).expect("the install succeeds")
    }

    fn remove(existing: &str, shim: &str) -> String {
        remove_user_hooks(existing, Path::new(shim)).expect("the removal succeeds")
    }

    fn state(existing: Option<&str>, shim: &str) -> UserHooksState {
        user_hooks_state(existing, Path::new(shim)).expect("the state reads")
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("the settings text is JSON")
    }

    /// `value` as a settings file: two-space indent, trailing newline.
    fn pretty(value: &Value) -> String {
        serde_json::to_string_pretty(value).expect("serializes") + "\n"
    }

    fn keys(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn install_into_a_missing_file_creates_every_entry() {
        let text = install(None, SHIM);
        assert!(text.starts_with("{\n  \"hooks\": {\n    \""), "{text}");
        assert!(text.ends_with("}\n") && !text.contains('\r'), "{text}");
        let document = parse(&text);
        assert_eq!(keys(&document), ["hooks"]);
        assert_eq!(
            keys(&document["hooks"]),
            HOOK_EVENTS.iter().map(|e| e.0).collect::<Vec<_>>()
        );
        for &(event, matcher) in HOOK_EVENTS {
            let event_groups = groups(&document, event);
            assert_eq!(event_groups.len(), 1, "{event}");
            assert_eq!(event_groups[0]["matcher"].as_str(), matcher, "{event}");
            assert_eq!(
                event_groups[0]["hooks"],
                json!([user_entry(SHIM)]),
                "{event}"
            );
        }
    }

    #[test]
    fn install_keeps_the_users_hooks_and_key_order() {
        let original = parse(USER_FILE);
        let document = parse(&install(Some(USER_FILE), SHIM));
        assert_eq!(keys(&document), ["theme", "hooks", "model", "env"]);
        assert_eq!(keys(&document["env"]), ["Z", "A"]);
        let events = keys(&document["hooks"]);
        assert_eq!(
            events[..2],
            ["Stop", "PreToolUse"],
            "the user's events stay first"
        );
        assert_eq!(events.len(), HOOK_EVENTS.len());
        let stop = groups(&document, "Stop");
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0], original["hooks"]["Stop"][0],
            "the user's group stays first"
        );
        assert_eq!(stop[1], json!({ "hooks": [user_entry(SHIM)] }));
        let pre = groups(&document, "PreToolUse");
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0], original["hooks"]["PreToolUse"][0]);
        assert_eq!(
            pre[1],
            json!({ "matcher": "AskUserQuestion|ExitPlanMode", "hooks": [user_entry(SHIM)] })
        );
        assert_eq!(keys(&pre[1]), ["matcher", "hooks"]);
    }

    #[test]
    fn install_twice_changes_nothing() {
        let once = install(Some(USER_FILE), SHIM);
        assert_eq!(install(Some(&once), SHIM), once);
        let fresh = install(None, SHIM);
        assert_eq!(install(Some(&fresh), SHIM), fresh);
    }

    #[test]
    fn install_replaces_a_stale_command() {
        let stale = install(Some(USER_FILE), OLD_SHIM);
        assert_eq!(
            install(Some(&stale), SHIM),
            install(Some(USER_FILE), SHIM),
            "the stale handler takes the new path where it stands"
        );
    }

    #[test]
    fn remove_takes_out_only_ours_and_prunes_empties() {
        let installed = install(Some(USER_FILE), SHIM);
        assert_eq!(
            remove(&installed, SHIM),
            USER_FILE,
            "the user's file comes back"
        );
        assert_eq!(
            remove(&install(None, SHIM), SHIM),
            "{}\n",
            "an emptied hooks key goes"
        );

        let user_copy =
            json!({ "type": "command", "command": SHIM, "args": ["hook", "--session", "x"] });
        let no_args = json!({ "type": "command", "command": SHIM });
        let other_args = json!({ "type": "command", "command": SHIM, "args": ["other"] });
        let file = pretty(&json!({
            "hooks": {
                "Stop": [{
                    "hooks": [user_copy, user_entry(OLD_SHIM), no_args, user_entry(SHIM), other_args]
                }],
                "SessionEnd": [{ "hooks": [] }],
                "Notification": [
                    { "matcher": "idle_prompt", "hooks": [user_entry(SHIM)] },
                    { "hooks": [user_entry(OLD_SHIM)] },
                ],
            },
            "model": "opus",
        }));
        let document = parse(&remove(&file, SHIM));
        assert_eq!(
            document,
            json!({
                "hooks": {
                    "Stop": [{ "hooks": [user_copy, no_args, other_args] }],
                    "SessionEnd": [{ "hooks": [] }],
                },
                "model": "opus",
            }),
            "only ours and ours-but-stale go; a group the user left empty stays"
        );
    }

    #[test]
    fn remove_with_none_of_ours_returns_the_text_unchanged() {
        let text = "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"x\"}]}]},  \"a\" : 1}";
        assert_eq!(remove(text, SHIM), text);
        assert_eq!(remove(USER_FILE, SHIM), USER_FILE);
    }

    #[test]
    fn install_keeps_crlf_and_four_space_indent() {
        let original = "{\r\n    \"theme\": \"dark\"\r\n}\r\n";
        let text = install(Some(original), SHIM);
        assert!(
            text.starts_with(
                "{\r\n    \"theme\": \"dark\",\r\n    \"hooks\": {\r\n        \"SessionStart\": ["
            ),
            "{text:?}"
        );
        assert!(text.ends_with("}\r\n"), "{text:?}");
        assert_eq!(
            text.matches('\n').count(),
            text.matches("\r\n").count(),
            "every line ends in CRLF"
        );
        assert_eq!(remove(&text, SHIM), original);
        let bare = install(Some("{\n\t\"theme\": \"dark\"\n}"), SHIM);
        assert!(bare.starts_with("{\n\t\"theme\""), "{bare:?}");
        assert!(
            bare.ends_with('}'),
            "no trailing newline when the file had none"
        );
    }

    #[test]
    fn a_file_that_is_not_json_is_refused() {
        for text in ["{ // a comment\n}", "{\"a\": 1,}", "", "[]", "\"x\""] {
            let errors = [
                install_user_hooks(Some(text), Path::new(SHIM)).err(),
                remove_user_hooks(text, Path::new(SHIM)).err(),
                user_hooks_state(Some(text), Path::new(SHIM)).err(),
            ];
            for error in errors {
                let message = error.expect("refused").to_string();
                assert!(message.contains("JSON"), "{text:?}: {message}");
            }
        }
    }

    #[test]
    fn a_hooks_section_of_the_wrong_shape_is_refused() {
        for text in ["{\"hooks\": []}", "{\"hooks\": {\"Stop\": {}}}"] {
            let errors = [
                install_user_hooks(Some(text), Path::new(SHIM)).err(),
                remove_user_hooks(text, Path::new(SHIM)).err(),
                user_hooks_state(Some(text), Path::new(SHIM)).err(),
            ];
            for error in errors {
                let message = error.expect("refused").to_string();
                assert!(
                    message.contains("the hooks section is not in the shape Claude Code writes"),
                    "{text:?}: {message}"
                );
            }
        }
    }

    #[test]
    fn state_reads_off_on_partial_and_stale() {
        let current = parse(&install(None, SHIM));
        let mut one_stale = current.clone();
        one_stale["hooks"]["Stop"][0]["hooks"][0]["command"] = json!(OLD_SHIM);
        let mut one_missing = current.clone();
        one_missing["hooks"]
            .as_object_mut()
            .expect("hooks")
            .remove("Stop");
        let mut stale_one_missing = parse(&install(None, OLD_SHIM));
        stale_one_missing["hooks"]
            .as_object_mut()
            .expect("hooks")
            .remove("Notification");
        let rows: [(&str, Option<String>, UserHooksState); 9] = [
            ("no file", None, UserHooksState::Off),
            ("empty object", Some("{}".to_owned()), UserHooksState::Off),
            (
                "only the user's hooks",
                Some(USER_FILE.to_owned()),
                UserHooksState::Off,
            ),
            (
                "every event current",
                Some(install(Some(USER_FILE), SHIM)),
                UserHooksState::On,
            ),
            (
                "every event stale",
                Some(install(None, OLD_SHIM)),
                UserHooksState::Stale,
            ),
            (
                "one event stale",
                Some(pretty(&one_stale)),
                UserHooksState::Stale,
            ),
            (
                "one event missing",
                Some(pretty(&one_missing)),
                UserHooksState::Partial,
            ),
            (
                "stale with one event missing",
                Some(pretty(&stale_one_missing)),
                UserHooksState::Partial,
            ),
            (
                "only the user's copy of an injected entry",
                Some(pretty(&json!({ "hooks": { "Stop": [{ "hooks": [
                    { "type": "command", "command": SHIM, "args": ["hook", "--session", "x"] }
                ] }] } }))),
                UserHooksState::Off,
            ),
        ];
        for (name, text, expected) in rows {
            assert_eq!(state(text.as_deref(), SHIM), expected, "{name}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_compares_the_shim_path_without_case() {
        let upper = "C:/RT/Data/Hook-Shim/RT-HOOK.EXE";
        let installed = install(None, upper);
        assert_eq!(state(Some(&installed), SHIM), UserHooksState::On);
        assert_eq!(install(Some(&installed), SHIM), installed, "not stale");
        assert_eq!(remove(&installed, SHIM), "{}\n");
    }

    /// A settings file whose `Stop` event holds one group with `handler`.
    fn stop_file(handler: &Value) -> String {
        pretty(&json!({ "hooks": { "Stop": [{ "hooks": [handler] }] } }))
    }

    /// Install appends our own group beside `handler`, which stays as it
    /// was; remove leaves it in place; it never counts as ours.
    fn assert_left_alone(handler: &Value) {
        let file = stop_file(handler);
        assert_eq!(state(Some(&file), SHIM), UserHooksState::Off, "{handler}");
        let installed = install(Some(&file), SHIM);
        let document = parse(&installed);
        let stop = groups(&document, "Stop");
        assert_eq!(stop.len(), 2, "{handler}");
        assert_eq!(stop[0], json!({ "hooks": [handler] }), "{handler}");
        assert_eq!(stop[1], json!({ "hooks": [user_entry(SHIM)] }), "{handler}");
        assert_eq!(remove(&file, SHIM), file, "{handler}");
        assert_eq!(remove(&installed, SHIM), file, "{handler}");
    }

    #[test]
    fn extra_args_are_not_ours() {
        assert_left_alone(&json!({
            "type": "command",
            "command": SHIM,
            "args": ["hook", "--from-env", "--verbose"],
            "async": true,
        }));
    }

    #[test]
    fn another_tools_handler_with_our_args_is_left_alone() {
        assert_left_alone(&json!({
            "type": "command",
            "command": r"C:\tools\othertool.exe",
            "args": ["hook", "--from-env"],
        }));
    }

    #[test]
    fn a_handler_without_a_command_is_left_alone() {
        assert_left_alone(&json!({
            "type": "command",
            "args": ["hook", "--from-env"],
        }));
    }

    /// The matcher the table names for `event`.
    fn matcher_of(event: &str) -> &'static str {
        HOOK_EVENTS
            .iter()
            .find(|&&(name, _)| name == event)
            .and_then(|&(_, matcher)| matcher)
            .expect("the table names the event with a matcher")
    }

    #[test]
    fn install_widens_an_old_matcher_to_the_table() {
        let matcher = matcher_of("Notification");
        let user_group = json!({ "matcher": "custom", "hooks": [
            { "type": "command", "command": "notify.sh" } ] });
        let file = pretty(&json!({ "hooks": { "Notification": [
            { "matcher": "permission_prompt", "hooks": [user_entry(SHIM)] },
            user_group,
        ] } }));
        let installed = install(Some(&file), SHIM);
        let document = parse(&installed);
        let notification = groups(&document, "Notification");
        assert_eq!(notification.len(), 2, "{notification:?}");
        assert_eq!(notification[0], user_group, "the user's group stays");
        assert_eq!(
            notification[1],
            json!({ "matcher": matcher, "hooks": [user_entry(SHIM)] }),
            "one group of ours, with the table's matcher"
        );
        assert_eq!(state(Some(&installed), SHIM), UserHooksState::On);
    }

    #[test]
    fn state_reads_stale_for_an_old_matcher() {
        let mut document = parse(&install(None, SHIM));
        document["hooks"]["Notification"][0]["matcher"] = json!("permission_prompt");
        assert_eq!(
            state(Some(&pretty(&document)), SHIM),
            UserHooksState::Stale,
            "an old matcher is not current"
        );
    }

    #[test]
    fn install_drops_a_stale_copy_beside_a_current_one() {
        let matcher = matcher_of("Notification");
        let file = pretty(&json!({ "hooks": { "Notification": [
            { "matcher": matcher, "hooks": [user_entry(SHIM)] },
            { "matcher": matcher, "hooks": [user_entry(OLD_SHIM)] },
        ] } }));
        let installed = install(Some(&file), SHIM);
        let document = parse(&installed);
        assert_eq!(
            groups(&document, "Notification"),
            &vec![json!({ "matcher": matcher, "hooks": [user_entry(SHIM)] })],
            "one group of ours stays"
        );
        assert_eq!(state(Some(&installed), SHIM), UserHooksState::On);
    }

    #[test]
    fn state_reads_stale_for_a_duplicate() {
        let mut document = parse(&install(None, SHIM));
        let copy = document["hooks"]["Notification"][0].clone();
        document["hooks"]["Notification"]
            .as_array_mut()
            .expect("groups")
            .push(copy);
        assert_eq!(
            state(Some(&pretty(&document)), SHIM),
            UserHooksState::Stale,
            "two copies of ours is not current"
        );
    }

    #[test]
    fn install_prunes_ours_from_an_event_the_table_dropped() {
        let user_handler = json!({ "type": "command", "command": "keep.sh" });
        let file = pretty(&json!({ "hooks": {
            "OldEvent": [
                { "hooks": [user_entry(OLD_SHIM), user_handler] },
                { "hooks": [user_entry(SHIM)] },
            ],
            "GoneEvent": [{ "hooks": [user_entry(SHIM)] }],
            "Stop": [{ "hooks": [user_handler] }],
        } }));
        let document = parse(&install(Some(&file), SHIM));
        assert_eq!(
            document["hooks"]["OldEvent"],
            json!([{ "hooks": [user_handler] }]),
            "ours goes with the group it emptied, the user's handler stays"
        );
        assert!(
            document["hooks"].get("GoneEvent").is_none(),
            "an event holding only ours goes: {document}"
        );
        assert_eq!(
            document["hooks"]["Stop"],
            json!([{ "hooks": [user_handler] }, { "hooks": [user_entry(SHIM)] }]),
            "the table's own events are still installed"
        );
    }

    #[test]
    fn state_reads_stale_with_ours_outside_the_table() {
        let mut document = parse(&install(None, SHIM));
        document["hooks"]["OldEvent"] = json!([{ "hooks": [user_entry(SHIM)] }]);
        assert_eq!(
            state(Some(&pretty(&document)), SHIM),
            UserHooksState::Stale,
            "an entry of ours in an event the table dropped is stale"
        );
    }

    #[test]
    fn a_settings_file_with_a_bom_installs_and_keeps_its_bom() {
        let original = format!("\u{feff}{USER_FILE}");
        let installed = install(Some(&original), SHIM);
        let body = installed
            .strip_prefix('\u{feff}')
            .expect("the byte order mark stays");
        assert_eq!(body, install(Some(USER_FILE), SHIM));
        assert_eq!(
            install(Some(&installed), SHIM),
            installed,
            "a second install changes nothing"
        );
    }

    #[test]
    fn state_reads_a_settings_file_with_a_bom() {
        let installed = install(None, SHIM);
        let with_bom = format!("\u{feff}{installed}");
        assert_eq!(state(Some(&with_bom), SHIM), UserHooksState::On);
    }

    #[test]
    fn remove_keeps_a_bom() {
        let original = format!("\u{feff}{USER_FILE}");
        let installed = install(Some(&original), SHIM);
        assert_eq!(remove(&installed, SHIM), original);
    }
}
