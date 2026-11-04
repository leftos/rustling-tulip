//! The folder fetch messages. `FetchFolder` resolves its folder, then walks
//! it ([`crate::folder_pack`]) on a blocking thread started through
//! [`start_fetch`], so it shares `FetchFile`'s ids, `CancelFetch` and file
//! queue; `StatPath` resolves the same way and answers `PathStat`. All their
//! disk work, the root's `canonicalize` included, runs on a blocking thread.
//! Both are served on any connection `FetchFile` is served on.
//!
//! A folder that passes its walk is answered with [`PACKING_UNAVAILABLE`].

use super::Hub;
use super::connection::{ConnCtx, start_fetch};
use super::git::repo_target_or_err;
use crate::file_fetch::{self, CanonicalRoot};
use crate::folder_pack::{self, Limits};
use anyhow::Context as _;
use protocol::{ClientMessage, DaemonMessage};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// The `FileFetchError` a `FetchFolder` whose walk passed is answered with.
pub(super) const PACKING_UNAVAILABLE: &str = "folder fetch: packing is not available yet";

/// A `FetchFolder` or `StatPath` request's target.
struct Request {
    repo_id: String,
    worktree_path: Option<String>,
    path: String,
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
            let request = Request {
                repo_id,
                worktree_path,
                path,
            };
            let hub = hub.clone();
            start_fetch(ctx, id, Ok(request), move |id, request, _tx, token| {
                tokio::task::spawn_blocking(move || Some(fetch_folder(&hub, id, &request, &token)))
            })
            .await;
        }
        ClientMessage::StatPath {
            id,
            repo_id,
            worktree_path,
            path,
        } => {
            let request = Request {
                repo_id,
                worktree_path,
                path,
            };
            let hub = hub.clone();
            let out_tx = ctx.out_tx.clone();
            tokio::spawn(async move {
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

/// The repo or worktree root `request` names. A `worktree_path` must be one
/// git working tree's root: the repo root itself, or a folder holding a
/// `.git` entry, so a fetch never spans several worktrees. Disk work.
fn target_root(hub: &Hub, request: &Request) -> anyhow::Result<PathBuf> {
    let root = repo_target_or_err(hub, &request.repo_id, request.worktree_path.as_deref())?;
    let Some(worktree) = &request.worktree_path else {
        return Ok(root);
    };
    let canon_root = std::fs::canonicalize(&root)
        .with_context(|| format!("resolving worktree_path {worktree}"))?;
    let worktrees = hub.state.worktrees_dir();
    if std::fs::canonicalize(&worktrees).is_ok_and(|canon| canon == canon_root) {
        anyhow::bail!(
            "worktree_path {worktree} is the worktrees root {} itself, not a worktree",
            worktrees.display()
        );
    }
    let repo = repo_target_or_err(hub, &request.repo_id, None)?;
    let is_repo = std::fs::canonicalize(&repo).is_ok_and(|canon| canon == canon_root);
    if !is_repo && std::fs::symlink_metadata(canon_root.join(".git")).is_err() {
        anyhow::bail!("worktree_path {worktree} is not a worktree: it holds no .git");
    }
    Ok(root)
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
/// folder, then hand back the one `FileFetchError` this build answers with,
/// for [`start_fetch`] to send once the id is free.
fn fetch_folder(
    hub: &Hub,
    id: String,
    request: &Request,
    token: &CancellationToken,
) -> DaemonMessage {
    #[cfg(test)]
    hooks::before_scan(&request.path);
    let walked = resolve(hub, request).and_then(|dir| {
        let scan = folder_pack::scan(&dir, Limits::FETCH, token)?;
        Ok((dir, scan))
    });
    let error = match walked {
        Ok((dir, scan)) => {
            debug!(
                %id,
                folder = %dir.path().display(),
                files = scan.files.len(),
                bytes = scan.bytes,
                skipped = ?scan.skipped,
                unlisted = scan.unlisted,
                "folder: the walk passed; packing is not available yet"
            );
            PACKING_UNAVAILABLE.to_owned()
        }
        Err(err) => format!("{err:#}"),
    };
    DaemonMessage::FileFetchError { id, error }
}

/// A worker that panics, from a test.
#[cfg(test)]
pub(super) mod hooks {
    /// The `FetchFolder` path whose worker panics.
    pub(super) const PANIC_PATH: &str = "panic-in-the-worker";

    /// Panic when the request names [`PANIC_PATH`].
    pub(super) fn before_scan(path: &str) {
        assert_ne!(
            path, PANIC_PATH,
            "the test hook panicked the folder fetch's worker"
        );
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
        HANG_GUARD, MemClient, ScratchDir, hello, mem_session, repo_entry, test_hub, test_peer,
    };
    use super::*;
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
        assert_eq!(f1.len(), 1, "{f1:?}");
        assert_eq!(error_text(&f1[0]), PACKING_UNAVAILABLE);

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
        let last = y.last().expect("the fetch ended");
        assert_eq!(error_text(last), PACKING_UNAVAILABLE, "{y:?}");
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
        assert_eq!(error_text(&again[0]), PACKING_UNAVAILABLE, "{again:?}");
    }

    #[tokio::test]
    async fn folder_fetch_is_not_yet_listed_in_welcome() {
        let (hub, _scratch, _repo) = repo_hub("folder-welcome");
        let (_client, _session, features) = local_session(&hub).await;
        assert!(
            features.iter().any(|f| f == protocol::FEATURE_FILE_TAIL),
            "{features:?}"
        );
        assert!(
            !features.iter().any(|f| f == protocol::FEATURE_FOLDER_FETCH),
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
            assert_eq!(
                error_text(again.last().expect("an end")),
                PACKING_UNAVAILABLE,
                "{again:?}"
            );
        }
    }
}
