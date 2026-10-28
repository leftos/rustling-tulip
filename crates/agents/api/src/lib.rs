//! The agent plugin seam.
//!
//! Each agent CLI the daemon knows how to spawn is an [`Agent`]: it describes
//! itself and its option schema with an [`AgentDescriptor`], builds the CLI's
//! argv from its typed options and a [`SpawnContext`], names the CLI's
//! executable, plans the hook files and argv its interactive spawns carry, and
//! turns the CLI's headless output into [`HeadlessEvent`]s the daemon applies
//! to the session.
//!
//! The registry holds every agent as the object-safe [`AgentPlugin`], which
//! this crate implements for every [`Agent`]: each call takes the spawn's
//! [`OptionValues`] and decodes them into the agent's typed options.
//! [`validate`] checks a set of values against an agent's schema, and
//! [`workspace_prelude`] is the workspace note a plugin hands its CLI for a
//! multi-member session.

pub mod activity;
#[cfg(feature = "test-support")]
pub mod conformance;
pub mod hook_vocabulary;
pub mod screen_rules;
#[cfg(any(test, feature = "test-support"))]
pub mod test_agent;

pub use activity::{
    ActivityReader, ActivitySnapshot, ActivitySource, BackgroundTask, ContextReading,
    MINUTE_COLUMNS, OpenCall,
};
pub use screen_rules::{
    LineMatcher, ReadySignal, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict,
};

use chrono::{DateTime, Utc};
use protocol::{AgentDescriptor, FieldKind, OptionField, OptionValue, OptionValues, SessionChip};
use protocol::{PendingInput, SessionMember, SessionStatus};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::path::{Path, PathBuf};
use tracing::error;

/// The rule set of a plugin that reads no screen: every reading is no match,
/// so its sessions are read by their hooks alone.
static EMPTY_SCREEN_RULES: ScreenRules = ScreenRules { rules: Vec::new() };

/// Cross-agent spawn context bundled together so trait signatures stay
/// readable. Fields are borrowed from the daemon's in-flight spawn so the
/// caller doesn't have to clone anything to call a plugin method.
pub struct SpawnContext<'a> {
    pub dangerously_skip_permissions: bool,
    /// `true` when the spawn has a prompt injector attached, in which case
    /// the plugin must NOT emit the initial prompt as a CLI arg — the
    /// injector delivers it through the PTY post-spawn instead. Ignored by
    /// headless paths.
    pub has_prompt_injector: bool,
    /// Conversation id the plugin passes its CLI on an interactive spawn
    /// (Claude's `--session-id`). Ignored by headless paths.
    pub conversation_id: Option<&'a str>,
    /// Conversation the CLI resumes (Claude's `--resume`), in place of the
    /// conversation id and any initial prompt.
    pub resume_conversation: Option<&'a str>,
    /// Directories a standalone target carries as extra folders. The plugin
    /// adds them after the extra members' worktrees; headless spawns ignore
    /// them.
    pub add_dirs: &'a [String],
    /// The argv the plugin's hooks add on an interactive spawn, fresh or
    /// resumed: [`Agent::hook_plan`]'s args with each `{file:<name>}` replaced
    /// by the path the daemon wrote for it. Empty for a spawn whose hook
    /// files could not be written, which stays on the terminal heuristic.
    /// Headless spawns never carry it.
    pub hook_args: &'a [String],
}

/// What a plugin builds its hook command from: the cached `rt-tracer.exe`
/// whose `hook` shim the hooks run, the session they report for, and the
/// daemon's config dir, where the shim reads the daemon's port and auth token.
pub struct HookShim<'a> {
    /// The cached tracer the session's spawn runs, so the hook command and the
    /// supervisor cannot disagree about which copy runs the shim.
    pub tracer_exe: &'a Path,
    /// The session the hooks report for.
    pub session_id: &'a str,
    /// The daemon's config dir.
    pub config_dir: &'a Path,
}

/// One file of a [`HookPlan`], written into the session's own
/// `sessions/<id>/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFile {
    /// The file's name inside the session directory: a plain `.json` file
    /// name, never a path.
    pub name: String,
    /// What the file holds.
    pub contents: String,
}

/// What a plugin's hooks need: the files the daemon writes into the session's
/// own directory, and the argv it appends to the spawn with each
/// `{file:<name>}` replaced by the written file's path. An empty plan means
/// the agent gets no hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookPlan {
    /// The files to write, in order.
    pub files: Vec<HookFile>,
    /// The argv to append, each `{file:<name>}` a placeholder for one of
    /// [`Self::files`].
    pub args: Vec<String>,
}

/// How a hook event changes a session's pending input.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingUpdate {
    /// Drop what is pending.
    Clear,
    /// Replace what is pending.
    Set(PendingInput),
    /// Keep what is pending; when nothing is pending, use this.
    KeepOr(PendingInput),
}

/// What one hook event says about its session, as [`Agent::map_hook_event`]
/// reads it. The daemon orders it against the session's other events and
/// applies it to the record.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact one event reports, not states of one machine"
)]
pub struct HookReport {
    /// The status the event implies, or `None` when it says nothing about
    /// the status.
    pub status: Option<SessionStatus>,
    /// How the event changes what the session is waiting for.
    pub pending: PendingUpdate,
    /// The conversation the session moved to (a new conversation's start, or
    /// a resumed one's own id).
    pub conversation_id: Option<String>,
    /// The assistant message the turn ended with, untrimmed and uncapped.
    pub last_message: Option<String>,
    /// The recent-actions line a failed turn leaves.
    pub failure: Option<String>,
    /// The spawn's permission mode and the one the agent reported, when they
    /// differ.
    pub permission_mismatch: Option<(String, String)>,
    /// The agent ended (a `/clear` ends only its conversation).
    pub agent_ended: bool,
    /// The agent started a new run.
    pub agent_started: bool,
    /// A subagent fired the event, not the main agent.
    pub subagent: bool,
    /// Whether work runs on beside an idle turn, when the main agent's turn
    /// end says so.
    pub background: Option<bool>,
    /// An idle the agent reports while it may still be waiting on a prompt,
    /// which leaves a pending prompt alone.
    pub weak_idle: bool,
}

/// One change a plugin read from a line of its CLI's headless output, for the
/// daemon to apply to the session's record in order.
#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessEvent {
    /// The session's new status.
    Status(SessionStatus),
    /// A line for the session's recent-actions list.
    RecentAction(String),
    /// Usage figures; each `Some` replaces the session's current figure and
    /// each `None` leaves it as it is.
    Metrics {
        cost_usd: Option<f64>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
}

/// How an interactive spawn's conversation id is decided, as the plugin plans
/// it.
#[derive(Debug, Clone, Default)]
pub enum ConversationPlan {
    /// The plugin chose the id the CLI starts its conversation under.
    Minted(String),
    /// The CLI continues this existing conversation.
    Resumed(String),
    /// The app runs the agent's program with `args` before the spawn and
    /// reads the new conversation's id from its output with `parse`.
    PreCreate {
        args: Vec<String>,
        parse: fn(&str) -> Option<String>,
    },
    /// The app looks the conversation up with
    /// [`Agent::find_conversations`] once the CLI runs.
    DiscoverAfterLaunch,
    /// The spawn records no conversation.
    #[default]
    None,
}

/// What [`Agent::find_conversations`] is asked: the conversations recorded
/// for `folder`, either the one `known_id` names or those active inside
/// `window`.
pub struct ConversationQuery<'a> {
    /// The folder the session ran in.
    pub folder: &'a Path,
    /// The span, both ends included, a conversation must have been active in.
    pub window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The conversation the session recorded, when it recorded one.
    pub known_id: Option<&'a str>,
    /// The session's environment rows, so a plugin honours a session's own
    /// home variable.
    pub rows: &'a ResolvedRows,
}

/// One conversation an agent recorded for a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundConversation {
    /// The conversation id, as the CLI resumes it.
    pub id: String,
    /// When the conversation was last written.
    pub last_active: DateTime<Utc>,
    /// The conversation's title, tidied for display, when it has one.
    pub title: Option<String>,
}

/// Whether `id` is a plain conversation id: non-empty ASCII letters, digits,
/// `-` and `_` only, so it can become a file-name stem or a CLI argument
/// without naming a path.
#[must_use]
pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An environment value whose text is never printed: no `Display`, no
/// `Serialize`, and a `Debug` that shows `Secret(<redacted>)`.
pub struct Secret(String);

impl Secret {
    /// Wrap a value so it redacts its text wherever it is printed.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The value's text, for the one place that needs it: the environment a
    /// child process is spawned with.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Where a resolved value came from, so a log line can say so without the
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The daemon's own process environment.
    Process,
    /// The user's persistent environment (`HKCU\Environment` on Windows).
    UserScope,
    /// The machine's environment (`HKLM\...\Session Manager\Environment` on
    /// Windows).
    SystemScope,
}

/// The app's lookup of a named variable the way the user set it, read at
/// each spawn and never stored. Any `Fn(&str) -> Option<(Secret, Origin)>`
/// is one.
pub trait KeyLookup {
    /// `name`'s value and where it came from, or `None` when it is unset.
    fn key(&self, name: &str) -> Option<(Secret, Origin)>;
}

impl<F: Fn(&str) -> Option<(Secret, Origin)>> KeyLookup for F {
    fn key(&self, name: &str) -> Option<(Secret, Origin)> {
        self(name)
    }
}

/// A spawn's environment rows after the app resolved their `${env:}` and
/// `${secret:}` references: values included, never stored. Names match
/// case-insensitively, as Windows environment names do.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ResolvedRows(Vec<(String, String)>);

impl ResolvedRows {
    /// The rows, in the order the spawn sent them.
    #[must_use]
    pub fn new(rows: Vec<(String, String)>) -> Self {
        Self(rows)
    }

    /// Every row as `(name, value)`, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The value of the last row named `name`: the one the child gets when
    /// the spawn sends a name twice.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether a row names `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

/// Shows the row names only: the values hold resolved secrets.
impl fmt::Debug for ResolvedRows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(k, _)| k))
            .finish()
    }
}

/// What a plugin checks about a spawn request before anything is sealed,
/// written or checked out.
pub struct SpawnCheck<'a> {
    /// The session's mode.
    pub mode: protocol::SessionMode,
    /// The request's environment rows as sent, references unresolved; only
    /// their names may be read.
    pub rows: &'a [(String, String)],
    /// The app's lookup, for a key whose presence the plugin requires.
    pub keys: &'a dyn KeyLookup,
}

/// A plugin refused a spawn: the modal the user sees, naming variables and
/// never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub title: String,
    pub detail: String,
    pub hint: Option<String>,
}

/// The plugin's part of a spawn's environment: `set` goes on top of the
/// app's keep-list and the resolved rows, and every name in `remove` is
/// taken away from the child.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct EnvPlan {
    /// Variables the child gets, in order, each name once.
    pub set: Vec<(String, String)>,
    /// Variables the child must not get, even when the daemon has them.
    pub remove: Vec<String>,
}

/// Shows the variable names only: `set` holds resolved secrets and keys.
impl fmt::Debug for EnvPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set_keys: Vec<&str> = self.set.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("EnvPlan")
            .field("set_keys", &set_keys)
            .field("remove", &self.remove)
            .finish()
    }
}

/// What [`Agent::recovery_options`] is asked: the options a session recorded
/// when it ran, if the app has them, and whether it ran trusted.
#[derive(Debug, Clone, Copy)]
pub struct RecoveredRun<'a> {
    /// The options the session ran with, as the app recorded them; `None`
    /// when it recorded none.
    pub recorded: Option<&'a OptionValues>,
    /// Whether the session ran trusted. The app carries the trusted flag on
    /// the recovery's request itself; Claude's options do not read it.
    pub trusted: bool,
}

impl RecoveredRun<'_> {
    /// The recorded options as `O`, or `None` when none were recorded. The
    /// [`AgentPlugin`] adapter refuses recorded options that do not decode
    /// before an agent sees them, so a typed agent reads a decode failure
    /// as none.
    #[must_use]
    pub fn recorded_as<O: DeserializeOwned>(&self) -> Option<O> {
        let values = self.recorded?;
        serde_json::to_value(values)
            .and_then(serde_json::from_value)
            .ok()
    }
}

/// One run a tracer log describes, as [`Agent::import_tracer_run`] reads it:
/// the program the tracer started, its arguments and its working folder.
#[derive(Debug, Clone, Copy)]
pub struct TracerRun<'a> {
    /// The program, as the log names it (a bare name or a path).
    pub program: &'a str,
    /// Its arguments, in order.
    pub args: &'a [String],
    /// The folder it ran in.
    pub cwd: &'a str,
}

/// A run an agent recognised as its own in a tracer log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRun<O> {
    /// The program name the imported session is listed under.
    pub program_name: String,
    /// The folders the run had on top of its working folder.
    pub extra_dirs: Vec<String>,
    /// The options the run's arguments show.
    pub options: O,
    /// Whether the run was trusted.
    pub trusted: bool,
}

/// A plugin's stand-in for its CLI, which the live test tier runs in place
/// of the real program.
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCli {
    /// The platform's shim that starts the fake: the one the agent's
    /// [`Agent::program_env_var`] is pointed at.
    pub shim: PathBuf,
    /// A variable the harness points at a fresh, empty home folder per test,
    /// so the agent never reads or writes the user's own.
    pub home_env: Option<&'static str>,
    /// A program name the harness shadows on `PATH` with a stub, so a shell
    /// the daemon starts never runs the user's real CLI.
    pub path_stub: Option<&'static str>,
}

/// Implemented once per supported CLI, over the agent's own typed options.
/// The daemon never calls it directly: it reaches every agent through the
/// [`AgentPlugin`] this crate implements for it, so the daemon doesn't need
/// to know which agent it is talking to.
pub trait Agent: Send + Sync + 'static {
    /// The agent's options, decoded from the spawn's [`OptionValues`]. Its
    /// serialized form must match the descriptor's fields: each field's id is
    /// a key, and every value the schema accepts must decode (the
    /// `conformance` check under the `test-support` feature proves it).
    type Options: DeserializeOwned + Serialize + Default;

    /// The agent's id, names, look, capabilities and option schema.
    fn descriptor(&self) -> &AgentDescriptor;

    /// Environment-variable name that overrides the default executable name.
    /// Example: `"RUSTLING_TULIP_CLAUDE"`.
    fn program_env_var(&self) -> &'static str;

    /// Fallback executable name when [`Self::program_env_var`] is unset.
    /// This is the name looked up on `PATH`.
    fn default_program(&self) -> &'static str;

    /// Variables of the daemon's environment the agent's child gets on top
    /// of the app's keep-list, each when the daemon has it.
    fn passthrough_vars(&self) -> &'static [&'static str] {
        &[]
    }

    /// Variables a running session of the agent sets for its own children.
    /// A daemon started from inside such a session would pass them to every
    /// session it spawns, marking each as that session's child, so every PTY
    /// spawn removes them unless the spawn sets one itself.
    fn inherited_session_vars(&self) -> &'static [&'static str] {
        &[]
    }

    /// The flags the plugin's own argv carries, interactive or headless. A
    /// user's extra launch argument may not name one: the daemon refuses it
    /// when the launch command is saved.
    fn reserved_args(&self) -> &'static [&'static str] {
        &[]
    }

    /// The process names a session of this agent may run as, matched against
    /// a live process when a sidecar names no program.
    fn liveness_tokens(&self) -> &'static [&'static str];

    /// The options a recovered run spawns with, from what `run` recorded.
    fn recovery_options(&self, run: &RecoveredRun<'_>) -> Self::Options;

    /// The revision of this agent's tracer-log import; an older import of
    /// this agent is read again.
    fn import_rev(&self) -> u32 {
        0
    }

    /// The run `run` describes when it is this agent's, or `None` for a run
    /// of another program.
    fn import_tracer_run(&self, _run: &TracerRun<'_>) -> Option<ImportedRun<Self::Options>> {
        None
    }

    /// Refuse a spawn request before anything is sealed, written or checked
    /// out. Reads the rows' names only.
    ///
    /// # Errors
    ///
    /// The refusal the user sees, naming a variable and never a value.
    fn check_request(&self, opts: &Self::Options, req: &SpawnCheck<'_>) -> Result<(), Refusal>;

    /// The agent's part of the child's environment for a spawn whose
    /// resolved rows are `rows`, reading any key it needs through `keys`.
    ///
    /// # Errors
    ///
    /// The refusal the user sees, naming a variable and never a value.
    fn spawn_env(
        &self,
        opts: &Self::Options,
        rows: &ResolvedRows,
        keys: &dyn KeyLookup,
    ) -> Result<EnvPlan, Refusal>;

    /// How an interactive spawn's conversation id is decided; `resume` is the
    /// conversation the spawn continues, when it continues one.
    fn conversation_plan(&self, opts: &Self::Options, resume: Option<&str>) -> ConversationPlan;

    /// The conversations the agent recorded for `query`'s folder: the one its
    /// known id names, else those active inside its window, newest first. An
    /// agent that keeps none finds none.
    fn find_conversations(&self, _query: &ConversationQuery<'_>) -> Vec<FoundConversation> {
        Vec::new()
    }

    /// The argv passed to the CLI for an interactive (PTY-attached) session.
    /// Does not include the executable name itself — that is resolved by
    /// [`AgentPlugin::resolve_program`].
    fn interactive_args(
        &self,
        opts: &Self::Options,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String>;

    /// The argv passed to the CLI for a headless session, or `None` for an
    /// agent that has no headless mode.
    fn headless_args(
        &self,
        _opts: &Self::Options,
        _ctx: &SpawnContext<'_>,
        _members: &[SessionMember],
        _prompt: &str,
    ) -> Option<Vec<String>> {
        None
    }

    /// Read one raw stdout line emitted by a headless child into the changes
    /// it makes to the session, in order. A line that carries none, or that
    /// does not parse, yields no events.
    fn parse_headless_line(&self, _line: &str) -> Vec<HeadlessEvent> {
        Vec::new()
    }

    /// The hook files and argv an interactive spawn of this CLI carries, or an
    /// empty plan for a CLI that takes none. The daemon writes the plan's
    /// files into the session's own directory and appends the plan's args to
    /// the spawn with each `{file:<name>}` replaced by the written path.
    fn hook_plan(&self, _opts: &Self::Options, _trusted: bool, _shim: &HookShim<'_>) -> HookPlan {
        HookPlan::default()
    }

    /// What one hook payload says about its session, or `None` for a payload
    /// the agent does not know. `spawn` is the session's spawn options, when
    /// the session has a record that carries them, and `trusted` whether it
    /// was spawned trusted.
    fn map_hook_event(
        &self,
        _body: &serde_json::Value,
        _spawn: Option<&Self::Options>,
        _trusted: bool,
    ) -> Option<HookReport> {
        None
    }

    /// The command a recovered shell types to resume conversation
    /// `conversation_id` in the agent, after the shell's spawn wrote the hook
    /// files `hook_files` from [`Self::hook_plan`] (empty when it wrote
    /// none), or `None` for an agent that cannot be resumed from a shell.
    ///
    /// # Errors
    ///
    /// The agent refuses to resume a conversation on these options from a
    /// shell; the spawn is refused with the message. A refusal depends on
    /// `opts` alone, never on `conversation_id` or `hook_files`, so the app
    /// may ask with any id to learn whether a shell recovery is allowed.
    fn shell_resume(
        &self,
        _opts: &Self::Options,
        _conversation_id: &str,
        _hook_files: &[PathBuf],
    ) -> Result<Option<String>, AgentError> {
        Ok(None)
    }

    /// The rules the daemon reads this agent's rendered screen with, to tell
    /// whether a session is working, waits for an answer or is idle. A plugin
    /// that hands over none is read by its hooks alone.
    fn screen_rules(&self) -> &ScreenRules {
        &EMPTY_SCREEN_RULES
    }

    /// A reader of the transcript `source` names, which the daemon keeps and
    /// asks for the session's activity on each Dashboard tick. `None` for an
    /// agent with no transcript to read.
    fn activity_reader(&self, _source: &ActivitySource) -> Option<Box<dyn ActivityReader>> {
        None
    }

    /// How much output the daemon must see and how long it must then stay
    /// quiet before this agent's TUI counts as up, so a prompt can be typed
    /// into it.
    fn ready_signal(&self) -> ReadySignal {
        ReadySignal::default()
    }

    /// How a prompt names the repo-relative file `repo_relative` so this agent
    /// reads it. Defaults to the bare path.
    fn file_reference(&self, repo_relative: &str) -> String {
        repo_relative.to_owned()
    }

    /// The fake CLI the live test tier runs for this agent, or `None` for an
    /// agent that ships none.
    #[cfg(feature = "test-support")]
    fn fake_cli(&self) -> Option<FakeCli> {
        None
    }
}

/// An [`Agent`] as the registry holds it: object-safe, taking the spawn's
/// [`OptionValues`] where the typed trait takes its options. Every [`Agent`]
/// implements it; a method that builds a spawn decodes the values into the
/// agent's options and refuses the spawn with an [`AgentError`] naming the
/// agent when they do not decode.
pub trait AgentPlugin: Send + Sync {
    /// See [`Agent::descriptor`].
    fn descriptor(&self) -> &AgentDescriptor;

    /// See [`Agent::program_env_var`].
    fn program_env_var(&self) -> &'static str;

    /// See [`Agent::default_program`].
    fn default_program(&self) -> &'static str;

    /// See [`Agent::passthrough_vars`].
    fn passthrough_vars(&self) -> &'static [&'static str];

    /// See [`Agent::inherited_session_vars`].
    fn inherited_session_vars(&self) -> &'static [&'static str];

    /// See [`Agent::reserved_args`].
    fn reserved_args(&self) -> &'static [&'static str];

    /// See [`Agent::liveness_tokens`].
    fn liveness_tokens(&self) -> &'static [&'static str];

    /// [`Agent::recovery_options`] as option values.
    ///
    /// # Errors
    ///
    /// One naming the agent when `run`'s recorded options do not decode.
    fn recovery_options(&self, run: &RecoveredRun<'_>) -> Result<OptionValues, AgentError>;

    /// See [`Agent::import_rev`].
    fn import_rev(&self) -> u32;

    /// [`Agent::import_tracer_run`] with the run's options as option values.
    fn import_tracer_run(&self, run: &TracerRun<'_>) -> Option<ImportedRun<OptionValues>>;

    /// [`Agent::check_request`] over `opts` decoded.
    ///
    /// # Errors
    ///
    /// The agent's refusal, or one naming the agent when `opts` do not
    /// decode.
    fn check_request(&self, opts: &OptionValues, req: &SpawnCheck<'_>) -> Result<(), Refusal>;

    /// [`Agent::spawn_env`] over `opts` decoded.
    ///
    /// # Errors
    ///
    /// The agent's refusal, or one naming the agent when `opts` do not
    /// decode.
    fn spawn_env(
        &self,
        opts: &OptionValues,
        rows: &ResolvedRows,
        keys: &dyn KeyLookup,
    ) -> Result<EnvPlan, Refusal>;

    /// [`Agent::conversation_plan`] over `opts` decoded; when they do not
    /// decode, an error is logged and the spawn records no conversation.
    fn conversation_plan(&self, opts: &OptionValues, resume: Option<&str>) -> ConversationPlan;

    /// See [`Agent::find_conversations`].
    fn find_conversations(&self, query: &ConversationQuery<'_>) -> Vec<FoundConversation>;

    /// [`Agent::interactive_args`] over `opts` decoded.
    fn interactive_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Result<Vec<String>, AgentError>;

    /// [`Agent::headless_args`] over `opts` decoded; `Ok(None)` for an agent
    /// with no headless mode.
    fn headless_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: &str,
    ) -> Result<Option<Vec<String>>, AgentError>;

    /// See [`Agent::parse_headless_line`].
    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent>;

    /// [`Agent::hook_plan`] over `opts` decoded.
    ///
    /// # Errors
    ///
    /// One naming the agent when `opts` do not decode.
    fn hook_plan(
        &self,
        opts: &OptionValues,
        trusted: bool,
        shim: &HookShim<'_>,
    ) -> Result<HookPlan, AgentError>;

    /// [`Agent::map_hook_event`] over `spawn` decoded; when they do not
    /// decode, an error is logged and the payload is mapped with no options.
    fn map_hook_event(
        &self,
        body: &serde_json::Value,
        spawn: Option<&OptionValues>,
        trusted: bool,
    ) -> Option<HookReport>;

    /// [`Agent::shell_resume`] over `opts` decoded.
    ///
    /// # Errors
    ///
    /// The agent's refusal, or one naming the agent when `opts` do not
    /// decode.
    fn shell_resume(
        &self,
        opts: &OptionValues,
        conversation_id: &str,
        hook_files: &[PathBuf],
    ) -> Result<Option<String>, AgentError>;

    /// See [`Agent::screen_rules`].
    fn screen_rules(&self) -> &ScreenRules;

    /// See [`Agent::activity_reader`].
    fn activity_reader(&self, source: &ActivitySource) -> Option<Box<dyn ActivityReader>>;

    /// See [`Agent::ready_signal`].
    fn ready_signal(&self) -> ReadySignal;

    /// See [`Agent::file_reference`].
    fn file_reference(&self, repo_relative: &str) -> String;

    /// See [`Agent::fake_cli`].
    #[cfg(feature = "test-support")]
    fn fake_cli(&self) -> Option<FakeCli>;

    /// Resolve the executable path (the env-var override, or the default).
    /// Concrete `which`/shim resolution lives in the daemon's
    /// `resolve_agent_program`.
    fn resolve_program(&self) -> String {
        std::env::var(self.program_env_var()).unwrap_or_else(|_| self.default_program().to_string())
    }
}

impl<A: Agent> AgentPlugin for A {
    fn descriptor(&self) -> &AgentDescriptor {
        Agent::descriptor(self)
    }

    fn program_env_var(&self) -> &'static str {
        Agent::program_env_var(self)
    }

    fn default_program(&self) -> &'static str {
        Agent::default_program(self)
    }

    fn passthrough_vars(&self) -> &'static [&'static str] {
        Agent::passthrough_vars(self)
    }

    fn inherited_session_vars(&self) -> &'static [&'static str] {
        Agent::inherited_session_vars(self)
    }

    fn reserved_args(&self) -> &'static [&'static str] {
        Agent::reserved_args(self)
    }

    fn liveness_tokens(&self) -> &'static [&'static str] {
        Agent::liveness_tokens(self)
    }

    fn recovery_options(&self, run: &RecoveredRun<'_>) -> Result<OptionValues, AgentError> {
        if let Some(values) = run.recorded {
            decode_options(self, values)?;
        }
        encode_options(self, &Agent::recovery_options(self, run))
    }

    fn import_rev(&self) -> u32 {
        Agent::import_rev(self)
    }

    fn import_tracer_run(&self, run: &TracerRun<'_>) -> Option<ImportedRun<OptionValues>> {
        let imported = Agent::import_tracer_run(self, run)?;
        match encode_options(self, &imported.options) {
            Ok(options) => Some(ImportedRun {
                program_name: imported.program_name,
                extra_dirs: imported.extra_dirs,
                options,
                trusted: imported.trusted,
            }),
            Err(err) => {
                error!(%err, "dropping an imported run whose options do not serialize");
                None
            }
        }
    }

    fn check_request(&self, opts: &OptionValues, req: &SpawnCheck<'_>) -> Result<(), Refusal> {
        let opts = decode_for_spawn(self, opts)?;
        Agent::check_request(self, &opts, req)
    }

    fn spawn_env(
        &self,
        opts: &OptionValues,
        rows: &ResolvedRows,
        keys: &dyn KeyLookup,
    ) -> Result<EnvPlan, Refusal> {
        let opts = decode_for_spawn(self, opts)?;
        Agent::spawn_env(self, &opts, rows, keys)
    }

    fn conversation_plan(&self, opts: &OptionValues, resume: Option<&str>) -> ConversationPlan {
        match decode_options(self, opts) {
            Ok(opts) => Agent::conversation_plan(self, &opts, resume),
            Err(err) => {
                error!(%err, "spawning without a recorded conversation");
                ConversationPlan::None
            }
        }
    }

    fn find_conversations(&self, query: &ConversationQuery<'_>) -> Vec<FoundConversation> {
        Agent::find_conversations(self, query)
    }

    fn interactive_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Result<Vec<String>, AgentError> {
        let opts = decode_options(self, opts)?;
        Ok(Agent::interactive_args(
            self,
            &opts,
            ctx,
            members,
            initial_prompt,
        ))
    }

    fn headless_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: &str,
    ) -> Result<Option<Vec<String>>, AgentError> {
        let opts = decode_options(self, opts)?;
        Ok(Agent::headless_args(self, &opts, ctx, members, prompt))
    }

    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent> {
        Agent::parse_headless_line(self, line)
    }

    fn hook_plan(
        &self,
        opts: &OptionValues,
        trusted: bool,
        shim: &HookShim<'_>,
    ) -> Result<HookPlan, AgentError> {
        let opts = decode_options(self, opts)?;
        Ok(Agent::hook_plan(self, &opts, trusted, shim))
    }

    fn map_hook_event(
        &self,
        body: &serde_json::Value,
        spawn: Option<&OptionValues>,
        trusted: bool,
    ) -> Option<HookReport> {
        let opts = spawn.and_then(|values| match decode_options(self, values) {
            Ok(opts) => Some(opts),
            Err(err) => {
                error!(%err, "mapping a hook event without the spawn's options");
                None
            }
        });
        Agent::map_hook_event(self, body, opts.as_ref(), trusted)
    }

    fn shell_resume(
        &self,
        opts: &OptionValues,
        conversation_id: &str,
        hook_files: &[PathBuf],
    ) -> Result<Option<String>, AgentError> {
        let opts = decode_options(self, opts)?;
        Agent::shell_resume(self, &opts, conversation_id, hook_files)
    }

    fn screen_rules(&self) -> &ScreenRules {
        Agent::screen_rules(self)
    }

    fn activity_reader(&self, source: &ActivitySource) -> Option<Box<dyn ActivityReader>> {
        Agent::activity_reader(self, source)
    }

    fn ready_signal(&self) -> ReadySignal {
        Agent::ready_signal(self)
    }

    fn file_reference(&self, repo_relative: &str) -> String {
        Agent::file_reference(self, repo_relative)
    }

    #[cfg(feature = "test-support")]
    fn fake_cli(&self) -> Option<FakeCli> {
        Agent::fake_cli(self)
    }
}

/// `values` as `agent`'s typed options.
fn decode_options<A: Agent>(agent: &A, values: &OptionValues) -> Result<A::Options, AgentError> {
    serde_json::to_value(values)
        .and_then(serde_json::from_value)
        .map_err(|err| AgentError {
            message: format!(
                "{} could not read the spawn's options: {err}",
                Agent::descriptor(agent).label
            ),
        })
}

/// `options` as option values, or an error naming the agent when they do not
/// serialize into a map.
fn encode_options<A: Agent>(agent: &A, options: &A::Options) -> Result<OptionValues, AgentError> {
    serde_json::to_value(options)
        .and_then(serde_json::from_value)
        .map_err(|err| AgentError {
            message: format!(
                "{} could not write its options: {err}",
                Agent::descriptor(agent).label
            ),
        })
}

/// `values` as `agent`'s typed options, or the refusal of a spawn naming the
/// agent when they do not decode.
fn decode_for_spawn<A: Agent>(agent: &A, values: &OptionValues) -> Result<A::Options, Refusal> {
    decode_options(agent, values).map_err(|err| Refusal {
        title: format!(
            "Couldn't start the {} session",
            Agent::descriptor(agent).label
        ),
        detail: err.to_string(),
        hint: None,
    })
}

/// An agent refused a call: the spawn's options did not decode into the
/// agent's options, or the agent itself refused. The message names the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}

impl AgentError {
    /// An agent's own refusal, shown as `message`, which names the agent.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentError {}

/// A set of option values an agent's schema refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// The key whose value was refused.
    pub field: String,
    /// What is wrong with it, for people.
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Check `values` against `descriptor`'s option schema and answer the values
/// a spawn runs with: every field's value, its default where `values` names
/// none, without the fields a `when` rule hides.
///
/// Refuses a key the schema has no field for, a toggle's value that is not a
/// bool, a choice's value that is not one of its choices (no value only when
/// the field's default is no value), and a text or model value that is
/// neither text nor no value.
pub fn validate(
    descriptor: &AgentDescriptor,
    values: &OptionValues,
) -> Result<OptionValues, ValidationError> {
    if let Some(key) = values
        .keys()
        .find(|key| !descriptor.fields.iter().any(|field| &field.id == *key))
    {
        return Err(ValidationError {
            field: key.clone(),
            message: format!("{} has no option `{key}`", descriptor.label),
        });
    }
    let mut checked = OptionValues::new();
    for field in &descriptor.fields {
        let value = values.get(&field.id).unwrap_or(&field.default);
        if !accepts(field, value) {
            return Err(ValidationError {
                field: field.id.clone(),
                message: format!(
                    "{}'s option `{}` does not take {}",
                    descriptor.label,
                    field.id,
                    describe(value)
                ),
            });
        }
        checked.insert(field.id.clone(), value.clone());
    }
    let hidden: Vec<&str> = descriptor
        .fields
        .iter()
        .filter(|field| is_hidden(field, &checked))
        .map(|field| field.id.as_str())
        .collect();
    for id in hidden {
        checked.remove(id);
    }
    Ok(checked)
}

/// Whether `field` takes `value`. A kind from a newer peer takes anything.
fn accepts(field: &OptionField, value: &OptionValue) -> bool {
    match (&field.kind, value) {
        (FieldKind::Toggle, OptionValue::Bool(_))
        | (
            FieldKind::Text { .. } | FieldKind::Model { .. },
            OptionValue::Text(_) | OptionValue::Null,
        )
        | (FieldKind::Unknown, _) => true,
        (FieldKind::Choice { choices, .. }, OptionValue::Text(text)) => {
            choices.iter().any(|choice| &choice.value == text)
        }
        (FieldKind::Choice { .. }, OptionValue::Null) => field.default == OptionValue::Null,
        _ => false,
    }
}

/// Whether one of `field`'s rules hides it while the fields hold `values`.
fn is_hidden(field: &OptionField, values: &OptionValues) -> bool {
    field.when.iter().any(|rule| {
        rule.then.hidden == Some(true) && values.get(&rule.if_field) == Some(&rule.equals)
    })
}

/// The chips a session whose agent options hold `options` shows: one per
/// chip rule of `descriptor` whose field holds a value other than no value
/// and the rule's `hide_when`. A chip's text is the label of the choice the
/// field holds, else the value itself; its tip is the rule's.
#[must_use]
pub fn session_chips(descriptor: &AgentDescriptor, options: &OptionValues) -> Vec<SessionChip> {
    descriptor
        .chips
        .iter()
        .filter_map(|rule| {
            let value = options.get(&rule.field)?;
            if *value == rule.hide_when {
                return None;
            }
            let raw = match value {
                OptionValue::Bool(flag) => flag.to_string(),
                OptionValue::Text(text) => text.clone(),
                OptionValue::Null => return None,
            };
            Some(SessionChip {
                text: choice_label(descriptor, &rule.field, value).unwrap_or(raw),
                tip: Some(rule.tip.clone()),
            })
        })
        .collect()
}

/// The label of the choice `value` is, when `field_id` names a choice field
/// of `descriptor` that offers it.
fn choice_label(
    descriptor: &AgentDescriptor,
    field_id: &str,
    value: &OptionValue,
) -> Option<String> {
    let field = descriptor
        .fields
        .iter()
        .find(|field| field.id == field_id)?;
    match (&field.kind, value) {
        (FieldKind::Choice { choices, .. }, OptionValue::Text(text)) => choices
            .iter()
            .find(|choice| &choice.value == text)
            .map(|choice| choice.label.clone()),
        _ => None,
    }
}

/// `value` as an error message names it.
fn describe(value: &OptionValue) -> String {
    match value {
        OptionValue::Bool(flag) => flag.to_string(),
        OptionValue::Text(text) => format!("\"{text}\""),
        OptionValue::Null => "no value".to_string(),
    }
}

/// Build a workspace-context prelude for the agent. Returns `Some` only when
/// the session has 2+ members. The note maps each member's repo name to its
/// per-session worktree path so the agent doesn't try to navigate to
/// original-repo paths referenced in its `instruction_files` that no longer
/// match where the session is rooted.
#[must_use]
pub fn workspace_prelude(members: &[SessionMember], instruction_files: &[&str]) -> Option<String> {
    if members.len() < 2 {
        return None;
    }
    let mut out = format!(
        "Workspace member paths for this session (use these for cross-repo \
         file access — they override any absolute paths referenced in \
         {}):\n",
        instruction_files.join(" / ")
    );
    let name_width = members.iter().map(|m| m.repo_name.len()).max().unwrap_or(0);
    for m in members {
        use std::fmt::Write as _;
        // Width-padded for visual alignment in the agent's view; failure
        // here would mean OOM during string formatting, which we treat as
        // unreachable for a few dozen members at most.
        let _ = writeln!(
            out,
            "  {name:<width$}  ->  {path}",
            name = m.repo_name,
            width = name_width,
            path = m.worktree_path,
        );
    }
    Some(out)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::test_agent::{DESCRIPTOR, TestAgent};

    fn text(value: &str) -> OptionValue {
        OptionValue::Text(value.to_string())
    }

    fn values(pairs: &[(&str, OptionValue)]) -> OptionValues {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    /// An agent that keeps every default the trait offers.
    struct Bare;

    impl Agent for Bare {
        type Options = crate::test_agent::TestOptions;

        fn descriptor(&self) -> &AgentDescriptor {
            &DESCRIPTOR
        }

        fn program_env_var(&self) -> &'static str {
            "RUSTLING_TULIP_BARE_AGENT"
        }

        fn default_program(&self) -> &'static str {
            "bare-agent"
        }

        fn liveness_tokens(&self) -> &'static [&'static str] {
            &["bare-agent"]
        }

        fn recovery_options(&self, _run: &RecoveredRun<'_>) -> Self::Options {
            crate::test_agent::TestOptions::default()
        }

        fn check_request(
            &self,
            _opts: &Self::Options,
            _req: &SpawnCheck<'_>,
        ) -> Result<(), Refusal> {
            Ok(())
        }

        fn spawn_env(
            &self,
            _opts: &Self::Options,
            _rows: &ResolvedRows,
            _keys: &dyn KeyLookup,
        ) -> Result<EnvPlan, Refusal> {
            Ok(EnvPlan::default())
        }

        fn conversation_plan(
            &self,
            _opts: &Self::Options,
            _resume: Option<&str>,
        ) -> ConversationPlan {
            ConversationPlan::None
        }

        fn interactive_args(
            &self,
            _opts: &Self::Options,
            _ctx: &SpawnContext<'_>,
            _members: &[SessionMember],
            _initial_prompt: Option<&str>,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn find_conversations_defaults_to_none_found() {
        let plugin: &dyn AgentPlugin = &Bare;
        let rows = ResolvedRows::default();
        let query = ConversationQuery {
            folder: Path::new(r"D:\proj"),
            window: None,
            known_id: Some("abc"),
            rows: &rows,
        };
        assert_eq!(plugin.find_conversations(&query), []);
    }

    #[test]
    fn adapter_plans_no_conversation_when_options_do_not_decode() {
        let plugin: &dyn AgentPlugin = &TestAgent::new(ConversationPlan::Minted("m-1".to_string()));
        let good = values(&[("mode", text("slow")), ("sandbox", OptionValue::Bool(true))]);
        assert!(
            matches!(plugin.conversation_plan(&good, None), ConversationPlan::Minted(id) if id == "m-1")
        );
        let bad = values(&[("mode", text("slow")), ("sandbox", text("yes"))]);
        assert!(matches!(
            plugin.conversation_plan(&bad, Some("r-1")),
            ConversationPlan::None
        ));
    }

    #[test]
    fn test_agent_answers_the_conversations_it_was_given() {
        let found = FoundConversation {
            id: "c-1".to_string(),
            last_active: chrono::DateTime::UNIX_EPOCH,
            title: Some("hello".to_string()),
        };
        let agent = TestAgent::default();
        agent.set_found(r"D:\proj", vec![found.clone()]);
        let plugin: &dyn AgentPlugin = &agent;
        let rows = ResolvedRows::default();
        let query = ConversationQuery {
            folder: Path::new(r"D:\proj"),
            window: None,
            known_id: None,
            rows: &rows,
        };
        assert_eq!(plugin.find_conversations(&query), [found]);
    }

    #[test]
    fn get_returns_the_last_of_two_same_named_rows() {
        let rows = ResolvedRows::new(vec![
            ("CLAUDE_CONFIG_DIR".to_string(), "first".to_string()),
            ("OTHER".to_string(), "x".to_string()),
            ("claude_config_dir".to_string(), "last".to_string()),
        ]);
        assert_eq!(rows.get("Claude_Config_Dir"), Some("last"));
        assert!(rows.contains("other"));
        assert_eq!(rows.get("MISSING"), None);
    }

    #[test]
    fn is_plain_id_takes_only_file_name_stems() {
        assert!(is_plain_id("0f1e2d3c-4b5a_x"));
        for bad in ["", "..", r"..\x", "a/b", "a b", "a.jsonl"] {
            assert!(!is_plain_id(bad), "{bad}");
        }
    }

    fn no_key(_name: &str) -> Option<(Secret, Origin)> {
        None
    }

    #[test]
    fn shell_resume_defaults_to_none() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let good = values(&[("mode", text("slow")), ("sandbox", OptionValue::Bool(true))]);
        let files = [std::path::PathBuf::from("C:/rt/sessions/s-1/hooks.json")];
        assert_eq!(plugin.shell_resume(&good, "c-1", &files), Ok(None));
        let bad = values(&[("mode", text("slow")), ("sandbox", text("yes"))]);
        let err = plugin
            .shell_resume(&bad, "c-1", &files)
            .expect_err("options that do not decode refuse the resume");
        assert!(err.to_string().contains("Test agent"), "{err}");
    }

    #[test]
    fn recovery_options_defaults_to_replaying_the_recorded_options() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let recorded = values(&[("mode", text("slow")), ("sandbox", OptionValue::Bool(true))]);
        let replayed = plugin.recovery_options(&RecoveredRun {
            recorded: Some(&recorded),
            trusted: true,
        });
        assert_eq!(
            replayed,
            Ok(values(&[
                ("mode", text("slow")),
                ("sandbox", OptionValue::Bool(true)),
                ("profile", OptionValue::Null),
            ]))
        );
        let fresh = plugin.recovery_options(&RecoveredRun {
            recorded: None,
            trusted: false,
        });
        assert_eq!(
            fresh,
            Ok(values(&[
                ("mode", text("")),
                ("sandbox", OptionValue::Bool(false)),
                ("profile", OptionValue::Null),
            ]))
        );
    }

    #[test]
    fn recovery_options_decode_failure_is_an_agent_error() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let bad = values(&[("mode", text("slow")), ("sandbox", text("yes"))]);
        let err = plugin
            .recovery_options(&RecoveredRun {
                recorded: Some(&bad),
                trusted: false,
            })
            .expect_err("recorded options that do not decode refuse the recovery");
        assert!(err.to_string().contains("Test agent"), "{err}");
    }

    #[test]
    fn import_defaults_to_none() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let args = ["--model".to_string(), "opus".to_string()];
        let run = TracerRun {
            program: "test-agent.exe",
            args: &args,
            cwd: r"D:\proj",
        };
        assert_eq!(plugin.import_tracer_run(&run), None);
    }

    #[test]
    fn import_rev_defaults_to_zero() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        assert_eq!(plugin.import_rev(), 0);
    }

    #[test]
    fn liveness_tokens_reach_the_adapter() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        assert_eq!(plugin.liveness_tokens(), [] as [&str; 0]);
        let bare: &dyn AgentPlugin = &Bare;
        assert_eq!(bare.liveness_tokens(), ["bare-agent"]);
    }

    #[test]
    fn passthrough_vars_defaults_to_none() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        assert_eq!(plugin.passthrough_vars(), [] as [&str; 0]);
    }

    #[test]
    fn reserved_args_defaults_to_none() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        assert_eq!(plugin.reserved_args(), [] as [&str; 0]);
    }

    #[test]
    fn adapter_refuses_check_request_and_spawn_env_when_options_do_not_decode() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let check = SpawnCheck {
            mode: protocol::SessionMode::Interactive,
            rows: &[],
            keys: &no_key,
        };
        let rows = ResolvedRows::new(Vec::new());
        let good = values(&[("mode", text("slow")), ("sandbox", OptionValue::Bool(true))]);
        assert_eq!(plugin.check_request(&good, &check), Ok(()));
        let plan = plugin
            .spawn_env(&good, &rows, &no_key)
            .expect("decoded options build a plan");
        assert_eq!(plan.set, [("TEST_MODE".to_string(), "slow".to_string())]);
        let bad = values(&[("mode", text("slow")), ("sandbox", text("yes"))]);
        for refusal in [
            plugin
                .check_request(&bad, &check)
                .expect_err("options that do not decode refuse the check"),
            plugin
                .spawn_env(&bad, &rows, &no_key)
                .expect_err("options that do not decode refuse the environment"),
        ] {
            assert_eq!(refusal.title, "Couldn't start the Test agent session");
            assert!(refusal.detail.contains("Test agent"), "{}", refusal.detail);
            assert_eq!(refusal.hint, None);
        }
    }

    fn context() -> SpawnContext<'static> {
        SpawnContext {
            dangerously_skip_permissions: false,
            has_prompt_injector: false,
            conversation_id: None,
            resume_conversation: None,
            add_dirs: &[],
            hook_args: &[],
        }
    }

    #[test]
    fn a_plugin_without_hooks_plans_nothing() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let good = values(&[
            ("mode", text("slow")),
            ("sandbox", OptionValue::Bool(true)),
            ("profile", OptionValue::Null),
        ]);
        let shim = HookShim {
            tracer_exe: Path::new("C:/rt/binaries/rt-tracer.exe"),
            session_id: "s-1",
            config_dir: Path::new("C:/rt/config"),
        };
        assert_eq!(
            plugin.hook_plan(&good, false, &shim),
            Ok(HookPlan::default())
        );
    }

    #[test]
    fn validate_fills_defaults_and_drops_hidden() {
        assert_eq!(
            validate(&DESCRIPTOR, &OptionValues::new()),
            Ok(values(&[
                ("mode", text("fast")),
                ("profile", OptionValue::Null),
                ("sandbox", OptionValue::Bool(false)),
            ]))
        );
        let hidden = values(&[
            ("sandbox", OptionValue::Bool(true)),
            ("profile", text("work")),
        ]);
        assert_eq!(
            validate(&DESCRIPTOR, &hidden),
            Ok(values(&[
                ("mode", text("fast")),
                ("sandbox", OptionValue::Bool(true)),
            ]))
        );
    }

    #[test]
    fn validate_refuses_unknown_key() {
        let err = validate(&DESCRIPTOR, &values(&[("colour", text("red"))]))
            .expect_err("an unknown key refuses");
        assert_eq!(err.field, "colour");
        assert!(err.to_string().contains("colour"), "{err}");
    }

    #[test]
    fn validate_refuses_value_outside_choices() {
        for bad in [text("medium"), OptionValue::Null, OptionValue::Bool(true)] {
            let err = validate(&DESCRIPTOR, &values(&[("mode", bad.clone())]))
                .expect_err("a value outside the choices refuses");
            assert_eq!(err.field, "mode", "{bad:?}");
            assert!(err.to_string().contains("mode"), "{err}");
        }
        let err = validate(&DESCRIPTOR, &values(&[("sandbox", text("yes"))]))
            .expect_err("a toggle takes only a bool");
        assert_eq!(err.field, "sandbox");
    }

    /// A descriptor with a provider choice and a profile text field, each
    /// with a chip rule.
    fn chip_descriptor() -> AgentDescriptor {
        let choice = |value: &str, label: &str| protocol::Choice {
            value: value.to_string(),
            label: label.to_string(),
        };
        let mut provider = DESCRIPTOR.fields[0].clone();
        provider.id = "provider".to_string();
        provider.kind = FieldKind::Choice {
            choices: vec![
                choice("anthropic", "Anthropic"),
                choice("deepseek", "DeepSeek"),
            ],
            style: protocol::ChoiceStyle::Segmented,
        };
        let mut profile = DESCRIPTOR.fields[0].clone();
        profile.id = "profile".to_string();
        profile.kind = FieldKind::Text { placeholder: None };
        let rule = |field: &str, hide_when: OptionValue, tip: &str| protocol::ChipRule {
            field: field.to_string(),
            hide_when,
            tip: tip.to_string(),
        };
        AgentDescriptor {
            fields: vec![provider, profile],
            chips: vec![
                rule("provider", text("anthropic"), "Runs on another provider"),
                rule("profile", text(""), "Profile"),
            ],
            ..DESCRIPTOR.clone()
        }
    }

    #[test]
    fn session_chips_show_the_choice_label_and_tip() {
        assert_eq!(
            session_chips(
                &chip_descriptor(),
                &values(&[("provider", text("deepseek"))])
            ),
            vec![SessionChip {
                text: "DeepSeek".to_string(),
                tip: Some("Runs on another provider".to_string()),
            }]
        );
    }

    #[test]
    fn session_chips_hide_a_value_in_hide_when() {
        let descriptor = chip_descriptor();
        for options in [
            values(&[("provider", text("anthropic")), ("profile", text(""))]),
            values(&[
                ("provider", OptionValue::Null),
                ("profile", OptionValue::Null),
            ]),
            OptionValues::new(),
        ] {
            assert_eq!(
                session_chips(&descriptor, &options),
                Vec::<SessionChip>::new(),
                "{options:?}"
            );
        }
    }

    #[test]
    fn session_chips_fall_back_to_the_raw_value() {
        assert_eq!(
            session_chips(
                &chip_descriptor(),
                &values(&[("provider", text("mistral")), ("profile", text("work"))])
            ),
            vec![
                SessionChip {
                    text: "mistral".to_string(),
                    tip: Some("Runs on another provider".to_string()),
                },
                SessionChip {
                    text: "work".to_string(),
                    tip: Some("Profile".to_string()),
                },
            ]
        );
    }

    #[test]
    fn adapter_refuses_options_that_do_not_decode() {
        let plugin: &dyn AgentPlugin = &TestAgent::default();
        let good = values(&[
            ("mode", text("slow")),
            ("sandbox", OptionValue::Bool(true)),
            ("profile", OptionValue::Null),
        ]);
        assert_eq!(
            plugin.interactive_args(&good, &context(), &[], None),
            Ok(vec!["slow".to_string(), "true".to_string()])
        );
        assert_eq!(plugin.headless_args(&good, &context(), &[], "hi"), Ok(None));
        let bad = values(&[("mode", text("slow")), ("sandbox", text("yes"))]);
        let err = plugin
            .interactive_args(&bad, &context(), &[], None)
            .expect_err("options that do not decode refuse the spawn");
        assert!(err.to_string().contains("Test agent"), "{err}");
        plugin
            .headless_args(&bad, &context(), &[], "hi")
            .expect_err("options that do not decode refuse a headless spawn");
    }

    #[test]
    fn option_value_round_trips_untagged() {
        let map = values(&[
            ("a", OptionValue::Bool(true)),
            ("b", text("x")),
            ("c", OptionValue::Null),
        ]);
        let json = serde_json::to_string(&map).expect("option values serialize");
        assert_eq!(json, r#"{"a":true,"b":"x","c":null}"#);
        let back: OptionValues = serde_json::from_str(&json).expect("option values parse");
        assert_eq!(back, map);
    }

    fn member(name: &str, path: &str) -> SessionMember {
        SessionMember {
            repo_id: format!("id-{name}"),
            repo_name: name.to_string(),
            branch: "main".to_string(),
            worktree_path: path.to_string(),
        }
    }

    #[test]
    fn workspace_prelude_names_the_instruction_files() {
        let files = ["CLAUDE.md", "AGENTS.md"];
        let two = [member("a", "X:/wt/a"), member("longer", "X:/wt/longer")];
        assert_eq!(
            workspace_prelude(&two, &files).as_deref(),
            Some(
                "Workspace member paths for this session (use these for cross-repo file access \
                 — they override any absolute paths referenced in CLAUDE.md / AGENTS.md):\n  \
                 a       ->  X:/wt/a\n  longer  ->  X:/wt/longer\n"
            )
        );
        assert_eq!(workspace_prelude(&two[..1], &files), None);
    }

    #[test]
    fn activity_reader_defaults_to_none() {
        let plugin: &dyn AgentPlugin = &Bare;
        let source = ActivitySource {
            folder: PathBuf::from(r"D:\proj"),
            conversation_id: "abc".to_string(),
            rows: ResolvedRows::default(),
            process_started_at: None,
        };

        assert!(plugin.activity_reader(&source).is_none());
    }
}
