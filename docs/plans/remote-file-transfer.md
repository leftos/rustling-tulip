# Remote file transfer

When the native client is connected to a daemon on another machine over Remote LAN access (see `docs/remote-lan-access.md`), fetch a file from the host and open it on the machine you're sitting at. There are two triggers: Ctrl-clicking a path in a terminal, and a "Fetch file…" popup where you type any path under a repo.

In this doc, the **host** is the machine running the daemon and the **remote client** is the machine running the native client connected to it.

## Status

Blocked on two native-client phases (see [native-client.md](./native-client.md)):
- Phase 2 (terminal link detection and Ctrl-click open), for the link trigger.
- Phase 6 (connection picker, pinned-TLS tunnel), because the native client can't connect remotely until then.

The daemon and protocol half (FT.1) has landed.

## Rulings (user, 2026-09-24)

- **Client:** native client only. The frozen Tauri app does not get this feature, and there it keeps showing its "not available remotely" toast.
- **Viewing:** the remote client saves the file to a per-host download folder, then opens it the way a local Ctrl-click does: `code -g path:line:col` when the link has a line, otherwise the OS default handler.
- **Scope:** only paths inside a registered repo root or one of its session worktrees. The daemon canonicalizes the path and rejects `..` segments, absolute paths that land elsewhere, and symlinks that point outside.
- **Size:** no cap. The file streams in chunks so PTY traffic on the same socket keeps flowing.

## Design

### Protocol (additive, no version bump)

- `ClientMessage::FetchFile { id, repo_id, worktree_path: Option<String>, path }`: `path` is relative to the repo root or worktree. The Ctrl-click trigger resolves an absolute or cwd-relative terminal path against the session's worktree roots before sending, and sends every candidate reading, as `open_terminal_path` does now.
- `DaemonMessage::FileFetchStarted { id, resolved_path, size }`, then `FileChunk { id, seq, data_b64 }` with chunks of about 256 KiB, then `FileFetchDone { id }`, or `FileFetchError { id, error }` at any point.
- `ClientMessage::CancelFetch { id }`: the popup's Cancel button and closing the connection both stop the stream.

### Daemon

- One path-confinement helper used by `FetchFile`. It checks that the root is a registered repo, or a worktree whose canonical path is under the canonical `worktrees_dir()`. It rejects `..` and absolute paths lexically, then canonicalizes the joined path and requires it to `starts_with` the canonical root. `GetFileSnapshot` and `GetFileDiff` use the same helper. Every rejection names the path it rejected and the reason.
- Reads run on a blocking task with a bounded channel into the connection's send queue, so a slow remote client applies backpressure instead of making the daemon buffer the whole file. PTY output keeps priority.
- A directory, a missing file, or an unreadable file ends in `FileFetchError`.

### Native client

- Downloads go to `<data_local_dir>/remote-files/<host-id>/<repo-name>/<rel-path>`, keeping the repo-relative layout so paths in the file still make sense. Each chunk is written to `<file>.part`, which is renamed on `FileFetchDone` and deleted on error or cancel.
- In remote mode, Ctrl-click sends `FetchFile` instead of opening the path locally. Local mode is unchanged.
- The "Fetch file…" popup is a path input scoped to one repo or worktree, picked from the focused session. It shows progress (bytes of `size`) and a Cancel button, and reports errors inline.
- Once the file is saved, it opens the same way a local Ctrl-click does, including the `:line[:col]` rule.

## Items

- [x] **FT.1 Daemon + protocol:** the `FetchFile` / `CancelFetch` messages, the path-confinement helper, and chunked streaming with backpressure. Tests cover: a `..` escape, an absolute path outside the root, a symlink escape (skipped when the OS denies symlink creation), a directory, a missing file, an empty file, a file larger than one chunk that reassembles byte-exact, and cancel mid-stream.
- [ ] **FT.2 Native download sink:** the per-host download folder, `.part` writes and rename, and the open hand-off with the `:line` rule. `FileFetchStarted.resolved_path` is the daemon's canonical path, so on Windows it starts with `\\?\`; strip that before showing it. Needs Phase 6.
- [ ] **FT.3 Ctrl-click trigger:** in remote mode, terminal links send `FetchFile` with candidate readings. Needs Phase 2 links and FT.2.
- [ ] **FT.4 "Fetch file…" popup:** the path input scoped to the focused session's repo or worktree, with progress, cancel, and inline errors. Needs FT.2.
