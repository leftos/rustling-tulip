//! The file and folder fetch messages. `FetchFile` and `FetchFolder` start
//! through [`start_fetch`], so they share ids, `CancelFetch` and the file
//! queue; `FetchFolder` then walks its folder ([`crate::folder_pack`]).
//! `StatPath` resolves the same way and answers `PathStat`. All their disk
//! work, the root's `canonicalize` included, runs on a blocking thread, and
//! the git listing their check needs ([`listing_for`]) runs in a task of its
//! own, never on the connection's reader. All are served on the same
//! connections.
//!
//! The boundary is folder against repo, decided on each request
//! ([`target_root`]): a `worktree_path` must be the registered path, a folder
//! git names for that repo, or under the worktrees root. Git messages keep
//! their narrower rule ([`repo_target_or_err`]). `ListSessionFolders` only
//! tells a client where to look: it decides nothing.
//!
//! A folder that passes its walk is answered with `FolderFetchStarted`, then
//! the folder packed ([`folder_pack::pack`]) as `FileChunk` frames, then
//! `FileFetchDone`, every one of them on the connection's file queue.

use super::Hub;
use super::connection::{ConnCtx, start_fetch};
use super::git::repo_target_or_err;
use crate::file_fetch::{self, CanonicalRoot};
use crate::folder_pack::{self, Limits};
use crate::paths::{normalize_path_key, simplify_path};
use crate::sync::lock;
use crate::{git, history};
use anyhow::Context as _;
use futures::future::join_all;
use protocol::{ClientMessage, DaemonMessage, RepoEntry, SessionFolder, SessionMember};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// A `FetchFile`, `FetchFolder` or `StatPath` request's target.
struct Request {
    repo_id: String,
    worktree_path: Option<String>,
    path: String,
    /// What git said about `worktree_path`, read for this request alone.
    listing: Listing,
}

/// What `git worktree list` said about a request's `worktree_path`.
enum Listing {
    /// Git was not asked: no `worktree_path`, or one that needs no listing.
    NotAsked,
    /// Git names it, spelled this way.
    Named(PathBuf),
    /// Git does not name it.
    NotNamed,
    /// Git could not be read, for this reason.
    Failed(String),
}

/// The folder answer to `ListSessionFolders`: the folders, and why any did
/// not read.
type FolderAnswer = (Vec<SessionFolder>, Option<String>);

impl Request {
    fn new(repo_id: String, worktree_path: Option<String>, path: String) -> Self {
        Self {
            repo_id,
            worktree_path,
            path,
            listing: Listing::NotAsked,
        }
    }

    /// This request with git's word on its `worktree_path`. Runs git, so never
    /// on the connection's reader.
    async fn listed(mut self, hub: &Hub) -> Self {
        self.listing = listing_for(hub, &self.repo_id, self.worktree_path.as_deref()).await;
        self
    }
}

/// `path`'s comparison key.
fn path_key(path: &Path) -> String {
    normalize_path_key(&path.to_string_lossy())
}

/// Whether the folder with key `folder` is, or holds, the path with key `key`.
/// By text alone.
fn holds(folder: &str, key: &str) -> bool {
    key.strip_prefix(folder)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `key` names the worktrees root or a folder below it, by text alone.
/// A `..` below the root is not below it, and is never resolved to find out.
fn in_worktrees_root(hub: &Hub, key: &str) -> bool {
    let root = path_key(&hub.state.worktrees_dir());
    holds(&root, key) && !key[root.len()..].split('/').any(|part| part == "..")
}

fn registered_repo(hub: &Hub, repo_id: &str) -> Option<RepoEntry> {
    hub.state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
}

/// Ask git about `worktree_path`, unless it needs no asking: nothing to ask
/// about, the registered path, or in the worktrees root. Client text goes no
/// further than a comparison of keys. A plain folder has no worktrees, and
/// git run there would answer for any repo enclosing it. An unknown repo is
/// left for [`target_root`] to refuse.
async fn listing_for(hub: &Hub, repo_id: &str, worktree_path: Option<&str>) -> Listing {
    let Some(worktree) = worktree_path else {
        return Listing::NotAsked;
    };
    let Some(repo) = registered_repo(hub, repo_id) else {
        return Listing::NotAsked;
    };
    let key = normalize_path_key(worktree);
    if key == normalize_path_key(&repo.path) || in_worktrees_root(hub, &key) {
        return Listing::NotAsked;
    }
    if !repo.is_git {
        return Listing::NotNamed;
    }
    match git::worktree_folders(Path::new(&repo.path)).await {
        Ok(folders) => folders
            .into_iter()
            .find(|folder| path_key(folder) == key)
            .map_or(Listing::NotNamed, Listing::Named),
        Err(err) => Listing::Failed(format!("{err:#}")),
    }
}

/// Handle the folder messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::FetchFolder {
            id,
            repo_id,
            worktree_path,
            path,
        } => {
            let request = Request::new(repo_id, worktree_path, path);
            let hub = hub.clone();
            start_fetch(ctx, id, Ok(request), move |id, request, tx, token| {
                tokio::spawn(folder_worker(hub, id, request, tx, token))
            })
            .await;
        }
        ClientMessage::FetchFile {
            id,
            repo_id,
            worktree_path,
            path,
        } => {
            let request = Request::new(repo_id, worktree_path, path);
            let hub = hub.clone();
            start_fetch(ctx, id, Ok(request), move |id, request, tx, token| {
                tokio::spawn(file_worker(hub, id, request, tx, token))
            })
            .await;
        }
        ClientMessage::ListSessionFolders { id, session_id } => {
            let hub = hub.clone();
            let out_tx = ctx.out_tx.clone();
            tokio::spawn(async move {
                let (folders, error) = session_folders(&hub, &session_id).await;
                // A closed outbox is a connection already ending.
                let _ = out_tx.send(DaemonMessage::SessionFolders { id, folders, error });
            });
        }
        ClientMessage::StatPath {
            id,
            repo_id,
            worktree_path,
            path,
        } => {
            let request = Request::new(repo_id, worktree_path, path);
            let hub = hub.clone();
            let out_tx = ctx.out_tx.clone();
            tokio::spawn(async move {
                let request = request.listed(&hub).await;
                let stat = tokio::task::spawn_blocking(move || stat_request(&hub, &request))
                    .await
                    .unwrap_or_else(|err| {
                        warn!(%id, error = %err, "folder: a StatPath failed");
                        folder_pack::PathStat::MISSING
                    });
                // A closed outbox is a connection already ending.
                let _ = out_tx.send(DaemonMessage::PathStat {
                    id,
                    kind: stat.kind,
                    size: stat.size,
                });
            });
        }
        other => debug!(?other, "folder: not a folder message"),
    }
}

/// The refusal of a `worktree_path` git does not name, or names but no longer
/// matches the folder there. One text, whether or not the folder exists.
fn not_a_worktree(hub: &Hub, repo_id: &str, worktree: &str) -> anyhow::Error {
    let repo = registered_repo(hub, repo_id).map_or_else(|| repo_id.to_owned(), |repo| repo.name);
    anyhow::anyhow!("worktree_path {worktree} is not a git worktree of {repo}")
}

/// The folder a `worktree_path` was vouched for as, in the daemon's spelling
/// or, under the worktrees root, the one the root's own rule keeps.
struct Vouched {
    root: PathBuf,
    /// Vouched for on git's word alone, so the folder itself still has to
    /// match it.
    by_git: bool,
}

/// Vouch for `worktree` by its text and what git said, touching no disk until
/// that text is vouched for: the registered path, a folder git names, or under
/// the worktrees root by today's rule. Anything else is refused.
fn vouch(
    hub: &Hub,
    request: &Request,
    worktree: &str,
    registered: &Path,
) -> anyhow::Result<Vouched> {
    let key = normalize_path_key(worktree);
    if key == path_key(registered) {
        let root = registered.to_path_buf();
        return Ok(Vouched {
            root,
            by_git: false,
        });
    }
    if let Listing::Named(folder) = &request.listing {
        let root = folder.clone();
        return Ok(Vouched { root, by_git: true });
    }
    if in_worktrees_root(hub, &key) {
        let root = repo_target_or_err(hub, &request.repo_id, Some(worktree))?;
        return Ok(Vouched {
            root,
            by_git: false,
        });
    }
    if let Listing::Failed(reason) = &request.listing {
        let repo = registered_repo(hub, &request.repo_id)
            .map_or_else(|| request.repo_id.clone(), |repo| repo.name);
        anyhow::bail!("couldn't read the git worktrees of {repo}: {reason}");
    }
    Err(not_a_worktree(hub, &request.repo_id, worktree))
}

/// The repo or worktree root `request` names. A `worktree_path` must be one
/// git working tree's root: the repo root itself, or a folder holding a
/// `.git` entry, so a fetch never spans several worktrees. Git checks no
/// back-link and does not see a junction, so a folder it names must also
/// resolve to that very name, which refuses a junction at or above the entry.
/// Decided afresh on each request. Disk work.
fn target_root(hub: &Hub, request: &Request) -> anyhow::Result<PathBuf> {
    let repo = repo_target_or_err(hub, &request.repo_id, None)?;
    let Some(worktree) = request.worktree_path.as_deref() else {
        return Ok(repo);
    };
    let Vouched { root, by_git } = vouch(hub, request, worktree, &repo)?;
    let canon_root = std::fs::canonicalize(&root)
        .with_context(|| format!("resolving worktree_path {worktree}"))?;
    let worktrees = hub.state.worktrees_dir();
    if std::fs::canonicalize(&worktrees).is_ok_and(|canon| canon == canon_root) {
        anyhow::bail!(
            "worktree_path {worktree} is the worktrees root {} itself, not a worktree",
            worktrees.display()
        );
    }
    if by_git && path_key(&simplify_path(&canon_root)) != path_key(&root) {
        return Err(not_a_worktree(hub, &request.repo_id, worktree));
    }
    let is_repo = !by_git && std::fs::canonicalize(&repo).is_ok_and(|canon| canon == canon_root);
    if !is_repo && std::fs::symlink_metadata(canon_root.join(".git")).is_err() {
        anyhow::bail!("worktree_path {worktree} is not a worktree: it holds no .git");
    }
    Ok(root)
}

/// The final message of a worker whose blocking half panicked.
fn worker_failed(id: String, err: &tokio::task::JoinError) -> DaemonMessage {
    warn!(%id, error = %err, "folder: a fetch's worker failed");
    let error = format!("the fetch stopped unexpectedly: {err}");
    DaemonMessage::FileFetchError { id, error }
}

/// A `FetchFile`'s worker: decide on the request, then stream the file. Hands
/// back the final message for [`start_fetch`] to send, a refusal included.
async fn file_worker(
    hub: Hub,
    id: String,
    request: Request,
    tx: mpsc::Sender<DaemonMessage>,
    token: CancellationToken,
) -> Option<DaemonMessage> {
    let request = request.listed(&hub).await;
    let repo_id = request.repo_id.clone();
    let worktree_path = request.worktree_path.clone();
    let rel = request.path.clone();
    let confined = tokio::task::spawn_blocking(move || {
        let root = target_root(&hub, &request)?;
        file_fetch::confine_path(&root, &request.path)
    })
    .await;
    match confined {
        Ok(Ok(path)) => file_fetch::stream_file(id, path, tx, token).await,
        Ok(Err(err)) => {
            info!(
                %id,
                %repo_id,
                worktree_path = worktree_path.as_deref().unwrap_or_default(),
                rel = %rel,
                error = %format!("{err:#}"),
                "file fetch failed"
            );
            let error = format!("{err:#}");
            Some(DaemonMessage::FileFetchError { id, error })
        }
        Err(err) => Some(worker_failed(id, &err)),
    }
}

/// A `FetchFolder`'s worker: read git's listing, then do the disk work on a
/// blocking thread ([`fetch_folder`]).
async fn folder_worker(
    hub: Hub,
    id: String,
    request: Request,
    tx: mpsc::Sender<DaemonMessage>,
    token: CancellationToken,
) -> Option<DaemonMessage> {
    let request = request.listed(&hub).await;
    let watched = id.clone();
    tokio::task::spawn_blocking(move || fetch_folder(&hub, id, &request, &tx, &token))
        .await
        .unwrap_or_else(|err| Some(worker_failed(watched, &err)))
}

/// The folders git names for `repo`, leaving out what could never be served
/// for holding no `.git`: a bare main, a submodule's git dir, a locked entry
/// whose folder is gone. Runs git, then a blocking thread for the disk.
async fn listed_folders(repo: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let named = git::worktree_folders(repo).await?;
    tokio::task::spawn_blocking(move || {
        named
            .into_iter()
            .filter(|folder| std::fs::symlink_metadata(folder.join(".git")).is_ok())
            .collect()
    })
    .await
    .context("checking the folders git named")
}

/// A [`FolderAnswer`] being gathered: duplicates by key dropped, the first
/// listing failure kept.
#[derive(Default)]
struct FolderList {
    folders: Vec<SessionFolder>,
    seen: HashSet<String>,
    error: Option<String>,
}

impl FolderList {
    fn push(&mut self, repo: &RepoEntry, path: &str) {
        if self.seen.insert(normalize_path_key(path)) {
            self.folders.push(SessionFolder {
                repo_id: repo.id.clone(),
                repo_name: repo.name.clone(),
                path: path.to_owned(),
            });
        }
    }

    fn push_listing(&mut self, repo: &RepoEntry, listing: anyhow::Result<Vec<PathBuf>>) {
        match listing {
            Ok(folders) => {
                for folder in folders {
                    self.push(repo, &folder.to_string_lossy());
                }
            }
            Err(err) => {
                self.error.get_or_insert_with(|| format!("{err:#}"));
            }
        }
    }
}

/// The folders of a session with `members`, in member order: each member's
/// worktree, its registered path, and its git worktrees. A plain folder repo
/// is its registered path alone, with no git run. A member whose repo is no
/// longer registered has nothing a fetch could name, so it lists nothing.
async fn member_folders(hub: &Hub, members: &[SessionMember]) -> FolderAnswer {
    let repos: Vec<Option<RepoEntry>> = members
        .iter()
        .map(|member| registered_repo(hub, &member.repo_id))
        .collect();
    let listings = join_all(repos.iter().map(|repo| async move {
        match repo {
            Some(repo) if repo.is_git => Some(listed_folders(Path::new(&repo.path)).await),
            _ => None,
        }
    }))
    .await;
    let mut list = FolderList::default();
    for ((member, repo), listing) in members.iter().zip(&repos).zip(listings) {
        let Some(repo) = repo else { continue };
        if repo.is_git && !member.worktree_path.is_empty() {
            list.push(repo, &member.worktree_path);
        }
        list.push(repo, &repo.path);
        if let Some(listing) = listing {
            list.push_listing(repo, listing);
        }
    }
    (list.folders, list.error)
}

/// The index of the repo holding the path with key `cwd` most deeply, by its
/// registered path or a folder git named.
fn deepest_holder(
    cwd: &str,
    repos: &[RepoEntry],
    listings: &[anyhow::Result<Vec<PathBuf>>],
) -> Option<usize> {
    repos
        .iter()
        .zip(listings)
        .enumerate()
        .filter_map(|(index, (repo, listing))| {
            let named = listing.iter().flatten().map(|folder| path_key(folder));
            std::iter::once(normalize_path_key(&repo.path))
                .chain(named)
                .filter(|folder| holds(folder, cwd))
                .map(|folder| folder.len())
                .max()
                .map(|depth| (depth, index))
        })
        .max_by_key(|&(depth, _)| depth)
        .map(|(_, index)| index)
}

/// The folders of a session with no members, a standalone shell: the
/// registered path and git worktrees of the repo holding `cwd`. A plain folder
/// repo is a holder like any other, with no git run: its registered path alone.
/// No `cwd`, or no repo holding it, lists nothing, and a failure of some other
/// repo's listing is not this answer's concern.
async fn shell_folders(hub: &Hub, cwd: Option<&str>) -> FolderAnswer {
    let Some(cwd) = cwd else {
        return (Vec::new(), None);
    };
    let repos: Vec<RepoEntry> = hub.state.with_persisted(|s| s.repos.clone());
    let mut listings = join_all(repos.iter().map(|repo| async move {
        if repo.is_git {
            listed_folders(Path::new(&repo.path)).await
        } else {
            Ok(Vec::new())
        }
    }))
    .await;
    let Some(index) = deepest_holder(&normalize_path_key(cwd), &repos, &listings) else {
        return (Vec::new(), None);
    };
    let repo = &repos[index];
    let mut list = FolderList::default();
    list.push(repo, &repo.path);
    list.push_listing(repo, listings.swap_remove(index));
    (list.folders, list.error)
}

/// The answer to `ListSessionFolders`: a hint for the client, since the check
/// at fetch time is the boundary.
async fn session_folders(hub: &Hub, session_id: &str) -> FolderAnswer {
    let Some(record) = hub.sessions.get(session_id) else {
        return (Vec::new(), Some(format!("unknown session {session_id}")));
    };
    let (members, cwd) = {
        let record = lock(&record);
        let cwd = record
            .current_cwd
            .clone()
            .filter(|cwd| !cwd.trim().is_empty())
            .or_else(|| history::primary_cwd(&record));
        (record.members.clone(), cwd)
    };
    if members.is_empty() {
        shell_folders(hub, cwd.as_deref()).await
    } else {
        member_folders(hub, &members).await
    }
}

/// The folder a `FetchFolder` request names, confined to its root. A
/// refused path, whatever the reason, gets one text naming only the
/// requested path, so the answer reveals nothing outside the root. Disk work.
fn resolve(hub: &Hub, request: &Request) -> anyhow::Result<CanonicalRoot> {
    let root = target_root(hub, request)?;
    file_fetch::confine_dir(&root, &request.path).map_err(|err| {
        debug!(path = %request.path, error = %format!("{err:#}"), "folder: a FetchFolder path was refused");
        anyhow::anyhow!("path \"{}\" is not a folder inside the repo", request.path)
    })
}

/// What a `StatPath` request's path names; anything refused reads as
/// missing. Disk work.
fn stat_request(hub: &Hub, request: &Request) -> folder_pack::PathStat {
    target_root(hub, request).map_or(folder_pack::PathStat::MISSING, |root| {
        folder_pack::stat(&root, &request.path)
    })
}

/// A `FetchFolder`'s worker, on a blocking thread: resolve and walk the
/// folder, send `FolderFetchStarted` and the packed folder on `tx` (the
/// connection's file queue), and hand back the final message, unsent, for
/// [`start_fetch`] to send once the id is free: `FileFetchDone`, or one
/// `FileFetchError` when the folder was refused or failed. `None` once the
/// fetch was cancelled or its connection closed.
fn fetch_folder(
    hub: &Hub,
    id: String,
    request: &Request,
    tx: &mpsc::Sender<DaemonMessage>,
    token: &CancellationToken,
) -> Option<DaemonMessage> {
    #[cfg(test)]
    let watched = id.clone();
    let last = walk_and_pack(hub, id, request, tx, token);
    #[cfg(test)]
    hooks::worker_ended(&watched);
    last
}

/// [`fetch_folder`]'s work.
fn walk_and_pack(
    hub: &Hub,
    id: String,
    request: &Request,
    tx: &mpsc::Sender<DaemonMessage>,
    token: &CancellationToken,
) -> Option<DaemonMessage> {
    #[cfg(test)]
    hooks::before_scan(&request.path);
    let walked = resolve(hub, request).and_then(|dir| {
        let scan = folder_pack::scan(&dir, Limits::FETCH, token)?;
        Ok((dir, scan))
    });
    let last = match walked {
        Ok((dir, scan)) => {
            debug!(
                %id,
                folder = %dir.path().display(),
                files = scan.files.len(),
                bytes = scan.bytes,
                skipped = ?scan.skipped,
                unlisted = scan.unlisted,
                "folder: the walk passed; packing"
            );
            let started = DaemonMessage::FolderFetchStarted {
                id: id.clone(),
                resolved_path: dir.path().display().to_string(),
                files: u64::try_from(scan.files.len()).unwrap_or(u64::MAX),
                bytes: scan.bytes,
            };
            if !file_fetch::send_unless_cancelled_blocking(tx, token, started) {
                return None;
            }
            folder_pack::pack(&id, &scan, &dir, tx, token)
        }
        Err(err) => {
            let error = format!("{err:#}");
            Some(DaemonMessage::FileFetchError { id, error })
        }
    };
    if token.is_cancelled() { None } else { last }
}

/// A worker that panics, and watching workers end, from a test.
#[cfg(test)]
pub(super) mod hooks {
    use super::super::test_support::HANG_GUARD;
    use crate::sync::lock;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::watch;

    /// The `FetchFolder` path whose worker panics.
    pub(super) const PANIC_PATH: &str = "panic-in-the-worker";

    /// Panic when the request names [`PANIC_PATH`].
    pub(super) fn before_scan(path: &str) {
        assert_ne!(
            path, PANIC_PATH,
            "the test hook panicked the folder fetch's worker"
        );
    }

    static WATCHES: Mutex<Vec<(u64, String, watch::Sender<usize>)>> = Mutex::new(Vec::new());

    /// Counts the `FetchFolder` workers of one id that returned, until
    /// dropped.
    pub(super) struct WorkerWatch {
        key: u64,
        ended: watch::Receiver<usize>,
    }

    impl WorkerWatch {
        /// Count the workers of `id`.
        pub(super) fn new(id: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let key = NEXT.fetch_add(1, Ordering::Relaxed);
            let (tx, ended) = watch::channel(0);
            lock(&WATCHES).push((key, id.to_owned(), tx));
            Self { key, ended }
        }

        /// Resolves once a worker of the id returned; fails as hung when
        /// none does.
        pub(super) async fn ended(&mut self) {
            let waited = tokio::time::timeout(HANG_GUARD, self.ended.wait_for(|ended| *ended >= 1))
                .await
                .map(|ended| ended.is_ok());
            assert_eq!(
                waited,
                Ok(true),
                "hung: the folder fetch's worker never returned"
            );
        }
    }

    impl Drop for WorkerWatch {
        fn drop(&mut self) {
            lock(&WATCHES).retain(|(key, _, _)| *key != self.key);
        }
    }

    /// A worker of `id` returned.
    pub(super) fn worker_ended(id: &str) {
        for (_, watched, tx) in lock(&WATCHES).iter() {
            if watched == id {
                tx.send_modify(|ended| *ended += 1);
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::super::connection::AuthKind;
    use super::super::connection::hooks::RegistryWatch;
    use super::super::test_support::{
        HANG_GUARD, MemClient, ScratchDir, hello, idle_record, mem_session, repo_entry,
        repo_entry_plain, test_hub, test_peer,
    };
    use super::*;
    use crate::file_fetch::test_support::TestDir;
    use crate::file_tail::hooks::{make_dir_link, make_junction};
    use crate::git_inspect::test_support::{commit_file, git, init_at};
    use protocol::PathKind;

    /// A hub with repo `r1` registered at `<scratch>/repo`, holding `a.txt`
    /// (3 bytes), `big.bin` (spanning many chunks) and `sub/b.txt`.
    fn repo_hub(tag: &str) -> (Hub, ScratchDir, PathBuf) {
        let (hub, scratch) = test_hub(tag);
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(repo.join("sub")).expect("create repo");
        std::fs::write(repo.join("a.txt"), "abc").expect("write a.txt");
        std::fs::write(repo.join("sub").join("b.txt"), "b").expect("write b.txt");
        std::fs::write(
            repo.join("big.bin"),
            vec![1u8; crate::file_fetch::CHUNK_SIZE * 12],
        )
        .expect("write big.bin");
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", &repo)))
            .expect("register repo");
        (hub, scratch, repo)
    }

    fn fetch_folder(id: &str, path: &str) -> ClientMessage {
        ClientMessage::FetchFolder {
            id: id.to_owned(),
            repo_id: "r1".to_owned(),
            worktree_path: None,
            path: path.to_owned(),
        }
    }

    fn fetch_file(id: &str, path: &str) -> ClientMessage {
        ClientMessage::FetchFile {
            id: id.to_owned(),
            repo_id: "r1".to_owned(),
            worktree_path: None,
            path: path.to_owned(),
        }
    }

    fn stat_path(id: &str, path: &str) -> ClientMessage {
        ClientMessage::StatPath {
            id: id.to_owned(),
            repo_id: "r1".to_owned(),
            worktree_path: None,
            path: path.to_owned(),
        }
    }

    /// The transfer or stat id `msg` carries.
    fn id_of(msg: &DaemonMessage) -> Option<&str> {
        match msg {
            DaemonMessage::FileFetchStarted { id, .. }
            | DaemonMessage::FolderFetchStarted { id, .. }
            | DaemonMessage::FileChunk { id, .. }
            | DaemonMessage::FileFetchDone { id }
            | DaemonMessage::FileFetchError { id, .. }
            | DaemonMessage::PathStat { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Read `client`, answering pings, until `done` matches: every message
    /// read, the matching one last.
    async fn read_until(
        client: &mut MemClient,
        done: impl Fn(&DaemonMessage) -> bool,
    ) -> Vec<DaemonMessage> {
        let mut seen = Vec::new();
        loop {
            let msg = tokio::time::timeout(HANG_GUARD, client.next(true))
                .await
                .expect("hung: the awaited message never came")
                .expect("the connection stayed open");
            let last = done(&msg);
            seen.push(msg);
            if last {
                return seen;
            }
        }
    }

    /// Read until the transfer `id` ends (`FileFetchDone` or
    /// `FileFetchError`): its messages, the last one last.
    async fn until_end(client: &mut MemClient, id: &str) -> Vec<DaemonMessage> {
        read_until(client, |msg| {
            id_of(msg) == Some(id)
                && matches!(
                    msg,
                    DaemonMessage::FileFetchDone { .. } | DaemonMessage::FileFetchError { .. }
                )
        })
        .await
        .into_iter()
        .filter(|msg| id_of(msg) == Some(id))
        .collect()
    }

    /// `seen`, one folder fetch's messages, held a `FolderFetchStarted` and
    /// ended with `FileFetchDone`.
    fn assert_folder_done(seen: &[DaemonMessage]) {
        assert!(
            seen.iter()
                .any(|msg| matches!(msg, DaemonMessage::FolderFetchStarted { .. })),
            "{seen:?}"
        );
        assert!(
            matches!(seen.last(), Some(DaemonMessage::FileFetchDone { .. })),
            "{seen:?}"
        );
    }

    fn error_text(msg: &DaemonMessage) -> &str {
        match msg {
            DaemonMessage::FileFetchError { error, .. } => error,
            other => unreachable!("expected FileFetchError, got {other:?}"),
        }
    }

    /// A served loopback connection, its `Welcome` read.
    async fn local_session(hub: &Hub) -> (MemClient, tokio::task::JoinHandle<()>, Vec<String>) {
        let (mut client, session) = mem_session(hub, AuthKind::Local);
        client.send(&hello(&hub.auth_token, None));
        let seen = read_until(&mut client, |msg| {
            matches!(msg, DaemonMessage::Welcome { .. })
        })
        .await;
        let Some(DaemonMessage::Welcome { features, .. }) = seen.last() else {
            unreachable!("read until a Welcome");
        };
        let features = features.clone();
        (client, session, features)
    }

    #[tokio::test]
    async fn folder_fetch_and_stat_path_are_served_on_a_tailnet_connection() {
        let (hub, _scratch, _repo) = repo_hub("folder-tailnet");
        let (_, token) = crate::sync::lock(&hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token");
        let (mut client, _session) = mem_session(&hub, AuthKind::Tailnet(test_peer()));
        client.send(&hello(&token, None));
        read_until(&mut client, |msg| {
            matches!(msg, DaemonMessage::Welcome { .. })
        })
        .await;

        client.send(&fetch_folder("f1", ""));
        let f1 = until_end(&mut client, "f1").await;
        assert!(
            matches!(
                &f1[0],
                DaemonMessage::FolderFetchStarted { files: 3, bytes, .. }
                    if *bytes == (3 + 1 + crate::file_fetch::CHUNK_SIZE * 12) as u64
            ),
            "{f1:?}"
        );
        assert_folder_done(&f1);

        client.send(&fetch_folder("f2", "a.txt"));
        let f2 = until_end(&mut client, "f2").await;
        assert_eq!(f2.len(), 1, "one error and no Started: {f2:?}");
        assert!(error_text(&f2[0]).contains("is not a folder"), "{f2:?}");

        client.send(&stat_path("s1", "a.txt"));
        client.send(&stat_path("s2", "sub"));
        client.send(&stat_path("s3", "nope"));
        // Each stat is answered from a task of its own, in any order.
        let mut stats = Vec::new();
        while stats.len() < 3 {
            let seen = read_until(&mut client, |msg| {
                matches!(msg, DaemonMessage::PathStat { .. })
            })
            .await;
            for msg in seen {
                assert!(
                    !matches!(msg, DaemonMessage::Error { .. }),
                    "nothing is refused on a tailnet connection: {msg:?}"
                );
                if let DaemonMessage::PathStat { id, kind, size } = msg {
                    stats.push((id, kind, size));
                }
            }
        }
        stats.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            stats,
            [
                ("s1".to_owned(), PathKind::File, 3),
                ("s2".to_owned(), PathKind::Dir, 0),
                ("s3".to_owned(), PathKind::Missing, 0),
            ]
        );
    }

    #[tokio::test]
    async fn a_folder_fetch_reusing_an_active_file_fetch_id_is_refused() {
        let (hub, _scratch, _repo) = repo_hub("folder-duplicate");
        let (mut client, _session, _) = local_session(&hub).await;
        let mut registry = RegistryWatch::new("x");
        // A stalled socket keeps the file fetch active: its queue fills and
        // its stream waits, so it cannot end before the socket resumes.
        client.stall();
        client.send(&fetch_file("x", "big.bin"));
        client.send(&fetch_folder("x", ""));
        // Both fetches reached the registry: one registered, one refused.
        registry.reached(2).await;
        client.resume();
        let x = until_end(&mut client, "x").await;
        let refusals: Vec<&str> = x
            .iter()
            .filter_map(|msg| match msg {
                DaemonMessage::FileFetchError { error, .. } => Some(error.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(refusals, ["duplicate fetch id"], "{x:?}");
        // The refused folder fetch left the file fetch running to its end.
        let rest = until_end(&mut client, "x").await;
        assert!(
            matches!(rest.last(), Some(DaemonMessage::FileFetchDone { .. })),
            "{rest:?}"
        );
        assert!(
            !x.iter()
                .chain(&rest)
                .any(|msg| matches!(msg, DaemonMessage::FolderFetchStarted { .. })),
            "{x:?} {rest:?}"
        );
    }

    #[tokio::test]
    async fn an_id_reused_after_cancel_starts_a_new_fetch() {
        let (hub, _scratch, _repo) = repo_hub("folder-reuse");
        let (mut client, _session, _) = local_session(&hub).await;
        let mut registry = RegistryWatch::new("y");
        client.stall();
        client.send(&fetch_file("y", "big.bin"));
        client.send(&ClientMessage::CancelFetch { id: "y".to_owned() });
        client.send(&fetch_folder("y", ""));
        // The folder fetch registered after the cancel freed the id.
        registry.reached(2).await;
        client.resume();
        let y = until_end(&mut client, "y").await;
        assert_folder_done(&y);
        assert!(
            !y.iter().any(|msg| matches!(
                msg,
                DaemonMessage::FileFetchError { error, .. } if error == "duplicate fetch id"
            )),
            "{y:?}"
        );
    }

    #[tokio::test]
    async fn a_worker_panic_ends_with_an_error_and_frees_the_id() {
        let (hub, _scratch, _repo) = repo_hub("folder-panic");
        let (mut client, _session, _) = local_session(&hub).await;
        client.send(&fetch_folder("p", hooks::PANIC_PATH));
        let p = until_end(&mut client, "p").await;
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(error_text(&p[0]).contains("stopped unexpectedly"), "{p:?}");
        client.send(&fetch_folder("p", ""));
        let again = until_end(&mut client, "p").await;
        assert_folder_done(&again);
    }

    #[tokio::test]
    async fn folder_fetch_feature_is_listed_in_welcome() {
        let (hub, _scratch, _repo) = repo_hub("folder-welcome");
        let (_client, _session, features) = local_session(&hub).await;
        assert!(
            features.iter().any(|f| f == protocol::FEATURE_FILE_TAIL),
            "{features:?}"
        );
        assert!(
            features.iter().any(|f| f == protocol::FEATURE_FOLDER_FETCH),
            "{features:?}"
        );
    }

    #[test]
    fn a_worktree_path_equal_to_the_worktrees_root_is_refused() {
        let (hub, _scratch, _repo) = repo_hub("folder-worktrees-root");
        let worktrees = hub.state.worktrees_dir();
        std::fs::create_dir_all(worktrees.join("wt1")).expect("create a worktree");
        std::fs::write(worktrees.join("wt1").join(".git"), "gitdir: elsewhere").expect("write");
        std::fs::write(worktrees.join("wt1").join("w.txt"), "w").expect("write");
        let request = |worktree: &std::path::Path, path: &str| Request {
            repo_id: "r1".to_owned(),
            worktree_path: Some(worktree.to_string_lossy().into_owned()),
            path: path.to_owned(),
            listing: Listing::NotAsked,
        };
        let err = resolve(&hub, &request(&worktrees, "")).expect_err("the worktrees root");
        assert!(
            format!("{err:#}").contains("is the worktrees root"),
            "{err:#}"
        );
        assert_eq!(
            stat_request(&hub, &request(&worktrees, "wt1")),
            folder_pack::PathStat::MISSING
        );
        let dir = resolve(&hub, &request(&worktrees.join("wt1"), "")).expect("a worktree");
        assert!(dir.path().join("w.txt").exists());
        assert_eq!(
            stat_request(&hub, &request(&worktrees.join("wt1"), "w.txt")),
            folder_pack::PathStat {
                kind: PathKind::File,
                size: 1
            }
        );
    }

    #[test]
    fn a_worktree_group_folder_is_refused() {
        let (hub, _scratch, _repo) = repo_hub("folder-worktree-group");
        let worktrees = hub.state.worktrees_dir();
        let group = crate::git::group_dir(&worktrees, "feat", "grp");
        for name in ["wt-a", "wt-b"] {
            std::fs::create_dir_all(group.join(name)).expect("create a worktree");
            std::fs::write(group.join(name).join(".git"), "gitdir: elsewhere").expect("write");
            std::fs::write(group.join(name).join("w.txt"), "w").expect("write");
        }
        let request = |worktree: &std::path::Path, path: &str| Request {
            repo_id: "r1".to_owned(),
            worktree_path: Some(worktree.to_string_lossy().into_owned()),
            path: path.to_owned(),
            listing: Listing::NotAsked,
        };
        for above in [&group, group.parent().expect("the group's parent")] {
            let err = resolve(&hub, &request(above, "")).expect_err("a folder above a worktree");
            assert!(format!("{err:#}").contains("is not a worktree"), "{err:#}");
            assert_eq!(
                stat_request(&hub, &request(above, "wt-a")),
                folder_pack::PathStat::MISSING
            );
        }
        let dir = resolve(&hub, &request(&group.join("wt-a"), "")).expect("one worktree");
        assert!(dir.path().join("w.txt").exists());
    }

    #[test]
    fn fetch_folder_through_an_outside_link_reveals_nothing() {
        let (hub, scratch, repo) = repo_hub("folder-outside-link");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");
        if !crate::file_tail::hooks::make_dir_link(&repo.join("link"), &outside) {
            return;
        }
        let outside_text = outside.to_string_lossy().into_owned();
        for rel in [
            "link",
            "link/secret.txt",
            "link/nope",
            "nope",
            "a.txt",
            "../outside",
        ] {
            let request = Request {
                repo_id: "r1".to_owned(),
                worktree_path: None,
                path: rel.to_owned(),
                listing: Listing::NotAsked,
            };
            let err = resolve(&hub, &request).expect_err("refused");
            let text = format!("{err:#}");
            assert_eq!(
                text,
                format!("path \"{rel}\" is not a folder inside the repo"),
                "{rel:?}"
            );
            assert!(!text.contains(&outside_text), "{text}");
        }
    }

    #[tokio::test]
    async fn an_id_reused_right_after_its_refusal_is_accepted() {
        let (hub, _scratch, _repo) = repo_hub("folder-reuse-after-end");
        let (mut client, _session, _) = local_session(&hub).await;
        // Repeated, so a cleanup that frees the id late is caught however
        // the tasks happen to interleave.
        for _ in 0..20 {
            client.send(&fetch_folder("r", "a.txt"));
            let refused = until_end(&mut client, "r").await;
            assert!(
                error_text(refused.last().expect("an end")).contains("is not a folder"),
                "{refused:?}"
            );
            client.send(&fetch_folder("r", ""));
            let again = until_end(&mut client, "r").await;
            assert_folder_done(&again);
        }
    }

    /// Add `noise.bin` to `repo`: incompressible and many chunks long, so a
    /// fetch of the repo cannot end while its socket is stalled.
    fn add_noise(repo: &std::path::Path) {
        std::fs::write(
            repo.join("noise.bin"),
            folder_pack::noise(crate::file_fetch::CHUNK_SIZE * 12, 7),
        )
        .expect("write noise.bin");
    }

    fn is_end(msg: &DaemonMessage) -> bool {
        matches!(
            msg,
            DaemonMessage::FileFetchDone { .. } | DaemonMessage::FileFetchError { .. }
        )
    }

    #[tokio::test]
    async fn cancel_stops_folder_pack_without_done() {
        let (hub, _scratch, repo) = repo_hub("folder-cancel");
        add_noise(&repo);
        let (mut client, _session, _) = local_session(&hub).await;
        let mut folder = RegistryWatch::new("c");
        let mut file = RegistryWatch::new("m");
        client.stall();
        client.send(&fetch_folder("c", ""));
        folder.reached(1).await;
        client.send(&ClientMessage::CancelFetch { id: "c".to_owned() });
        client.send(&fetch_file("m", "a.txt"));
        // "m" reached the registry, so the cancel before it was handled.
        file.reached(1).await;
        client.resume();
        let seen = read_until(&mut client, |msg| id_of(msg) == Some("m") && is_end(msg)).await;
        assert!(
            matches!(seen.last(), Some(DaemonMessage::FileFetchDone { .. })),
            "{seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|msg| id_of(msg) == Some("c") && is_end(msg)),
            "{seen:?}"
        );
    }

    #[test]
    fn cancel_during_the_scan_sends_nothing() {
        let (hub, _scratch, _repo) = repo_hub("folder-cancel-scan");
        let (tx, mut rx) = mpsc::channel(4);
        let token = CancellationToken::new();
        token.cancel();
        let request = Request {
            repo_id: "r1".to_owned(),
            worktree_path: None,
            path: String::new(),
            listing: Listing::NotAsked,
        };
        let last = super::fetch_folder(&hub, "s".to_owned(), &request, &tx, &token);
        assert!(last.is_none(), "{last:?}");
        drop(tx);
        assert!(rx.try_recv().is_err(), "nothing was sent");
    }

    #[tokio::test]
    async fn a_closed_connection_stops_its_folder_fetch() {
        let (hub, _scratch, repo) = repo_hub("folder-closed");
        add_noise(&repo);
        let (client, _session, _) = local_session(&hub).await;
        let mut worker = hooks::WorkerWatch::new("d");
        let mut registry = RegistryWatch::new("d");
        client.stall();
        client.send(&fetch_folder("d", ""));
        registry.reached(1).await;
        drop(client);
        worker.ended().await;
    }

    // ---- worktree_path: every folder git names for the repo ----

    /// The file every sibling worktree holds, relative to its root.
    const HTML: &str = ".tmp/m/i.html";

    /// A hub with repo `r1` a real git repo at `<base>/r`, and a worktree of
    /// it at `<base>/r.wt/s/r`, cut by hand as `git worktree add` would be.
    struct Sibling {
        hub: Hub,
        _scratch: ScratchDir,
        base: TestDir,
        repo: PathBuf,
        /// The worktree, in git's own spelling.
        worktree: String,
    }

    /// A repo at `<base>/<name>`, holding one commit.
    async fn real_repo(base: &Path, name: &str) -> PathBuf {
        let repo = base.join(name);
        std::fs::create_dir_all(&repo).expect("create repo");
        init_at(&repo).await;
        commit_file(&repo, "seed.txt", b"seed").await;
        repo
    }

    /// Add a worktree of `repo` at `path` on a new branch, and return it as
    /// git spells it.
    async fn add_worktree(repo: &Path, path: &Path, branch: &str) -> String {
        let text = path.to_string_lossy();
        git(repo, &["worktree", "add", "-b", branch, &text]).await;
        let tail = path.file_name().expect("a folder name").to_string_lossy();
        named_by_git(repo, &tail)
            .await
            .into_iter()
            .next_back()
            .expect("git names the worktree it added")
    }

    /// The entries of `git worktree list` whose name ends in `tail`, whatever
    /// git says of them: git's output, read apart from the code under test.
    async fn named_by_git(repo: &Path, tail: &str) -> Vec<String> {
        git(repo, &["worktree", "list", "--porcelain"])
            .await
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .filter(|name| name.ends_with(tail))
            .map(str::to_owned)
            .collect()
    }

    /// `dir` as git would print it: resolved where a temp dir may sit behind
    /// a link (`/tmp` is `/private/tmp` on macOS), and as given on Windows,
    /// whose canonical form carries a verbatim prefix git never prints.
    fn as_git_prints(dir: &Path) -> PathBuf {
        if cfg!(windows) {
            dir.to_path_buf()
        } else {
            std::fs::canonicalize(dir).expect("resolve the base")
        }
    }

    async fn sibling(tag: &str) -> Sibling {
        let (hub, scratch) = test_hub(tag);
        let base = TestDir::new(tag);
        let root = as_git_prints(base.path());
        let repo = real_repo(&root, "r").await;
        let worktree = add_worktree(&repo, &root.join("r.wt/s/r"), "s").await;
        write_html(Path::new(&worktree));
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", &repo)))
            .expect("register repo");
        Sibling {
            hub,
            _scratch: scratch,
            base,
            repo,
            worktree,
        }
    }

    fn write_html(root: &Path) {
        let file = root.join(HTML);
        std::fs::create_dir_all(file.parent().expect("a parent")).expect("create folders");
        std::fs::write(file, "<html>").expect("write html");
    }

    fn at(worktree: &str, path: &str, listing: Listing) -> Request {
        Request {
            repo_id: "r1".to_owned(),
            worktree_path: Some(worktree.to_owned()),
            path: path.to_owned(),
            listing,
        }
    }

    /// What the daemon decides for `worktree`, its listing read as a request's is.
    async fn decide(hub: &Hub, worktree: &str) -> anyhow::Result<PathBuf> {
        let listing = listing_for(hub, "r1", Some(worktree)).await;
        target_root(hub, &at(worktree, "", listing))
    }

    /// What the daemon says `path` under `worktree` is.
    async fn stat_at(hub: &Hub, worktree: &str, path: &str) -> folder_pack::PathStat {
        let listing = listing_for(hub, "r1", Some(worktree)).await;
        stat_request(hub, &at(worktree, path, listing))
    }

    /// `worktree` was refused with the one text of an unlisted folder.
    async fn assert_one_refusal(hub: &Hub, worktree: &str) {
        let err = decide(hub, worktree).await.expect_err(worktree);
        assert_eq!(
            format!("{err:#}"),
            format!("worktree_path {worktree} is not a git worktree of r1"),
        );
        assert_eq!(
            stat_at(hub, worktree, "").await,
            folder_pack::PathStat::MISSING
        );
    }

    /// `path` with Windows' own separator, which `mklink` needs: it reads a
    /// forward slash as a switch. As given elsewhere.
    fn native_separators(path: &Path) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(path.to_string_lossy().replace('/', "\\"))
        } else {
            path.to_path_buf()
        }
    }

    /// Link `link` to the folder `target`: a junction on Windows, a symlink
    /// elsewhere. A machine that cannot make one fails the test, never skips.
    fn link_folder(link: &Path, target: &Path) {
        let (link, target) = (native_separators(link), native_separators(target));
        assert!(
            make_junction(&link, &target) || make_dir_link(&link, &target),
            "could not link {} to {}",
            link.display(),
            target.display()
        );
    }

    /// Make `repo` one git cannot read: a gitfile naming nothing, so git
    /// fails here rather than walking up to an enclosing repo.
    fn break_git(repo: &Path) {
        std::fs::rename(repo.join(".git"), repo.join("git-moved")).expect("move .git");
        std::fs::write(repo.join(".git"), "gitdir: nowhere").expect("write gitfile");
    }

    fn fetch_file_at(id: &str, worktree: &str, path: &str) -> ClientMessage {
        ClientMessage::FetchFile {
            id: id.to_owned(),
            repo_id: "r1".to_owned(),
            worktree_path: Some(worktree.to_owned()),
            path: path.to_owned(),
        }
    }

    fn fetch_folder_at(id: &str, worktree: &str, path: &str) -> ClientMessage {
        ClientMessage::FetchFolder {
            id: id.to_owned(),
            repo_id: "r1".to_owned(),
            worktree_path: Some(worktree.to_owned()),
            path: path.to_owned(),
        }
    }

    fn stat_path_at(id: &str, worktree: &str, path: &str) -> ClientMessage {
        ClientMessage::StatPath {
            id: id.to_owned(),
            repo_id: "r1".to_owned(),
            worktree_path: Some(worktree.to_owned()),
            path: path.to_owned(),
        }
    }

    /// The kind and size the `StatPath` `id` was answered with.
    async fn answered_stat(client: &mut MemClient, id: &str) -> (PathKind, u64) {
        let seen = read_until(
            client,
            |msg| matches!(msg, DaemonMessage::PathStat { id: got, .. } if got == id),
        )
        .await;
        match seen.last() {
            Some(DaemonMessage::PathStat { kind, size, .. }) => (*kind, *size),
            other => unreachable!("read until a PathStat, got {other:?}"),
        }
    }

    /// `seen`, one file fetch's messages, started and ended with `FileFetchDone`.
    fn assert_file_done(seen: &[DaemonMessage]) {
        assert!(
            matches!(seen.first(), Some(DaemonMessage::FileFetchStarted { .. })),
            "{seen:?}"
        );
        assert!(
            matches!(seen.last(), Some(DaemonMessage::FileFetchDone { .. })),
            "{seen:?}"
        );
    }

    /// `StatPath`, `FetchFile` and `FetchFolder` are all served from `worktree`.
    async fn assert_all_served(client: &mut MemClient, worktree: &str) {
        client.send(&stat_path_at("s", worktree, HTML));
        assert_eq!(answered_stat(client, "s").await, (PathKind::File, 6));
        client.send(&fetch_file_at("f", worktree, HTML));
        assert_file_done(&until_end(client, "f").await);
        client.send(&fetch_folder_at("d", worktree, ""));
        assert_folder_done(&until_end(client, "d").await);
    }

    #[tokio::test]
    async fn a_sibling_worktree_git_lists_is_fetched() {
        let s = sibling("sibling-fetched").await;
        let (mut client, _session, features) = local_session(&s.hub).await;
        assert!(
            features
                .iter()
                .any(|f| f == protocol::FEATURE_SESSION_FOLDERS),
            "{features:?}"
        );
        assert_all_served(&mut client, &s.worktree).await;
    }

    #[tokio::test]
    async fn a_listed_worktree_is_served_on_a_tailnet_connection() {
        let s = sibling("sibling-tailnet").await;
        let (_, token) = crate::sync::lock(&s.hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token");
        let (mut client, _session) = mem_session(&s.hub, AuthKind::Tailnet(test_peer()));
        client.send(&hello(&token, None));
        read_until(&mut client, |msg| {
            matches!(msg, DaemonMessage::Welcome { .. })
        })
        .await;
        assert_all_served(&mut client, &s.worktree).await;
    }

    #[tokio::test]
    async fn a_listed_worktree_matches_in_any_windows_spelling() {
        let s = sibling("sibling-spelling").await;
        let mut spellings = vec![format!("{}/", s.worktree)];
        if cfg!(windows) {
            let backslashed = s.worktree.replace('/', "\\");
            spellings.push(format!("{backslashed}\\"));
            spellings.push(s.worktree.to_uppercase());
            spellings.push(backslashed.to_lowercase());
            spellings.push(backslashed);
        }
        for spelling in spellings {
            let root = decide(&s.hub, &spelling).await.expect(&spelling);
            assert_eq!(root, PathBuf::from(&s.worktree), "git's own spelling");
            assert_eq!(
                stat_at(&s.hub, &spelling, HTML).await.kind,
                PathKind::File,
                "{spelling}"
            );
        }
    }

    #[tokio::test]
    async fn a_respelled_worktree_path_is_refused() {
        let s = sibling("sibling-respelled").await;
        let wt = &s.worktree;
        let backslashed = wt.replace('/', "\\");
        let parent = wt.strip_suffix("/s/r").expect("the worktree's shape");
        let respelled = [
            format!(r"\\?\{backslashed}"),
            format!(r"\??\{backslashed}"),
            format!("{wt}."),
            format!("{wt} "),
            format!("{wt}/./"),
            format!("{parent}/s/x/../r"),
            format!("{parent}/s/../s/r"),
            format!(r"{parent}\s\x\..\r"),
            format!("{parent}/S~1/r"),
            wt.replace("r.wt", "R~1.WT"),
        ];
        for spelling in respelled {
            assert_one_refusal(&s.hub, &spelling).await;
        }
    }

    #[tokio::test]
    async fn an_unlisted_path_gets_one_refusal_whether_or_not_it_exists() {
        let s = sibling("sibling-unlisted").await;
        let existing = s.base.path().join("elsewhere");
        std::fs::create_dir_all(&existing).expect("create a folder");
        std::fs::write(existing.join("secret.txt"), "secret").expect("write");
        let missing = s.base.path().join("nothing-here");
        for worktree in [
            existing.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
            r"\\server\share\folder".to_owned(),
            String::new(),
        ] {
            assert_one_refusal(&s.hub, &worktree).await;
        }
        // The wire says the same, for a file that exists in the unlisted folder.
        let existing = existing.to_string_lossy().into_owned();
        let (mut client, _session, _) = local_session(&s.hub).await;
        client.send(&fetch_file_at("f", &existing, "secret.txt"));
        let f = until_end(&mut client, "f").await;
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(
            error_text(&f[0]),
            format!("worktree_path {existing} is not a git worktree of r1"),
        );
    }

    #[tokio::test]
    async fn a_gitfile_pointing_at_the_repo_does_not_list_a_folder() {
        let s = sibling("sibling-gitfile").await;
        let forged = s.base.path().join("forged");
        std::fs::create_dir_all(&forged).expect("create a folder");
        let admin = s.repo.join(".git").join("worktrees").join("r");
        std::fs::write(
            forged.join(".git"),
            format!("gitdir: {}", admin.to_string_lossy()),
        )
        .expect("write gitfile");
        assert_one_refusal(&s.hub, &forged.to_string_lossy()).await;
    }

    #[tokio::test]
    async fn a_listed_worktree_with_a_rewritten_gitfile_is_served() {
        let s = sibling("sibling-rewritten").await;
        std::fs::write(Path::new(&s.worktree).join(".git"), "gitdir: nowhere").expect("write");
        let root = decide(&s.hub, &s.worktree).await.expect("named by git");
        assert_eq!(root, PathBuf::from(&s.worktree));
        assert_eq!(
            stat_at(&s.hub, &s.worktree, HTML).await.kind,
            PathKind::File
        );
    }

    /// `s.repo` as a session's one member, in session `s1`.
    fn member_session(s: &Sibling) {
        let mut record = idle_record("s1");
        record.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: "main".to_owned(),
            worktree_path: s.repo.to_string_lossy().into_owned(),
        }];
        s.hub.sessions.insert(record);
    }

    /// The keys of `folders`.
    fn keys(folders: &[SessionFolder]) -> Vec<String> {
        folders
            .iter()
            .map(|folder| normalize_path_key(&folder.path))
            .collect()
    }

    #[tokio::test]
    async fn a_prunable_worktree_is_neither_listed_nor_served() {
        let s = sibling("sibling-prunable").await;
        std::fs::rename(&s.worktree, s.base.path().join("moved")).expect("move the worktree");
        assert_one_refusal(&s.hub, &s.worktree).await;
        member_session(&s);
        let (folders, error) = session_folders(&s.hub, "s1").await;
        assert_eq!(error, None);
        assert_eq!(
            keys(&folders),
            [normalize_path_key(&s.repo.to_string_lossy())]
        );
    }

    #[tokio::test]
    async fn a_locked_worktree_whose_folder_is_gone_is_refused() {
        let s = sibling("sibling-locked").await;
        git(&s.repo, &["worktree", "lock", &s.worktree]).await;
        std::fs::remove_dir_all(&s.worktree).expect("remove the worktree");
        assert_eq!(
            named_by_git(&s.repo, "/s/r").await,
            std::slice::from_ref(&s.worktree)
        );
        decide(&s.hub, &s.worktree)
            .await
            .expect_err("its folder is gone");
        assert_eq!(
            stat_at(&s.hub, &s.worktree, "").await,
            folder_pack::PathStat::MISSING
        );
        member_session(&s);
        let (folders, error) = session_folders(&s.hub, "s1").await;
        assert_eq!(error, None);
        assert_eq!(
            keys(&folders),
            [normalize_path_key(&s.repo.to_string_lossy())]
        );
    }

    #[tokio::test]
    async fn a_worktree_swapped_for_a_junction_is_refused() {
        let s = sibling("sibling-swapped").await;
        let moved = s.base.path().join("moved");
        std::fs::rename(&s.worktree, &moved).expect("move the worktree");
        link_folder(Path::new(&s.worktree), &moved);
        assert_eq!(
            named_by_git(&s.repo, "/s/r").await,
            std::slice::from_ref(&s.worktree)
        );
        assert_one_refusal(&s.hub, &s.worktree).await;
    }

    #[tokio::test]
    async fn a_redirected_parent_is_refused() {
        let s = sibling("sibling-parent").await;
        let parent = s.base.path().join("r.wt");
        let moved = s.base.path().join("moved.wt");
        std::fs::rename(&parent, &moved).expect("move the parent");
        link_folder(&parent, &moved);
        assert_eq!(
            named_by_git(&s.repo, "/s/r").await,
            std::slice::from_ref(&s.worktree)
        );
        assert_one_refusal(&s.hub, &s.worktree).await;
    }

    #[tokio::test]
    async fn an_outside_link_below_a_listed_worktree_is_refused() {
        let s = sibling("sibling-outside").await;
        let outside = s.base.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");
        link_folder(&Path::new(&s.worktree).join("out"), &outside);
        decide(&s.hub, &s.worktree)
            .await
            .expect("the root is listed");
        assert_eq!(
            stat_at(&s.hub, &s.worktree, "out/secret.txt").await,
            folder_pack::PathStat::MISSING
        );
        let (mut client, _session, _) = local_session(&s.hub).await;
        client.send(&fetch_file_at("f", &s.worktree, "out/secret.txt"));
        let f = until_end(&mut client, "f").await;
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(&f[0], DaemonMessage::FileFetchError { .. }),
            "{f:?}"
        );
        client.send(&fetch_folder_at("d", &s.worktree, "out"));
        let d = until_end(&mut client, "d").await;
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(error_text(&d[0]).contains("is not a folder"), "{d:?}");
    }

    #[tokio::test]
    async fn a_bare_main_is_refused_and_its_linked_worktree_served() {
        let (hub, _scratch) = test_hub("sibling-bare");
        let base = TestDir::new("sibling-bare");
        real_repo(base.path(), "r").await;
        git(base.path(), &["clone", "--bare", "r", "b.git"]).await;
        let bare = base.path().join("b.git");
        let registered = add_worktree(&bare, &base.path().join("b.wt/x"), "x").await;
        let linked = add_worktree(&bare, &base.path().join("b.wt/y"), "y").await;
        write_html(Path::new(&linked));
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", Path::new(&registered))))
            .expect("register repo");
        let main = named_by_git(Path::new(&registered), "b.git").await;
        assert_eq!(main.len(), 1, "git names the bare main: {main:?}");
        let err = decide(&hub, &main[0]).await.expect_err("a bare main");
        assert!(format!("{err:#}").contains("holds no .git"), "{err:#}");
        assert_eq!(
            decide(&hub, &linked).await.expect("linked"),
            PathBuf::from(&linked)
        );
        assert_eq!(stat_at(&hub, &linked, HTML).await.kind, PathKind::File);
    }

    #[tokio::test]
    async fn a_submodule_git_dir_entry_is_refused() {
        let (hub, _scratch) = test_hub("sibling-submodule");
        let base = TestDir::new("sibling-submodule");
        let lib = real_repo(base.path(), "lib").await;
        let outer = real_repo(base.path(), "outer").await;
        git(
            &outer,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                &lib.to_string_lossy(),
                "sub",
            ],
        )
        .await;
        let sub = outer.join("sub");
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", &sub)))
            .expect("register repo");
        let git_dir = named_by_git(&sub, "modules/sub").await;
        assert_eq!(git_dir.len(), 1, "git names the git dir: {git_dir:?}");
        let err = decide(&hub, &git_dir[0]).await.expect_err("a git dir");
        assert!(format!("{err:#}").contains("holds no .git"), "{err:#}");
        let registered = sub.to_string_lossy();
        assert_eq!(decide(&hub, &registered).await.expect("registered"), sub);
    }

    #[tokio::test]
    async fn a_worktree_removed_after_the_listing_is_refused() {
        let s = sibling("sibling-removed").await;
        let named = || Listing::Named(PathBuf::from(&s.worktree));
        target_root(&s.hub, &at(&s.worktree, "", named())).expect("it is there");
        std::fs::remove_dir_all(&s.worktree).expect("remove the worktree");
        target_root(&s.hub, &at(&s.worktree, "", named())).expect_err("it is gone");
        assert_eq!(
            stat_request(&s.hub, &at(&s.worktree, HTML, named())),
            folder_pack::PathStat::MISSING
        );
    }

    #[tokio::test]
    async fn fetch_file_refuses_the_worktrees_root() {
        let (hub, _scratch, _repo) = repo_hub("fetch-file-worktrees-root");
        let worktrees = hub.state.worktrees_dir();
        std::fs::create_dir_all(&worktrees).expect("create the worktrees root");
        std::fs::write(worktrees.join("x.txt"), "x").expect("write");
        let (mut client, _session, _) = local_session(&hub).await;
        client.send(&fetch_file_at("f", &worktrees.to_string_lossy(), "x.txt"));
        let f = until_end(&mut client, "f").await;
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(error_text(&f[0]).contains("is the worktrees root"), "{f:?}");
    }

    #[tokio::test]
    async fn a_worktrees_root_git_lists_is_refused() {
        let s = sibling("sibling-root-listed").await;
        let worktrees = s.hub.state.worktrees_dir();
        let listed = add_worktree(&s.repo, &worktrees, "at-the-root").await;
        let err = decide(&s.hub, &listed)
            .await
            .expect_err("the worktrees root");
        assert!(
            format!("{err:#}").contains("is the worktrees root"),
            "{err:#}"
        );
        let named = Listing::Named(PathBuf::from(&listed));
        let err = target_root(&s.hub, &at(&listed, "", named)).expect_err("the worktrees root");
        assert!(
            format!("{err:#}").contains("is the worktrees root"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_member_under_the_worktrees_root_needs_no_listing() {
        let s = sibling("sibling-under-root").await;
        let member = s.hub.state.worktrees_dir().join("wt1");
        std::fs::create_dir_all(&member).expect("create a worktree");
        std::fs::write(member.join(".git"), "gitdir: elsewhere").expect("write");
        write_html(&member);
        break_git(&s.repo);
        let member = member.to_string_lossy().into_owned();
        assert!(matches!(
            listing_for(&s.hub, "r1", Some(&member)).await,
            Listing::NotAsked
        ));
        assert_eq!(
            decide(&s.hub, &member).await.expect("served"),
            PathBuf::from(&member)
        );
        assert_eq!(stat_at(&s.hub, &member, HTML).await.kind, PathKind::File);
    }

    #[tokio::test]
    async fn a_failed_listing_refuses_naming_git() {
        let s = sibling("sibling-broken-git").await;
        break_git(&s.repo);
        let err = decide(&s.hub, &s.worktree)
            .await
            .expect_err("git is broken");
        assert!(
            format!("{err:#}").starts_with("couldn't read the git worktrees of r1: "),
            "{err:#}"
        );
        assert_eq!(
            stat_at(&s.hub, &s.worktree, HTML).await,
            folder_pack::PathStat::MISSING
        );
    }

    #[tokio::test]
    async fn a_plain_folder_repo_is_served_by_equality() {
        let (hub, scratch) = test_hub("plain-equality");
        let plain = scratch.path().join("plain");
        std::fs::create_dir_all(&plain).expect("create folder");
        std::fs::write(plain.join("a.txt"), "abc").expect("write");
        hub.state
            .mutate(|s| s.repos.push(repo_entry_plain("r1", "r1", &plain)))
            .expect("register repo");
        let text = plain.to_string_lossy();
        assert_eq!(decide(&hub, &text).await.expect("served"), plain);
        assert_eq!(stat_at(&hub, &text, "a.txt").await.kind, PathKind::File);
        // A plain folder has no worktrees, and git is never asked for any: one
        // inside a repo must not reach that repo's other folders.
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create folder");
        assert_one_refusal(&hub, &outside.to_string_lossy()).await;
    }

    #[tokio::test]
    async fn git_messages_still_refuse_a_listed_sibling() {
        let s = sibling("sibling-git-messages").await;
        let (mut client, _session, _) = local_session(&s.hub).await;
        client.send(&ClientMessage::StageFiles {
            repo_id: "r1".to_owned(),
            paths: vec![HTML.to_owned()],
            worktree_path: Some(s.worktree.clone()),
        });
        let seen = read_until(&mut client, |msg| {
            matches!(msg, DaemonMessage::GitWriteError { .. })
        })
        .await;
        let Some(DaemonMessage::GitWriteError { error, .. }) = seen.last() else {
            unreachable!("read until a GitWriteError");
        };
        assert!(error.contains("is not under the worktrees root"), "{error}");
        client.send(&ClientMessage::RepoStatus {
            repo_id: "r1".to_owned(),
            worktree_path: Some(s.worktree.clone()),
            request_id: Some("q".to_owned()),
        });
        let seen = read_until(
            &mut client,
            |msg| matches!(msg, DaemonMessage::Error { request_id: Some(id), .. } if id == "q"),
        )
        .await;
        let Some(DaemonMessage::Error { message, .. }) = seen.last() else {
            unreachable!("read until an Error");
        };
        assert!(
            message.contains("is not under the worktrees root"),
            "{message}"
        );
    }

    /// The folders and error the wire answers `session_id` with.
    async fn asked_folders(
        client: &mut MemClient,
        session_id: &str,
    ) -> (Vec<SessionFolder>, Option<String>) {
        client.send(&ClientMessage::ListSessionFolders {
            id: "l".to_owned(),
            session_id: session_id.to_owned(),
        });
        let seen = read_until(client, |msg| {
            matches!(msg, DaemonMessage::SessionFolders { .. })
        })
        .await;
        match seen.into_iter().next_back() {
            Some(DaemonMessage::SessionFolders { id, folders, error }) => {
                assert_eq!(id, "l");
                (folders, error)
            }
            other => unreachable!("read until SessionFolders, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_folders_of_a_workspace_join_each_members_worktrees() {
        let (hub, _scratch) = test_hub("folders-workspace");
        let base = TestDir::new("folders-workspace");
        let root = as_git_prints(base.path());
        let mut members = Vec::new();
        let mut expected = Vec::new();
        for name in ["b-first", "a-second"] {
            let repo = real_repo(&root, name).await;
            let member = add_worktree(&repo, &root.join(format!("{name}.wt/m")), "m").await;
            let spare = add_worktree(&repo, &root.join(format!("{name}.wt/n")), "n").await;
            hub.state
                .mutate(|s| s.repos.push(repo_entry(name, name, &repo)))
                .expect("register repo");
            members.push(SessionMember {
                repo_id: name.to_owned(),
                repo_name: name.to_owned(),
                branch: "m".to_owned(),
                worktree_path: member.clone(),
            });
            expected.extend(
                [member, repo.to_string_lossy().into_owned(), spare]
                    .map(|path| (name.to_owned(), normalize_path_key(&path))),
            );
        }
        let mut record = idle_record("s1");
        record.members = members;
        hub.sessions.insert(record);
        let (mut client, _session, _) = local_session(&hub).await;
        let (folders, error) = asked_folders(&mut client, "s1").await;
        assert_eq!(error, None);
        let got: Vec<(String, String)> = folders
            .iter()
            .map(|folder| (folder.repo_id.clone(), normalize_path_key(&folder.path)))
            .collect();
        assert_eq!(got, expected);
        assert!(
            folders
                .iter()
                .all(|folder| folder.repo_id == folder.repo_name)
        );
    }

    #[tokio::test]
    async fn session_folders_of_a_shell_follow_its_cwd() {
        let (hub, _scratch) = test_hub("folders-shell");
        let base = TestDir::new("folders-shell");
        let root = as_git_prints(base.path());
        // An outer repo holding `app` and its worktree, registered first: the
        // deepest holder of the cwd wins.
        let outer = real_repo(&root, "outer").await;
        let app = real_repo(&outer, "app").await;
        let wt = add_worktree(&app, &outer.join("app.wt/x/app"), "x").await;
        // Plain folders, one alone and one inside `outer`. Git run in the
        // inner one would answer for `outer`, so its answer shows no git ran.
        let plain = root.join("notes");
        let nested = outer.join("notes");
        for folder in [&plain, &nested] {
            std::fs::create_dir_all(folder).expect("create a plain folder");
        }
        hub.state
            .mutate(|s| {
                s.repos.push(repo_entry("outer", "outer", &outer));
                s.repos.push(repo_entry("app", "app", &app));
                s.repos.push(repo_entry_plain("plain", "plain", &plain));
                s.repos.push(repo_entry_plain("nested", "nested", &nested));
            })
            .expect("register repos");
        let app_text = app.to_string_lossy().into_owned();
        let elsewhere = root.join("elsewhere");
        for (session, cwd) in [
            ("in-repo", format!("{app_text}/src")),
            ("in-sibling", format!("{wt}/src")),
            (
                "in-outer",
                outer.join("docs").to_string_lossy().into_owned(),
            ),
            ("in-plain", plain.join("sub").to_string_lossy().into_owned()),
            (
                "in-nested",
                nested.join("sub").to_string_lossy().into_owned(),
            ),
            ("elsewhere", elsewhere.to_string_lossy().into_owned()),
        ] {
            let mut record = idle_record(session);
            record.current_cwd = Some(cwd);
            hub.sessions.insert(record);
        }
        let no_cwd = idle_record("no-cwd");
        hub.sessions.insert(no_cwd);
        let app_keys = [normalize_path_key(&app_text), normalize_path_key(&wt)];
        for session in ["in-repo", "in-sibling"] {
            let (folders, error) = session_folders(&hub, session).await;
            assert_eq!(error, None, "{session}");
            assert_eq!(keys(&folders), app_keys, "{session}");
            assert!(
                folders.iter().all(|folder| folder.repo_id == "app"),
                "{folders:?}"
            );
        }
        let (folders, _) = session_folders(&hub, "in-outer").await;
        assert_eq!(
            keys(&folders),
            [normalize_path_key(&outer.to_string_lossy())]
        );
        for (session, id, folder) in [
            ("in-plain", "plain", &plain),
            ("in-nested", "nested", &nested),
        ] {
            let (folders, error) = session_folders(&hub, session).await;
            assert_eq!(error, None, "{session}");
            assert_eq!(
                keys(&folders),
                [normalize_path_key(&folder.to_string_lossy())],
                "{session}"
            );
            assert_eq!(folders[0].repo_id, id, "{session}");
        }
        for session in ["elsewhere", "no-cwd"] {
            assert_eq!(
                session_folders(&hub, session).await,
                (Vec::new(), None),
                "{session}"
            );
        }
    }

    #[tokio::test]
    async fn a_parent_step_below_the_worktrees_root_is_refused() {
        let s = sibling("sibling-parent-step").await;
        let worktrees = s.hub.state.worktrees_dir();
        std::fs::create_dir_all(worktrees.join("x")).expect("create below the root");
        let elsewhere = worktrees
            .parent()
            .expect("the root's parent")
            .join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create elsewhere");
        let stepped = format!("{}/x/../../elsewhere", worktrees.display());
        assert_one_refusal(&s.hub, &stepped).await;
        std::fs::remove_dir_all(&elsewhere).expect("remove elsewhere");
        assert_one_refusal(&s.hub, &stepped).await;
    }

    #[tokio::test]
    async fn a_failed_listing_keeps_the_folders_that_read() {
        let (hub, _scratch) = test_hub("folders-failed");
        let base = TestDir::new("folders-failed");
        let root = as_git_prints(base.path());
        let broken = real_repo(&root, "broken").await;
        let good = real_repo(&root, "good").await;
        let spare = add_worktree(&good, &root.join("good.wt/n"), "n").await;
        break_git(&broken);
        hub.state
            .mutate(|s| {
                s.repos.push(repo_entry("broken", "broken", &broken));
                s.repos.push(repo_entry("good", "good", &good));
            })
            .expect("register repos");
        let mut record = idle_record("s1");
        record.members = [("broken", &broken), ("good", &good)]
            .map(|(name, repo)| SessionMember {
                repo_id: name.to_owned(),
                repo_name: name.to_owned(),
                branch: "main".to_owned(),
                worktree_path: repo.to_string_lossy().into_owned(),
            })
            .to_vec();
        hub.sessions.insert(record);
        let (folders, error) = session_folders(&hub, "s1").await;
        assert_eq!(
            keys(&folders),
            [
                normalize_path_key(&broken.to_string_lossy()),
                normalize_path_key(&good.to_string_lossy()),
                normalize_path_key(&spare),
            ]
        );
        let error = error.expect("the broken repo's reason");
        assert!(error.contains("worktree"), "{error}");
    }

    #[tokio::test]
    async fn a_plain_member_is_listed_with_no_git_run() {
        let (hub, _scratch) = test_hub("folders-plain-member");
        let base = TestDir::new("folders-plain-member");
        // Inside a real repo: git run here would answer for `outer`.
        let outer = real_repo(&as_git_prints(base.path()), "outer").await;
        let plain = outer.join("notes");
        std::fs::create_dir_all(&plain).expect("create a plain folder");
        hub.state
            .mutate(|s| s.repos.push(repo_entry_plain("r1", "r1", &plain)))
            .expect("register repo");
        let mut record = idle_record("s1");
        record.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: String::new(),
            worktree_path: plain.to_string_lossy().into_owned(),
        }];
        hub.sessions.insert(record);
        let (folders, error) = session_folders(&hub, "s1").await;
        assert_eq!(error, None);
        assert_eq!(
            keys(&folders),
            [normalize_path_key(&plain.to_string_lossy())]
        );
    }

    #[tokio::test]
    async fn session_folders_of_an_unknown_session_say_so() {
        let (hub, _scratch) = test_hub("folders-unknown");
        let (mut client, _session, _) = local_session(&hub).await;
        let (folders, error) = asked_folders(&mut client, "nope").await;
        assert_eq!(folders, []);
        assert_eq!(error.as_deref(), Some("unknown session nope"));
    }
}
