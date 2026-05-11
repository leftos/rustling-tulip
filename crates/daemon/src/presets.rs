//! Preset loader, template renderer, and batch-spawn orchestrator.
//!
//! Reads `.rustling-tulip/presets.json` from a repo's working directory,
//! resolves prompt sources + variables, then issues N spawn requests with
//! `stagger_ms` pacing between them. Optionally groups the resulting
//! sessions into one or more tabs via the [`crate::tabs`] module.
//!
//! See `docs/plans/i-d-like-to-plan-zesty-scott.md` for the full design.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use chrono::Utc;
use protocol::{
    FooterLine, GridNode, InjectorStep, InjectorTemplate, LaunchPresetSource, PresetEntry,
    PresetTarget, PresetVariable, PresetVariableKind, PromptInjector, RepoEntry, SessionMode,
    SpawnRequest, SpawnTarget, SplitDirection, TabEntry, TabGroupingConfig, TabLayout,
};
use tokio::time::sleep;
use tracing::{info, warn};
use uuid::Uuid;

use crate::server::{Hub, PresetEvent, TabEvent, spawn_session};
use crate::tabs;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn list(hub: &Hub, target: &PresetTarget) -> anyhow::Result<Vec<PresetEntry>> {
    match target {
        PresetTarget::Repo { repo_id } => {
            let repo = find_repo(hub, repo_id)?;
            load_repo_presets(Path::new(&repo.path), &repo.id).await
        }
        PresetTarget::Workspace { workspace_id } => list_workspace_presets(hub, workspace_id).await,
    }
}

pub struct LaunchArgs {
    pub target: PresetTarget,
    pub preset_id: String,
    pub source: LaunchPresetSource,
    pub variable_values: Vec<(String, String)>,
    pub use_worktree_override: Option<bool>,
    pub max_panes_per_tab_override: Option<u32>,
}

pub async fn launch(hub: Hub, args: LaunchArgs) {
    let preset_id = args.preset_id.clone();
    if let Err(failure) = run(&hub, args).await {
        warn!(
            preset_id = %preset_id,
            error = %failure.error,
            "preset launch failed"
        );
        let _ = hub.preset_events.send(PresetEvent::Failed {
            preset_id,
            error: failure.error,
            partial_session_ids: failure.partial_session_ids,
            partial_tab_ids: failure.partial_tab_ids,
        });
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

async fn list_workspace_presets(hub: &Hub, workspace_id: &str) -> anyhow::Result<Vec<PresetEntry>> {
    let member_ids = hub
        .state
        .with_persisted(|s| {
            s.workspaces
                .iter()
                .find(|w| w.id == workspace_id)
                .map(|w| w.member_repo_ids.clone())
        })
        .ok_or_else(|| anyhow!("unknown workspace: {workspace_id}"))?;
    let mut all = Vec::new();
    for repo_id in member_ids {
        let Ok(repo) = find_repo(hub, &repo_id) else {
            continue;
        };
        match load_repo_presets(Path::new(&repo.path), &repo.id).await {
            Ok(mut entries) => all.append(&mut entries),
            Err(err) => warn!(?err, repo_id = %repo.id, "failed to load presets"),
        }
    }
    Ok(all)
}

async fn load_repo_presets(repo_path: &Path, repo_id: &str) -> anyhow::Result<Vec<PresetEntry>> {
    let path = repo_path.join(".rustling-tulip").join("presets.json");
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(Vec::new());
    }
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut entries: Vec<PresetEntry> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    for entry in &mut entries {
        entry.source_repo_id = repo_id.to_string();
    }
    Ok(entries)
}

fn find_repo(hub: &Hub, repo_id: &str) -> anyhow::Result<RepoEntry> {
    hub.state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))
}

// ---------------------------------------------------------------------------
// Failure type
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LaunchFailure {
    error: String,
    partial_session_ids: Vec<String>,
    partial_tab_ids: Vec<String>,
}

impl LaunchFailure {
    fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            partial_session_ids: Vec::new(),
            partial_tab_ids: Vec::new(),
        }
    }

    fn with_partials(mut self, sessions: &[String], tabs: &[String]) -> Self {
        self.partial_session_ids = sessions.to_vec();
        self.partial_tab_ids = tabs.to_vec();
        self
    }
}

// ---------------------------------------------------------------------------
// Prompt parsing and resolution
// ---------------------------------------------------------------------------

struct RawPrompt {
    text: String,
    /// Only set when the source is a folder: the file stem (e.g. `auth-bug`
    /// for `auth-bug.md`). Available as `{stem}` in templates.
    stem: Option<String>,
}

/// Split a prompts file into individual prompts. Mirrors yaat's parser:
/// a leading `- ` (with optional indent) starts a single-line prompt;
/// otherwise lines accumulate into a paragraph until a blank line ends it.
fn parse_prompts(content: &str) -> Vec<String> {
    let mut prompts: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for raw in content.split('\n') {
        let line = raw.trim_end_matches('\r');
        let trimmed_start = line.trim_start();
        if let Some(rest) = trimmed_start
            .strip_prefix("- ")
            .or_else(|| trimmed_start.strip_prefix("-\t"))
        {
            flush_block(&mut prompts, &mut current);
            let text = rest.trim().to_string();
            if !text.is_empty() {
                prompts.push(text);
            }
        } else if line.trim().is_empty() {
            flush_block(&mut prompts, &mut current);
        } else {
            current.push(line.to_string());
        }
    }
    flush_block(&mut prompts, &mut current);
    prompts
}

fn flush_block(prompts: &mut Vec<String>, current: &mut Vec<String>) {
    if current.is_empty() {
        return;
    }
    let block = current.join("\n").trim().to_string();
    current.clear();
    if !block.is_empty() {
        prompts.push(block);
    }
}

async fn resolve_prompts(
    repo_path: &Path,
    source: &LaunchPresetSource,
) -> Result<Vec<RawPrompt>, LaunchFailure> {
    match source {
        LaunchPresetSource::File { path } => {
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| LaunchFailure::new(format!("reading prompts file '{path}': {e}")))?;
            let text = String::from_utf8_lossy(&bytes);
            Ok(parse_prompts(&text)
                .into_iter()
                .map(|text| RawPrompt { text, stem: None })
                .collect())
        }
        LaunchPresetSource::Folder { path } => resolve_folder_prompts(repo_path, path).await,
        LaunchPresetSource::Inline { prompts } => Ok(prompts
            .iter()
            .filter(|p| !p.trim().is_empty())
            .map(|t| RawPrompt {
                text: t.clone(),
                stem: None,
            })
            .collect()),
    }
}

async fn resolve_folder_prompts(
    repo_path: &Path,
    folder: &str,
) -> Result<Vec<RawPrompt>, LaunchFailure> {
    let folder_path = Path::new(folder);
    let mut entries = tokio::fs::read_dir(folder_path)
        .await
        .map_err(|e| LaunchFailure::new(format!("reading prompts folder '{folder}': {e}")))?;
    let mut files: Vec<(String, String)> = Vec::new();
    loop {
        let next = entries
            .next_entry()
            .await
            .map_err(|e| LaunchFailure::new(format!("scanning folder: {e}")))?;
        let Some(entry) = next else { break };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
            .to_string();
        files.push((stem, name.to_string()));
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    // Format prompts as `@<repo-relative-path>` so Claude reads them.
    let rel = folder_path.strip_prefix(repo_path).map_or_else(
        |_| folder_path.to_string_lossy().replace('\\', "/"),
        |p| p.to_string_lossy().replace('\\', "/"),
    );
    Ok(files
        .into_iter()
        .map(|(stem, name)| RawPrompt {
            text: format!("@{rel}/{name}"),
            stem: Some(stem),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Variable resolution
// ---------------------------------------------------------------------------

fn resolve_variables(
    preset_vars: &[PresetVariable],
    values: &[(String, String)],
    repo_root: &Path,
) -> Result<HashMap<String, String>, LaunchFailure> {
    let supplied: HashMap<&str, &str> = values
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut out: HashMap<String, String> = HashMap::new();
    for var in preset_vars {
        let value = resolve_one_variable(var, &supplied, repo_root);
        if value.is_empty() && !var.optional {
            return Err(LaunchFailure::new(format!(
                "variable '{}' is required but no value was supplied",
                var.name
            )));
        }
        out.insert(var.name.clone(), value);
    }
    Ok(out)
}

fn resolve_one_variable(
    var: &PresetVariable,
    supplied: &HashMap<&str, &str>,
    repo_root: &Path,
) -> String {
    match &var.kind {
        PresetVariableKind::Text
        | PresetVariableKind::FilePath { .. }
        | PresetVariableKind::FolderPath => supplied
            .get(var.name.as_str())
            .map(|s| (*s).to_string())
            .or_else(|| var.default.clone())
            .unwrap_or_default(),
        PresetVariableKind::EnvVar { name } => std::env::var(name).unwrap_or_default(),
        PresetVariableKind::LiteralPath { path } => expand_literal_path(path, repo_root),
    }
}

fn expand_literal_path(raw: &str, repo_root: &Path) -> String {
    let with_repo = raw.replace("{repo_root}", &repo_root.to_string_lossy());
    let after_dollar = expand_env_pattern(&with_repo, "${", "}");
    expand_env_pattern(&after_dollar, "%", "%")
}

fn expand_env_pattern(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            // No closing delimiter — bail and copy the remainder verbatim.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after_open[..end];
        out.push_str(&std::env::var(name).unwrap_or_default());
        rest = &after_open[end + close.len()..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Template rendering
// ---------------------------------------------------------------------------

struct TemplateContext<'a> {
    prompt: Option<&'a str>,
    index_str: String,
    slug: String,
    stem: Option<&'a str>,
    date: &'a str,
    datetime: &'a str,
    repo: &'a str,
    vars: &'a HashMap<String, String>,
}

impl TemplateContext<'_> {
    fn resolve(&self, name: &str) -> Option<&str> {
        match name {
            "prompt" => self.prompt,
            "index" => Some(&self.index_str),
            "slug" => Some(&self.slug),
            "stem" => self.stem,
            "date" => Some(self.date),
            "datetime" => Some(self.datetime),
            "repo" => Some(self.repo),
            _ => self.vars.get(name).map(String::as_str),
        }
    }
}

fn render_template(template: &str, ctx: &TemplateContext) -> Result<String, LaunchFailure> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return Err(LaunchFailure::new(format!(
                "unclosed '{{' in template: {template}"
            )));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(LaunchFailure::new(format!(
                "empty placeholder in template: {template}"
            )));
        }
        match ctx.resolve(name) {
            Some(value) => out.push_str(value),
            None => {
                return Err(LaunchFailure::new(format!(
                    "unresolved placeholder '{{{name}}}' in template: {template}"
                )));
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn render_footer(lines: &[FooterLine], vars: &HashMap<String, String>) -> String {
    let parts: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            let value = vars.get(&line.variable)?.as_str();
            if value.is_empty() {
                None
            } else {
                Some(format!("{}: {}", line.label, value))
            }
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", parts.join("\n"))
    }
}

fn slugify(first_line: &str) -> String {
    let mut out = String::new();
    let mut last_was_hyphen = false;
    for ch in first_line.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen && !out.is_empty() {
            out.push('-');
            last_was_hyphen = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Injector composition
// ---------------------------------------------------------------------------

fn build_injector(template: &InjectorTemplate, prompt_text: &str) -> PromptInjector {
    let mut steps = Vec::with_capacity(template.pre_input.len() + template.post_input.len() + 2);
    if template.startup_delay_ms > 0 {
        steps.push(InjectorStep::Delay {
            ms: template.startup_delay_ms,
        });
    }
    steps.extend(template.pre_input.iter().cloned());
    steps.push(InjectorStep::Text {
        content: prompt_text.to_string(),
        newline: false,
    });
    steps.extend(template.post_input.iter().cloned());
    PromptInjector { steps }
}

// ---------------------------------------------------------------------------
// Tab state
// ---------------------------------------------------------------------------

struct TabState {
    /// `None` when `tab_grouping = none`. Otherwise the primary split
    /// direction for `tabs::build_balanced`.
    dir: Option<SplitDirection>,
    /// Rendered base name (already interpolated with the first prompt's
    /// context). Only meaningful when `dir.is_some()`.
    base_name: String,
    cap: Option<u32>,
    tab_ids: Vec<String>,
    current_tab_id: Option<String>,
    current_panes: Vec<(String, Option<String>)>,
    tab_count: u32,
}

impl TabState {
    fn is_active(&self) -> bool {
        self.dir.is_some()
    }

    fn needs_new_tab(&self) -> bool {
        if !self.is_active() {
            return false;
        }
        if self.current_tab_id.is_none() {
            return true;
        }
        match self.cap {
            Some(cap) => u32::try_from(self.current_panes.len()).unwrap_or(u32::MAX) >= cap,
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

struct LaunchPlan {
    preset: PresetEntry,
    repo: RepoEntry,
    raw_prompts: Vec<RawPrompt>,
    vars: HashMap<String, String>,
    branch_names: Vec<String>,
    use_worktree: bool,
    tab_state: TabState,
    date_str: String,
    datetime_str: String,
}

async fn run(hub: &Hub, args: LaunchArgs) -> Result<(), LaunchFailure> {
    let plan = build_plan(hub, &args).await?;
    let preset_id = plan.preset.id.clone();
    let total = u32::try_from(plan.raw_prompts.len()).unwrap_or(u32::MAX);
    info!(
        preset_id = %preset_id,
        prompts = total,
        repo = %plan.repo.id,
        "preset launch starting"
    );
    orchestrate(hub, plan, total).await
}

async fn build_plan(hub: &Hub, args: &LaunchArgs) -> Result<LaunchPlan, LaunchFailure> {
    let presets = list(hub, &args.target)
        .await
        .map_err(|e| LaunchFailure::new(format!("loading presets: {e}")))?;
    let preset = presets
        .into_iter()
        .find(|p| p.id == args.preset_id)
        .ok_or_else(|| LaunchFailure::new(format!("preset not found: {}", args.preset_id)))?;
    let repo = find_repo(hub, &preset.source_repo_id)
        .map_err(|e| LaunchFailure::new(format!("preset's source repo missing: {e}")))?;
    let repo_path = PathBuf::from(&repo.path);
    let raw_prompts = resolve_prompts(&repo_path, &args.source).await?;
    if raw_prompts.is_empty() {
        return Err(LaunchFailure::new("no prompts to launch"));
    }
    let vars = resolve_variables(&preset.variables, &args.variable_values, &repo_path)?;
    let date_str = Utc::now().format("%Y-%m-%d").to_string();
    let datetime_str = Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let branch_names = render_branch_names(
        &preset,
        &raw_prompts,
        &vars,
        &repo,
        &date_str,
        &datetime_str,
    )?;
    check_branch_uniqueness(&branch_names)?;
    let use_worktree = args
        .use_worktree_override
        .or(preset.default_use_worktree)
        .unwrap_or(repo.default_use_worktree);
    let tab_state = build_tab_state(
        &preset,
        args,
        &raw_prompts,
        &vars,
        &repo,
        &date_str,
        &datetime_str,
    )?;
    Ok(LaunchPlan {
        preset,
        repo,
        raw_prompts,
        vars,
        branch_names,
        use_worktree,
        tab_state,
        date_str,
        datetime_str,
    })
}

fn render_branch_names(
    preset: &PresetEntry,
    raw_prompts: &[RawPrompt],
    vars: &HashMap<String, String>,
    repo: &RepoEntry,
    date_str: &str,
    datetime_str: &str,
) -> Result<Vec<String>, LaunchFailure> {
    raw_prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let ctx = TemplateContext {
                prompt: None,
                index_str: (i + 1).to_string(),
                slug: slugify(p.text.lines().next().unwrap_or("")),
                stem: p.stem.as_deref(),
                date: date_str,
                datetime: datetime_str,
                repo: &repo.name,
                vars,
            };
            render_template(&preset.branch_template, &ctx)
        })
        .collect()
}

fn check_branch_uniqueness(branch_names: &[String]) -> Result<(), LaunchFailure> {
    let mut seen: HashSet<&str> = HashSet::new();
    for name in branch_names {
        if !seen.insert(name.as_str()) {
            return Err(LaunchFailure::new(format!(
                "duplicate branch name '{name}'; ensure branch_template includes a per-prompt placeholder like {{index}} or {{stem}}"
            )));
        }
    }
    Ok(())
}

fn build_tab_state(
    preset: &PresetEntry,
    args: &LaunchArgs,
    raw_prompts: &[RawPrompt],
    vars: &HashMap<String, String>,
    repo: &RepoEntry,
    date_str: &str,
    datetime_str: &str,
) -> Result<TabState, LaunchFailure> {
    match &preset.tab_grouping {
        TabGroupingConfig::None => Ok(TabState {
            dir: None,
            base_name: String::new(),
            cap: None,
            tab_ids: Vec::new(),
            current_tab_id: None,
            current_panes: Vec::new(),
            tab_count: 0,
        }),
        TabGroupingConfig::NewTab {
            layout,
            max_panes_per_tab,
            tab_name_template,
        } => {
            let dir = match layout {
                TabLayout::TileHorizontal | TabLayout::BalancedHorizontal => {
                    SplitDirection::Horizontal
                }
                TabLayout::TileVertical | TabLayout::BalancedVertical => SplitDirection::Vertical,
            };
            let cap = args.max_panes_per_tab_override.or(*max_panes_per_tab);
            let name_template = tab_name_template.as_deref().unwrap_or(&preset.name);
            let first = raw_prompts
                .first()
                .ok_or_else(|| LaunchFailure::new("empty prompts (unreachable)"))?;
            let ctx = TemplateContext {
                prompt: None,
                index_str: "1".to_string(),
                slug: slugify(first.text.lines().next().unwrap_or("")),
                stem: first.stem.as_deref(),
                date: date_str,
                datetime: datetime_str,
                repo: &repo.name,
                vars,
            };
            let base_name = render_template(name_template, &ctx)?;
            Ok(TabState {
                dir: Some(dir),
                base_name,
                cap,
                tab_ids: Vec::new(),
                current_tab_id: None,
                current_panes: Vec::new(),
                tab_count: 0,
            })
        }
    }
}

async fn orchestrate(hub: &Hub, mut plan: LaunchPlan, total: u32) -> Result<(), LaunchFailure> {
    let mut session_ids: Vec<String> = Vec::new();
    let stagger = Duration::from_millis(u64::from(plan.preset.stagger_ms));
    let last_idx = plan.raw_prompts.len() - 1;

    for (i, raw) in plan.raw_prompts.iter().enumerate() {
        let snapshot_id = spawn_one(hub, &plan, i, raw, &session_ids).await?;
        session_ids.push(snapshot_id.clone());

        if plan.tab_state.is_active() {
            attach_to_tab(hub, &mut plan.tab_state, &snapshot_id, &session_ids)?;
        }

        let launched = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let _ = hub.preset_events.send(PresetEvent::Progress {
            preset_id: plan.preset.id.clone(),
            total,
            launched,
            current_tab_id: plan.tab_state.current_tab_id.clone(),
            tab_ids: plan.tab_state.tab_ids.clone(),
        });

        if i < last_idx {
            sleep(stagger).await;
        }
    }
    info!(preset_id = %plan.preset.id, count = total, "preset launch complete");
    Ok(())
}

async fn spawn_one(
    hub: &Hub,
    plan: &LaunchPlan,
    i: usize,
    raw: &RawPrompt,
    session_ids: &[String],
) -> Result<String, LaunchFailure> {
    let index = u32::try_from(i + 1).unwrap_or(u32::MAX);
    let ctx = TemplateContext {
        prompt: Some(&raw.text),
        index_str: index.to_string(),
        slug: slugify(raw.text.lines().next().unwrap_or("")),
        stem: raw.stem.as_deref(),
        date: &plan.date_str,
        datetime: &plan.datetime_str,
        repo: &plan.repo.name,
        vars: &plan.vars,
    };
    let body = render_template(&plan.preset.prompt_template, &ctx)
        .map_err(|e| e.with_partials(session_ids, &plan.tab_state.tab_ids))?;
    let footer = render_footer(&plan.preset.context_footer_lines, &plan.vars);
    let prompt_text = format!("{body}{footer}");
    let label = match &plan.preset.session_label_template {
        Some(tpl) => render_template(tpl, &ctx)
            .map_err(|e| e.with_partials(session_ids, &plan.tab_state.tab_ids))?,
        None => format!("{} {}", plan.preset.name, index),
    };
    let injector = build_injector(&plan.preset.injector, &prompt_text);
    let req = SpawnRequest {
        label: Some(label),
        target: SpawnTarget::Single {
            repo_id: plan.repo.id.clone(),
            branch_name: plan.branch_names[i].clone(),
            base_branch: None,
            use_worktree: plan.use_worktree,
        },
        mode: SessionMode::Interactive,
        initial_prompt: None,
        dangerously_skip_permissions: plan.preset.dangerously_skip_permissions,
        agent: plan.preset.agent,
        model: plan.preset.model.clone(),
        permission_mode: plan.preset.permission_mode,
        codex_sandbox: plan.preset.codex_sandbox,
        extra_env: Vec::new(),
        prompt_injector: Some(injector),
    };
    let snapshot = spawn_session(hub, req).await.map_err(|e| {
        LaunchFailure::new(format!("spawn failed at #{index}: {e}"))
            .with_partials(session_ids, &plan.tab_state.tab_ids)
    })?;
    Ok(snapshot.id)
}

fn attach_to_tab(
    hub: &Hub,
    state: &mut TabState,
    session_id: &str,
    session_ids: &[String],
) -> Result<(), LaunchFailure> {
    let Some(dir) = state.dir else {
        return Ok(());
    };
    if state.needs_new_tab() {
        open_new_tab(hub, state, session_ids)?;
    }
    let pane_id = Uuid::new_v4().to_string();
    state
        .current_panes
        .push((pane_id, Some(session_id.to_string())));
    let new_grid = tabs::build_balanced(&state.current_panes, dir).ok_or_else(|| {
        LaunchFailure::new("build_balanced returned None for non-empty input")
            .with_partials(session_ids, &state.tab_ids)
    })?;
    let tab_id = state.current_tab_id.clone().ok_or_else(|| {
        LaunchFailure::new("missing current tab id").with_partials(session_ids, &state.tab_ids)
    })?;
    let updated = hub
        .state
        .mutate(|s| {
            let tab = s
                .tabs
                .iter_mut()
                .find(|t| t.id == tab_id)
                .ok_or_else(|| anyhow!("tab vanished mid-launch: {tab_id}"))?;
            tab.grid = new_grid;
            Ok::<_, anyhow::Error>(tab.clone())
        })
        .map_err(|e| {
            LaunchFailure::new(format!("persisting tab update: {e}"))
                .with_partials(session_ids, &state.tab_ids)
        })?
        .map_err(|e| {
            LaunchFailure::new(format!("updating tab grid: {e}"))
                .with_partials(session_ids, &state.tab_ids)
        })?;
    let _ = hub.tab_events.send(TabEvent::Updated(updated));
    Ok(())
}

fn open_new_tab(
    hub: &Hub,
    state: &mut TabState,
    session_ids: &[String],
) -> Result<(), LaunchFailure> {
    state.tab_count += 1;
    let name = if state.tab_count == 1 {
        state.base_name.clone()
    } else {
        format!("{} {}", state.base_name, state.tab_count)
    };
    let tab = TabEntry {
        id: Uuid::new_v4().to_string(),
        name,
        grid: GridNode::Pane {
            pane_id: Uuid::new_v4().to_string(),
            session_id: None,
        },
        created_at: Utc::now(),
    };
    let tab_id = tab.id.clone();
    hub.state.mutate(|s| s.tabs.push(tab)).map_err(|e| {
        LaunchFailure::new(format!("persisting new tab: {e}"))
            .with_partials(session_ids, &state.tab_ids)
    })?;
    state.tab_ids.push(tab_id.clone());
    state.current_tab_id = Some(tab_id);
    state.current_panes.clear();
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect/panic; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_prompts_bullets() {
        let input = "- one\n- two\n- three\n";
        assert_eq!(parse_prompts(input), vec!["one", "two", "three"]);
    }

    #[test]
    fn parse_prompts_paragraphs() {
        let input = "first prompt\nstill first\n\nsecond prompt\n";
        assert_eq!(
            parse_prompts(input),
            vec!["first prompt\nstill first", "second prompt"]
        );
    }

    #[test]
    fn parse_prompts_mixed() {
        let input = "- bullet one\n\nparagraph\nstill paragraph\n\n- bullet two\n";
        assert_eq!(
            parse_prompts(input),
            vec!["bullet one", "paragraph\nstill paragraph", "bullet two"]
        );
    }

    #[test]
    fn parse_prompts_empty() {
        assert!(parse_prompts("").is_empty());
        assert!(parse_prompts("\n\n  \n").is_empty());
    }

    #[test]
    fn slugify_truncates_to_40_chars() {
        let long = "This is a fairly long bug report title that needs slugifying down";
        let slug = slugify(long);
        assert!(slug.len() <= 40);
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn slugify_collapses_punctuation() {
        assert_eq!(slugify("Hello, World!! 123"), "hello-world-123");
    }

    #[test]
    fn render_template_substitutes_known_placeholders() {
        let vars = HashMap::new();
        let ctx = TemplateContext {
            prompt: Some("ignored here"),
            index_str: "3".to_string(),
            slug: "auth-bug".to_string(),
            stem: Some("auth-bug"),
            date: "2026-05-10",
            datetime: "2026-05-10T12:00:00",
            repo: "yaat",
            vars: &vars,
        };
        let out = render_template("bug/{date}/{index}-{slug}", &ctx).expect("ok");
        assert_eq!(out, "bug/2026-05-10/3-auth-bug");
    }

    #[test]
    fn render_template_rejects_unresolved() {
        let vars = HashMap::new();
        let ctx = TemplateContext {
            prompt: None,
            index_str: "1".to_string(),
            slug: "x".to_string(),
            stem: None,
            date: "2026-05-10",
            datetime: "2026-05-10T00:00:00",
            repo: "r",
            vars: &vars,
        };
        let err = render_template("hi {bogus}", &ctx).expect_err("should fail");
        assert!(err.error.contains("unresolved"));
    }

    #[test]
    fn render_template_custom_var() {
        let mut vars = HashMap::new();
        vars.insert("bundle".to_string(), "C:/x/y.zip".to_string());
        let ctx = TemplateContext {
            prompt: None,
            index_str: "1".to_string(),
            slug: "x".to_string(),
            stem: None,
            date: "d",
            datetime: "dt",
            repo: "r",
            vars: &vars,
        };
        assert_eq!(
            render_template("bundle: {bundle}", &ctx).expect("ok"),
            "bundle: C:/x/y.zip"
        );
    }

    #[test]
    fn render_footer_omits_empty_lines() {
        let mut vars = HashMap::new();
        vars.insert("bundle".to_string(), String::new());
        vars.insert("client_log".to_string(), "C:/log.log".to_string());
        let lines = vec![
            FooterLine {
                label: "Bundle".to_string(),
                variable: "bundle".to_string(),
            },
            FooterLine {
                label: "Client log".to_string(),
                variable: "client_log".to_string(),
            },
        ];
        let out = render_footer(&lines, &vars);
        assert_eq!(out, "\n\nClient log: C:/log.log");
    }

    #[test]
    fn render_footer_empty_when_all_blank() {
        let vars = HashMap::new();
        let lines = vec![FooterLine {
            label: "x".to_string(),
            variable: "missing".to_string(),
        }];
        assert!(render_footer(&lines, &vars).is_empty());
    }

    #[test]
    fn expand_literal_path_substitutes_repo_root() {
        let path = expand_literal_path("{repo_root}/foo", Path::new("C:/repos/yaat"));
        assert_eq!(path, "C:/repos/yaat/foo");
    }

    #[test]
    fn expand_env_pattern_handles_missing_close() {
        // No close → bail and copy verbatim.
        let out = expand_env_pattern("${HOME prefix only", "${", "}");
        assert_eq!(out, "${HOME prefix only");
    }

    #[test]
    fn check_branch_uniqueness_rejects_dupes() {
        let names = vec!["bug/static".to_string(), "bug/static".to_string()];
        let err = check_branch_uniqueness(&names).expect_err("duplicates should fail");
        assert!(err.error.contains("duplicate"));
    }

    #[test]
    fn check_branch_uniqueness_accepts_distinct() {
        let names = vec!["bug/1".to_string(), "bug/2".to_string()];
        check_branch_uniqueness(&names).expect("ok");
    }

    #[test]
    fn build_injector_sandwiches_prompt_text() {
        let template = InjectorTemplate {
            startup_delay_ms: 100,
            pre_input: vec![InjectorStep::Write {
                data_b64: "G1ta".to_string(),
            }],
            post_input: vec![InjectorStep::Text {
                content: String::new(),
                newline: true,
            }],
        };
        let injector = build_injector(&template, "hello");
        assert_eq!(injector.steps.len(), 4);
        assert!(matches!(injector.steps[0], InjectorStep::Delay { ms: 100 }));
        assert!(matches!(injector.steps[1], InjectorStep::Write { .. }));
        match &injector.steps[2] {
            InjectorStep::Text { content, newline } => {
                assert_eq!(content, "hello");
                assert!(!*newline);
            }
            _ => panic!("expected Text step"),
        }
        assert!(matches!(
            injector.steps[3],
            InjectorStep::Text { newline: true, .. }
        ));
    }

    #[test]
    fn build_injector_skips_zero_startup_delay() {
        let template = InjectorTemplate {
            startup_delay_ms: 0,
            pre_input: vec![],
            post_input: vec![],
        };
        let injector = build_injector(&template, "x");
        assert_eq!(injector.steps.len(), 1);
        assert!(matches!(injector.steps[0], InjectorStep::Text { .. }));
    }

    #[test]
    fn resolve_variables_requires_non_optional() {
        let vars = vec![PresetVariable {
            name: "required".to_string(),
            label: "Required".to_string(),
            kind: PresetVariableKind::Text,
            prompt_at_launch: true,
            default: None,
            optional: false,
        }];
        let err = resolve_variables(&vars, &[], Path::new("/")).expect_err("required missing");
        assert!(err.error.contains("required"));
    }

    /// Parses the smoke-test preset that lives at the root of this repo
    /// (`.rustling-tulip/presets.json`). Failing this test means a checked-in
    /// preset file no longer round-trips through the protocol shape and the
    /// app would also fail to load it.
    #[test]
    fn repo_smoke_preset_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(".rustling-tulip")
            .join("presets.json");
        let bytes =
            std::fs::read(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        let entries: Vec<protocol::PresetEntry> = serde_json::from_slice(&bytes)
            .unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()));
        assert!(!entries.is_empty(), "expected at least one preset");
        let smoke = entries
            .iter()
            .find(|p| p.id == "smoke-inline")
            .expect("smoke-inline preset present");
        // Sanity: a single inline source, two-step injector framing,
        // tab grouping enabled with no cap.
        assert!(
            smoke
                .prompt_sources
                .iter()
                .any(|s| matches!(s, protocol::PresetPromptSource::Inline))
        );
        assert_eq!(smoke.stagger_ms, 3000);
        match &smoke.tab_grouping {
            protocol::TabGroupingConfig::NewTab {
                max_panes_per_tab, ..
            } => assert!(max_panes_per_tab.is_none()),
            protocol::TabGroupingConfig::None => panic!("expected new_tab grouping"),
        }
    }

    #[test]
    fn resolve_variables_uses_default_when_missing() {
        let vars = vec![PresetVariable {
            name: "x".to_string(),
            label: "X".to_string(),
            kind: PresetVariableKind::Text,
            prompt_at_launch: false,
            default: Some("fallback".to_string()),
            optional: false,
        }];
        let resolved = resolve_variables(&vars, &[], Path::new("/")).expect("ok");
        assert_eq!(resolved.get("x").map(String::as_str), Some("fallback"));
    }
}
