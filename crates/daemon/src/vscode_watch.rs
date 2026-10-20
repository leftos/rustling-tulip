//! Watches the `.code-workspace` file a workspace is linked to and re-syncs
//! the workspace's members when the file changes.
//!
//! A workspace records `watch_vscode_workspace` when it was created through
//! "Create & watch". One non-recursive watcher sits on each distinct linked
//! file's parent directory, with the same 750 ms debounce as the
//! source-control watchers, and the debouncer callback keeps only events that
//! name the file. On a quiet change the file is parsed against the registered
//! repos, a folder that matches no repo is registered with
//! [`crate::registry::add_repo`], and the workspace is upserted with the
//! file's folder order as its member order. A folder removed from the file
//! drops out of the members; its repo stays registered.
//!
//! The watched set is reconciled on every [`StateEvent::Workspaces`]
//! broadcast: a workspace that starts watching is armed, and one that stops
//! watching, is removed or is unlinked is dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;
use protocol::{VscodeWorkspaceFolder, WorkspaceEntry};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::server::StateEvent;
use crate::state::AppState;
use crate::{registry, vscode};

/// Debounce window for a linked workspace file. The same as the
/// source-control watchers: wide enough to coalesce an editor's save burst,
/// short enough to feel live.
const DEBOUNCE_MS: u64 = 750;

/// One armed watcher: dropping it stops the OS-level watch and, through the
/// callback's sender, the re-sync task.
struct WatchHandle {
    _debouncer: Box<dyn std::any::Any + Send + Sync>,
}

/// Public entry point. Arms the watchers the registry already asks for,
/// synchronously, so a change written right after start is seen; the
/// supervisor task then reconciles them on every workspace broadcast.
pub fn start(state: &Arc<AppState>, state_events: &broadcast::Sender<StateEvent>) {
    // A watcher whose backend fails drops itself through this channel; the
    // next workspace broadcast tries to arm it again.
    let (drop_tx, mut drop_rx) = mpsc::unbounded_channel::<String>();
    let mut handles = HashMap::<String, WatchHandle>::new();
    sync_watches(state, state_events, &drop_tx, &mut handles);
    let state = Arc::clone(state);
    let events = state_events.clone();
    tokio::spawn(async move {
        let mut rx = events.subscribe();
        loop {
            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Ok(StateEvent::Workspaces(_)) => {
                        sync_watches(&state, &events, &drop_tx, &mut handles);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "vscode_watch: state event stream lagged");
                        sync_watches(&state, &events, &drop_tx, &mut handles);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                dropped = drop_rx.recv() => {
                    if let Some(key) = dropped {
                        handles.remove(&key);
                        warn!(
                            file = %key,
                            "vscode_watch: watcher stopped; the next workspace broadcast re-arms it"
                        );
                    }
                }
            }
        }
        info!("vscode_watch: supervisor exiting (channel closed)");
    });
}

/// The keys and paths of every file some workspace asks to watch, one entry
/// per distinct file.
fn wanted_files(state: &AppState) -> HashMap<String, PathBuf> {
    state.with_persisted(|s| {
        s.workspaces
            .iter()
            .filter(|workspace| workspace.watch_vscode_workspace)
            .filter_map(|workspace| workspace.linked_vscode_workspace.as_deref())
            .map(|file| (path_key(file), PathBuf::from(file)))
            .collect()
    })
}

/// Reconcile the armed watchers with the registry: arm a file that is newly
/// watched, and drop a watcher whose file no workspace watches any more.
fn sync_watches(
    state: &Arc<AppState>,
    state_events: &broadcast::Sender<StateEvent>,
    drop_tx: &mpsc::UnboundedSender<String>,
    handles: &mut HashMap<String, WatchHandle>,
) {
    let wanted = wanted_files(state);
    handles.retain(|key, _| {
        let keep = wanted.contains_key(key);
        if !keep {
            debug!(file = key, "vscode_watch: stopping watcher");
        }
        keep
    });
    for (key, file) in wanted {
        if handles.contains_key(&key) {
            continue;
        }
        if let Some(handle) = arm(&key, &file, state, state_events, drop_tx) {
            handles.insert(key, handle);
        }
    }
}

/// Watch `file`'s parent directory non-recursively and re-sync the
/// workspaces linked to it whenever the file changes. `None` when the parent
/// directory does not exist or the watcher cannot be created.
fn arm(
    key: &str,
    file: &Path,
    state: &Arc<AppState>,
    state_events: &broadcast::Sender<StateEvent>,
    drop_tx: &mpsc::UnboundedSender<String>,
) -> Option<WatchHandle> {
    let dir = file.parent()?;
    if !dir.is_dir() {
        warn!(
            file = %file.display(),
            "vscode_watch: the linked workspace file's folder is missing; not watching"
        );
        return None;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let watched = file.to_path_buf();
    let drop_key = key.to_owned();
    let drop_tx = drop_tx.clone();
    let debouncer_result = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(events) => {
                let hit = events
                    .iter()
                    .any(|event| event.paths.iter().any(|path| paths_eq(path, &watched)));
                if hit {
                    let _ = tx.send(());
                }
            }
            Err(errs) => {
                for err in errs {
                    warn!(
                        file = %watched.display(),
                        error = %err,
                        "vscode_watch: debouncer error; the watcher drops and the next workspace broadcast re-arms it"
                    );
                }
                let _ = drop_tx.send(drop_key.clone());
            }
        },
    );
    let mut debouncer = match debouncer_result {
        Ok(debouncer) => debouncer,
        Err(err) => {
            warn!(
                file = %file.display(),
                ?err,
                "vscode_watch: failed to create its watcher"
            );
            return None;
        }
    };
    if let Err(err) = debouncer.watch(dir, RecursiveMode::NonRecursive) {
        warn!(
            file = %file.display(),
            ?err,
            "vscode_watch: failed to watch its folder"
        );
        return None;
    }
    let state = Arc::clone(state);
    let events = state_events.clone();
    let key = key.to_owned();
    let path = file.to_path_buf();
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            while rx.try_recv().is_ok() {}
            sync_file(&state, &events, &key, &path).await;
        }
    });
    info!(file = %file.display(), "vscode_watch: watching linked workspace file");
    Some(WatchHandle {
        _debouncer: Box::new(debouncer),
    })
}

/// Re-sync every watching workspace linked to `file`.
async fn sync_file(
    state: &AppState,
    events: &broadcast::Sender<StateEvent>,
    key: &str,
    file: &Path,
) {
    let linked: Vec<WorkspaceEntry> = state.with_persisted(|s| {
        s.workspaces
            .iter()
            .filter(|workspace| workspace.watch_vscode_workspace)
            .filter(|workspace| {
                workspace
                    .linked_vscode_workspace
                    .as_deref()
                    .is_some_and(|linked| path_key(linked) == key)
            })
            .cloned()
            .collect()
    });
    for workspace in linked {
        if let Err(err) = sync_workspace(state, events, &workspace, file).await {
            warn!(
                workspace = %workspace.name,
                file = %file.display(),
                ?err,
                "vscode_watch: re-sync failed; the workspace is left as it was"
            );
        }
    }
}

/// Parse `file` and apply its folders to `workspace`: a matched folder keeps
/// its repo, an unmatched one is registered, and the file's folder order is
/// the member order. Broadcasts only when the members or the registry
/// changed.
async fn sync_workspace(
    state: &AppState,
    events: &broadcast::Sender<StateEvent>,
    workspace: &WorkspaceEntry,
    file: &Path,
) -> anyhow::Result<()> {
    let (known, prior_paths) = state.with_persisted(|s| {
        let known: Vec<(String, String)> = s
            .repos
            .iter()
            .map(|repo| (repo.id.clone(), repo.path.clone()))
            .collect();
        let prior_paths: Vec<(String, String)> = workspace
            .member_repo_ids
            .iter()
            .filter_map(|id| {
                s.repos
                    .iter()
                    .find(|repo| &repo.id == id)
                    .map(|repo| (id.clone(), repo.path.clone()))
            })
            .collect();
        (known, prior_paths)
    });
    let suggestion = vscode::parse_workspace_file(file, &known)?;
    let plan = merge_members(
        &suggestion.folders,
        &workspace.member_repo_ids,
        &prior_paths,
    );
    let repos_before = state.with_persisted(|s| s.repos.len());
    let mut members = Vec::with_capacity(plan.len());
    for (folder, matched) in suggestion.folders.iter().zip(plan) {
        if let Some(id) = matched {
            members.push(id);
        } else {
            let entry = registry::add_repo(state, &folder.path, folder.name.clone()).await?;
            members.push(entry.id);
        }
    }
    let registry_changed = state.with_persisted(|s| s.repos.len()) != repos_before;
    if members == workspace.member_repo_ids && !registry_changed {
        debug!(
            workspace = %workspace.name,
            "vscode_watch: the file changed but the members did not"
        );
        return Ok(());
    }
    let changed = write_members(state, &workspace.id, &suggestion.source_path, &members)?;
    let (repos, workspaces) = state.with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
    if registry_changed {
        let _ = events.send(StateEvent::Repos(repos));
    }
    if changed {
        let _ = events.send(StateEvent::Workspaces(workspaces));
    }
    Ok(())
}

/// Write the re-synced members onto the workspace, re-validating it under the
/// write's own lock: the entry must still exist, still watch, and still link
/// `source_path`. Only `member_repo_ids` changes, so a removed workspace
/// never comes back and a concurrent rename or relink stands. Returns whether
/// it wrote.
fn write_members(
    state: &AppState,
    workspace_id: &str,
    source_path: &str,
    members: &[String],
) -> anyhow::Result<bool> {
    let changed = state.mutate(|s| {
        let Some(entry) = s
            .workspaces
            .iter_mut()
            .find(|entry| entry.id == workspace_id)
        else {
            return false;
        };
        let same_file = entry
            .linked_vscode_workspace
            .as_deref()
            .is_some_and(|linked| path_key(linked) == path_key(source_path));
        if !entry.watch_vscode_workspace || !same_file || entry.member_repo_ids == members {
            return false;
        }
        entry.member_repo_ids = members.to_vec();
        true
    })?;
    Ok(changed)
}

/// Each folder's member repo id, in file order: the repo the registry
/// matched it with, else the member `workspace` already held at that path
/// (compared like the parser's path match), else `None` for the caller to
/// register. A folder the file no longer names is absent from the result,
/// so removing it drops the member, and reordering the file reorders the
/// members.
pub(crate) fn merge_members(
    folders: &[VscodeWorkspaceFolder],
    prior_members: &[String],
    prior_paths: &[(String, String)],
) -> Vec<Option<String>> {
    folders
        .iter()
        .map(|folder| {
            if let Some(id) = &folder.matched_repo_id {
                return Some(id.clone());
            }
            prior_paths
                .iter()
                .find(|(id, path)| {
                    prior_members.iter().any(|member| member == id)
                        && path_key(path) == path_key(&folder.path)
                })
                .map(|(id, _)| id.clone())
        })
        .collect()
}

/// The comparison key for a path: separators and case do not matter on
/// Windows, where the two forms name the same folder.
fn path_key(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// Whether two paths name the same file, as [`path_key`] compares them.
fn paths_eq(a: &Path, b: &Path) -> bool {
    path_key(&a.to_string_lossy()) == path_key(&b.to_string_lossy())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::broadcast;
    use uuid::Uuid;

    fn folder(path: &str, matched: Option<&str>) -> VscodeWorkspaceFolder {
        VscodeWorkspaceFolder {
            path: path.to_owned(),
            name: None,
            matched_repo_id: matched.map(str::to_owned),
        }
    }

    #[test]
    fn the_file_order_is_the_member_order() {
        let prior = vec!["a".to_owned(), "b".to_owned()];
        let paths = vec![
            ("a".to_owned(), "C:/work/a".to_owned()),
            ("b".to_owned(), "C:/work/b".to_owned()),
        ];
        let merged = merge_members(
            &[folder("C:/work/b", None), folder("C:/work/a", None)],
            &prior,
            &paths,
        );
        assert_eq!(
            merged,
            [Some("b".to_owned()), Some("a".to_owned())],
            "a reordered file reorders the members"
        );
    }

    #[test]
    fn a_new_folder_asks_to_be_registered() {
        let prior = vec!["a".to_owned()];
        let paths = vec![("a".to_owned(), "C:/work/a".to_owned())];
        let merged = merge_members(
            &[folder("C:/work/a", None), folder("C:/work/c", None)],
            &prior,
            &paths,
        );
        assert_eq!(merged, [Some("a".to_owned()), None]);
    }

    #[test]
    fn a_folder_the_file_drops_loses_its_member() {
        let prior = vec!["a".to_owned(), "b".to_owned()];
        let paths = vec![
            ("a".to_owned(), "C:/work/a".to_owned()),
            ("b".to_owned(), "C:/work/b".to_owned()),
        ];
        let merged = merge_members(&[folder("C:/work/a", None)], &prior, &paths);
        assert_eq!(merged, [Some("a".to_owned())], "b drops out entirely");
    }

    #[test]
    fn a_registry_match_wins_and_paths_compare_loosely() {
        let prior = vec!["a".to_owned()];
        let paths = vec![("a".to_owned(), r"C:\WORK\A".to_owned())];
        assert_eq!(
            merge_members(&[folder("c:/work/a", None)], &prior, &paths),
            [Some("a".to_owned())],
            "case and separator differences still name the same folder"
        );
        assert_eq!(
            merge_members(&[folder("c:/work/a", Some("other"))], &prior, &paths),
            [Some("other".to_owned())],
            "the registry's match is authoritative"
        );
    }

    /// A scratch directory that removes itself on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("rt-vscode-watch-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dirs(root: &Path) -> crate::paths::Dirs {
        let config = root.join("config");
        std::fs::create_dir_all(&config).expect("create config dir");
        crate::paths::Dirs {
            config: config.clone(),
            state_file: config.join("state.json"),
            handshake_file: config.join("daemon.json"),
            lan_config_file: config.join("lan.json"),
            remote_config_file: config.join("remote.json"),
            lan_cert_file: config.join("lan-cert.pem"),
            lan_key_file: config.join("lan-key.pem"),
            sessions_dir: config.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    fn canonical(path: &Path) -> PathBuf {
        crate::paths::simplify_path(&std::fs::canonicalize(path).expect("canonicalize"))
    }

    fn workspace_file(folders: &[&str]) -> String {
        let entries: Vec<String> = folders
            .iter()
            .map(|folder| format!(r#"{{ "path": "{folder}" }}"#))
            .collect();
        format!(r#"{{ "folders": [{}] }}"#, entries.join(", "))
    }

    #[tokio::test]
    async fn a_changed_file_resyncs_the_workspaces_members() {
        let scratch = Scratch::new();
        let state =
            Arc::new(AppState::load_or_default(&dirs(scratch.path())).expect("load a fresh state"));
        let first = scratch.path().join("first");
        let second = scratch.path().join("second");
        std::fs::create_dir_all(&first).expect("create first folder");
        std::fs::create_dir_all(&second).expect("create second folder");
        let first = canonical(&first);
        let second = canonical(&second);
        let file = scratch.path().join("team.code-workspace");
        let file_text = file.to_string_lossy().into_owned();
        let first_repo = registry::add_repo(&state, &first.to_string_lossy(), None)
            .await
            .expect("register the first repo");
        let workspace = registry::upsert_workspace(
            &state,
            None,
            "team",
            vec![first_repo.id.clone()],
            Some(file_text),
            Some(true),
        )
        .expect("register the workspace");

        let (events, _) = broadcast::channel(32);
        let mut rx = events.subscribe();
        start(&state, &events);
        // The watcher is armed by `start` itself, so a write from here on is
        // an event it sees.
        std::fs::write(&file, workspace_file(&["first", "second"]))
            .expect("write the workspace file");

        // A change waits on the real debouncer; the deadline is a hang guard,
        // never a bound a passing run comes near.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let event = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("the watcher broadcasts the re-sync before its deadline");
            match event {
                Ok(StateEvent::Workspaces(workspaces)) => {
                    let synced = workspaces
                        .iter()
                        .find(|entry| entry.id == workspace.id)
                        .expect("the workspace is still registered");
                    if synced.member_repo_ids.len() == 2 {
                        break;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("state events closed"),
            }
        }

        let (members, repos) = state.with_persisted(|s| {
            let workspace = s
                .workspaces
                .iter()
                .find(|entry| entry.id == workspace.id)
                .expect("the workspace is registered");
            (workspace.member_repo_ids.clone(), s.repos.clone())
        });
        assert_eq!(members.first(), Some(&first_repo.id), "file order first");
        let second_repo = repos
            .iter()
            .find(|repo| repo.id != first_repo.id)
            .expect("the second folder registered");
        assert!(
            paths_eq(Path::new(&second_repo.path), &second),
            "the new folder registered from its canonical path: {}",
            second_repo.path
        );
        assert_eq!(members, [first_repo.id, second_repo.id.clone()]);
        let synced = state.with_persisted(|s| {
            s.workspaces
                .iter()
                .find(|entry| entry.id == workspace.id)
                .expect("the workspace is registered")
                .clone()
        });
        assert!(synced.watch_vscode_workspace, "the watch flag is kept");
    }

    fn folder_dir(scratch: &Scratch, name: &str) -> PathBuf {
        let path = scratch.path().join(name);
        std::fs::create_dir_all(&path).expect("create folder");
        canonical(&path)
    }

    fn linked_workspace(state: &AppState, file: &Path, name: &str) -> WorkspaceEntry {
        registry::upsert_workspace(
            state,
            None,
            name,
            Vec::new(),
            Some(file.to_string_lossy().into_owned()),
            Some(true),
        )
        .expect("register the linked workspace")
    }

    #[tokio::test]
    async fn a_removed_workspace_is_not_resurrected_by_a_resync() {
        let scratch = Scratch::new();
        let state =
            Arc::new(AppState::load_or_default(&dirs(scratch.path())).expect("load a fresh state"));
        let first = folder_dir(&scratch, "first");
        folder_dir(&scratch, "second");
        let file = scratch.path().join("team.code-workspace");
        std::fs::write(&file, workspace_file(&["first", "second"])).expect("write the file");
        let first_repo = registry::add_repo(&state, &first.to_string_lossy(), None)
            .await
            .expect("register the first repo");
        let workspace = linked_workspace(&state, &file, "team");
        state
            .mutate(|s| s.workspaces[0].member_repo_ids = vec![first_repo.id.clone()])
            .expect("seed one member");
        registry::remove_workspace(&state, &workspace.id).expect("remove the workspace");

        let (events, _) = broadcast::channel(16);
        sync_workspace(&state, &events, &workspace, &file)
            .await
            .expect("the re-sync itself succeeds");

        assert!(
            state.with_persisted(|s| s.workspaces.is_empty()),
            "a workspace removed while the file was being re-synced stays removed"
        );
    }

    #[tokio::test]
    async fn a_resync_keeps_the_current_name_and_link() {
        let scratch = Scratch::new();
        let state =
            Arc::new(AppState::load_or_default(&dirs(scratch.path())).expect("load a fresh state"));
        let first = folder_dir(&scratch, "first");
        folder_dir(&scratch, "second");
        let file = scratch.path().join("team.code-workspace");
        std::fs::write(&file, workspace_file(&["first", "second"])).expect("write the file");
        let first_repo = registry::add_repo(&state, &first.to_string_lossy(), None)
            .await
            .expect("register the first repo");
        let workspace = linked_workspace(&state, &file, "old name");
        state
            .mutate(|s| {
                s.workspaces[0].member_repo_ids = vec![first_repo.id.clone()];
                s.workspaces[0].name = "new name".to_owned();
            })
            .expect("rename the workspace");

        let (events, _) = broadcast::channel(16);
        // The snapshot carries the old name; the write must not put it back.
        sync_workspace(&state, &events, &workspace, &file)
            .await
            .expect("the re-sync succeeds");

        let synced = state.with_persisted(|s| s.workspaces[0].clone());
        assert_eq!(synced.name, "new name", "a concurrent rename stands");
        assert!(
            paths_eq(
                Path::new(synced.linked_vscode_workspace.as_deref().expect("linked")),
                &file
            ),
            "the link stands"
        );
        assert!(synced.watch_vscode_workspace, "and so does the watch flag");
        assert_eq!(
            synced.member_repo_ids.len(),
            2,
            "the members followed the file"
        );
    }

    #[tokio::test]
    async fn reconciliation_stops_and_rearms_watchers() {
        let scratch = Scratch::new();
        let state =
            Arc::new(AppState::load_or_default(&dirs(scratch.path())).expect("load a fresh state"));
        let file = scratch.path().join("team.code-workspace");
        let workspace = linked_workspace(&state, &file, "team");
        let key = path_key(&file.to_string_lossy());
        let (events, _) = broadcast::channel(16);
        let (drop_tx, _drop_rx) = mpsc::unbounded_channel();
        let mut handles = HashMap::new();

        sync_watches(&state, &events, &drop_tx, &mut handles);
        assert_eq!(handles.len(), 1, "a watched file is armed");
        handles.remove(&key);
        sync_watches(&state, &events, &drop_tx, &mut handles);
        assert_eq!(
            handles.len(),
            1,
            "the next reconciliation re-arms a dropped watcher"
        );

        state
            .mutate(|s| s.workspaces[0].watch_vscode_workspace = false)
            .expect("clear the watch flag");
        sync_watches(&state, &events, &drop_tx, &mut handles);
        assert!(handles.is_empty(), "clearing the flag stops watching");

        state
            .mutate(|s| s.workspaces[0].watch_vscode_workspace = true)
            .expect("restore the watch flag");
        sync_watches(&state, &events, &drop_tx, &mut handles);
        assert_eq!(handles.len(), 1, "and setting it arms again");

        registry::remove_workspace(&state, &workspace.id).expect("remove the workspace");
        sync_watches(&state, &events, &drop_tx, &mut handles);
        assert!(handles.is_empty(), "removing the workspace stops watching");
    }
}
