# herdr-gpui: remote file links and upload-back over SSH

Read 2026-10-17 from the `feat/remote-file-links` branch of the leftos fork of herdr-gpui (tip `f4a3574a`, local checkout `X:\dev\_ext\penso\herdr-gpui.wt\remote-file-links`), proposed upstream as [penso/herdr-gpui#627](https://github.com/penso/herdr-gpui/pull/627), open at the time. It is in daily use in the fork and works well. Notes are for [remote-file-transfer.md](../plans/remote-file-transfer.md).

## What it does

A modifier-click on a path printed in a pane running on an SSH host fetches the file over that host's SSH login into a private local cache and opens the copy in the system's default application (a non-document opens its folder). The copy is then watched; a save asks once whether to upload it back (**Upload**, **Upload future saves without asking**, **Not now**), and every upload is refused when the host's file changed since the copy was synced.

On a conflict, a dialog offers **Keep both** (uploads beside it as `name.conflict.ext`), **Overwrite anyway**, or **Re-fetch** (drops the local edits). `herdr-gpui remote-files` lists each window's copies with host, host path, local path, sync state and upload policy, read-only.

Files over 64 MB are neither fetched nor uploaded. Hover never contacts the host: a path is a link by its shape alone.

## Where the code is

| File | Owns |
|---|---|
| `crates/herdr-client/src/remote_file.rs` | `fetch_remote_file` / `push_remote_file`, the `Expect` and `Access` enums, the exit-code contract, `MAX_REMOTE_FILE_BYTES` |
| `crates/herdr-client/src/remote_file/posix.rs` | the `/bin/sh` fetch and push scripts |
| `crates/herdr-client/src/remote_file/powershell.rs` | the Windows PowerShell 5.1+ scripts, sent as `-EncodedCommand` |
| `crates/herdr-gpui/src/remote_files.rs` | per-window `RemoteFiles`: copies by `(target, path)`, the fetch into the cache, the prompt queue |
| `crates/herdr-gpui/src/remote_files/sync.rs` | the sync state machine, one pure `step(state, policy, event)` |
| `crates/herdr-gpui/src/remote_files/watch.rs` | the save watcher and its two-run settle |
| `crates/herdr-gpui/src/remote_files/push.rs` | `push_copy`: exact-length, hash-as-you-send |
| `crates/herdr-gpui/src/remote_files/uploads.rs` | the window side: poll tick, prompts, pushes, re-fetch |
| `crates/herdr-gpui/src/remote_files/cache.rs` | the per-launch cache folder, its lock and the stale-folder sweep |
| `crates/herdr-gpui/src/window/remote_paths.rs` | turning a printed path plus the pane's cwd into a host path and a shell |

Tests run the POSIX scripts through a local `/bin/sh` and, opt-in, the PowerShell scripts through local `powershell.exe` and `pwsh`.

## The transfer contract

- **Bytes only on stdout and stdin.** The path travels quoted inside the script text; file contents never touch a command line. PowerShell reads and writes the raw console streams (`[Console]::OpenStandardInput()` / `OpenStandardOutput()`), since PowerShell's own pipeline re-encodes bytes as text.
- **One exit-code contract for both shells.** A refusal exits with a fixed code before streaming or replacing anything: 65 symlink, 66 missing, 67 not a regular file, 68 too large, 69 no SHA-256 tool, 70 short transfer, 71 access cannot be kept, 75 conflict. The client maps each to a typed error; anything else is a generic script failure.
- **A fetch returns its byte count and SHA-256**, hashed by a writer wrapper on the way to the output. That hash is what a later push expects.
- **A push states what it expects** of the host's file just before the rename: `Sha256(digest)` (still the file as fetched), `Absent` (nothing there, not even a dangling link: used for Keep both), or `Any` (Overwrite anyway).
- **A push states its exact length.** The host writes stdin to a temp file and refuses (exit 70) unless it holds exactly that many bytes, so a dropped connection or a cancel never lands a truncated file. The client reads the copy with `take(len)` from its stat'ed length and hashes exactly what it hands over, so the hash it records as synced is the hash of what the host received.
- **Temp file beside the target, then rename.** The temp is dot-named in the same folder (same filesystem), is checked again for symlink or non-file just before the rename, and is removed on every exit that did not rename it (a `trap` on POSIX, `finally` in PowerShell). A symlink or non-regular target is never replaced.
- **A 30-second no-progress timeout** on the child's streams ends a stalled transfer.
- **Known gap, accepted:** another writer can slip in between the hash check and the rename; the host has no lock every editor honours.

## Keeping the file's access on upload

The rule: an upload never lets anyone read the bytes who could not read the file they replace (or, for a new Keep-both file, the file it sits beside).

- **POSIX.** The temp is created under `umask 077` (0600), then given the target's group (`chgrp`, only when it differs) and mode (`chmod`, after `chgrp` since that can clear bits). Mode and group are read with GNU `stat -c` or BSD `stat -f`.

  When access cannot be copied faithfully (an ACL shows as `+` or macOS's `@` in `ls -ld`, or `getfacl -s` lists one; `chgrp` fails and the group and other bits differ; the mode or group cannot be read), an existing target is written **in place** instead (`cat tmp > path`), which keeps owner, group, ACL and hard links but is not atomic. A target the SSH user cannot write is refused (71) before anything changes.
- **Windows.** The temp is created with the target's DACL (`FileStream` with a `FileSecurity` on Windows PowerShell 5.1; `FileSystemAclExtensions.Create` on PowerShell 7), then `[System.IO.File]::Replace(tmp, path, [NullString]::Value)`, which keeps the target's DACL; `[NullString]::Value` because PowerShell would pass `$null` as an empty backup name. A new file uses `File.Move`.
- **Hashing on Windows uses .NET** (`SHA256.Create().ComputeHash`), not `Get-FileHash`, whose module fails to load in Windows PowerShell when `PSModulePath` lists PowerShell 7's modules first.
- **Documented limitation:** on a macOS host an inheritable folder ACL can read the 0600 temp file during the transfer.

## The sync state machine

`sync::step(state, policy, event) -> (state, policy, action)` is pure and holds every rule; the window only feeds it events and runs the action it returns (`Ask`, `ShowConflict`, `Push(Expected | Any | Beside(n))`, `Refetch`, `Nothing`).

States: `Synced`, `Edited` (an upload failed; waits for the next save), `Asking`, `Uploading`, `Refetching`, `Conflict`, `Declined` (Not now). Every state after a save carries that save's SHA-256. The policy is `Ask` or `Always`, kept per copy and carried over when the same file is fetched again.

Rules worth copying:

- A save whose bytes equal the synced hash is a **revert**: it returns the copy to `Synced`, drops any waiting question, and closes an open dialog about it.
- A save while `Conflict` re-shows the conflict question rather than pushing again, since the host still differs.
- A save that matches the hash the user already declined asks nothing.
- Saves while `Uploading` or `Refetching` are ignored; the watcher skips copies mid-transfer.
- Keep both tries `name.conflict.ext`, then `.conflict-2` up to `-9`, each pushed with `Expect::Absent`, and reports when every name is taken. The conflict name splits at the host's own separators (a backslash is a name character on POSIX) and treats `.bashrc` as having no extension.
- A failed Re-fetch puts the conflict question back.

## Noticing saves

A background run every second at most stats each copy (mtime and length). A copy counts as saved only once **two runs in a row see the same changed metadata**, which lets an editor's write-then-rename save settle without a timer; only then is it hashed. A copy that is momentarily missing (mid-rename) is skipped silently.

A run's findings carry the synced metadata they compared against, and are dropped if the copy was synced again or started a transfer meanwhile.

Questions wait in a queue until no other dialog is open, and an upload question that another page displaced is queued again.

## The local cache

- One folder per app launch under the state dir, `remote-files/<pid>-<time>-<serial>`, 0700, holding a locked `.lock`. The first use in a later launch removes every launch folder whose lock nobody holds, so crashed runs are cleaned without touching a live instance. The lock is taken under `.lock.new` and only then renamed, so a sweep cannot catch the window between create and lock. The folder is removed at quit.
- Copies are keyed per window: `w<window>/<hash(target)>/<hash(path)>/<basename>`, so the same host file in two windows is two copies and one window's fetch never overwrites another's edits; the file keeps its real name so applications show its name and type.
- A fetch writes `<name>.<serial>.part` (0600, `create_new`) and renames it over the copy only when whole, so a failed fetch leaves the older copy intact and two fetches never write each other's part file.
- A click on a copy with unuploaded edits reopens it rather than fetching; a synced copy is fetched again since the host may be newer. A click while the same file is fetching does nothing more.
- The re-fetch checks for local edits **as late as possible**, after the bytes arrive and before the rename: a save made while the fetch ran wins, and the fetched part is discarded.

## Path resolution

A printed path becomes a host path and a shell without asking the host: a drive (`C:\`, `c:/`) or UNC path means a Windows OpenSSH host and PowerShell; a leading `/` means POSIX. A relative path joins the pane's cwd with the host's separator, choosing the shell from the cwd's shape. `~` and `~/…` are left for the host to expand (`$HOME`, `$env:USERPROFILE`); `~user` is not resolved. A `file://` URL's `/C:/…` is read as a drive path.

## What carries over to rustling-tulip

Our transport differs: the daemon already runs on the host and streams over the WebSocket (`FetchFile` / `FileChunk` / `CancelFetch`, FT.1, landed), so there are no shell scripts, and the daemon can do the temp-file, hash and ACL work natively in Rust. These parts transfer directly:

- **For FT.2, the download sink:** a unique part name per fetch (`<name>.<serial>.part`, `create_new`) rather than a fixed `<file>.part`, so two fetches of one file never clobber each other; and the late "edited since synced" check before the rename, if copies are ever re-fetched over.
- **Hash on fetch.** Having `FileFetchDone` (or `FileFetchStarted`) carry the SHA-256 the daemon read costs nothing and is the base any later write-back needs.
- **Upload-back is new scope for us.** `remote-file-transfer.md` is download-only. If we add it, the herdr shape is a good template: a `PushFile { expect: Sha256 | Absent | Any, len }` with the daemon refusing a short body and a changed target; a temp beside the target plus `ReplaceFileW` on Windows (keeps the DACL) or rename with copied mode and group on POSIX; the pure state machine above; the two-run settle watcher; Keep both / Overwrite / Re-fetch on conflict.
- **Size.** Our ruling is no cap, streamed. Herdr caps at 64 MB both ways; with upload-back, a cap on the push side bounds what a stray save of a huge file can send.
- **Cache lifetime.** Our plan keeps downloads in a persistent per-host folder (`<data_local_dir>/remote-files/<host-id>/<repo-name>/<rel-path>`); herdr keeps a per-launch folder removed at quit, with a lock-guarded sweep for crashed runs. With upload-back, per-launch is safer: a stale copy with edits from a past run would otherwise sit there looking current. Download-only, our persistent layout is fine.
- **Typed refusals.** One error enum mapped from a fixed set of reasons (missing, not a file, too large, symlink, conflict, incomplete, access) keeps the client's messages exact; our `FileFetchError { error }` is a free string today.
