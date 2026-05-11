//! Preset loader, template renderer, and batch-spawn orchestrator.
//!
//! Reads `.rustling-tulip/presets.json` from a repo's working directory,
//! resolves prompt sources + variables, then issues N spawn requests with
//! `stagger_ms` pacing between them. Optionally groups the resulting
//! sessions into one or more tabs via the [`crate::tabs`] module.
//!
//! See `docs/plans/i-d-like-to-plan-zesty-scott.md` for the full design.

use protocol::{LaunchPresetSource, PresetEntry, PresetTarget};

use crate::server::{Hub, PresetEvent};

/// Return the union of presets visible from `target`. For a repo target,
/// reads `.rustling-tulip/presets.json` from the repo's working dir. For a
/// workspace target, reads each member repo's file and concatenates the
/// entries (each tagged with its `source_repo_id`).
///
/// Currently a stub — returns an empty list. Full implementation in a
/// follow-up patch.
#[expect(
    clippy::unused_async,
    reason = "stub; real impl performs blocking fs reads via tokio::fs"
)]
pub async fn list(_hub: &Hub, _target: &PresetTarget) -> anyhow::Result<Vec<PresetEntry>> {
    Ok(Vec::new())
}

/// Orchestrate a batch spawn for a named preset. Streams progress via the
/// hub's `preset_events` broadcast. Run as a detached tokio task; failures
/// are reported through the same channel rather than bubbled up.
///
/// Currently a stub — emits a single "not implemented" failure event so the
/// UI surfaces it instead of silently doing nothing.
#[expect(
    clippy::unused_async,
    reason = "stub; real impl awaits spawn_session and sleeps between sessions"
)]
pub async fn launch(
    hub: Hub,
    _target: PresetTarget,
    preset_id: String,
    _source: LaunchPresetSource,
    _variable_values: Vec<(String, String)>,
    _use_worktree_override: Option<bool>,
    _max_panes_per_tab_override: Option<u32>,
) {
    let _ = hub.preset_events.send(PresetEvent::Failed {
        preset_id,
        error: "preset launch not implemented yet".to_string(),
        partial_session_ids: Vec::new(),
        partial_tab_ids: Vec::new(),
    });
}
