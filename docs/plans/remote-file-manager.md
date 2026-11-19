# Remote file manager

A file manager in the native client that browses each machine's own file system in that machine's own paths. From a Mac, a file manager for a Windows host takes `x:\dev\yaat` in its location bar and opens it; no separator is converted. Its actions match what Ctrl+click on a terminal path offers, and that menu gains an item opening the path's folder in the file manager.

## Owner rulings

- **Reach:** pairing assumes the owner owns both machines, so any path the daemon's user can read is servable to a paired device through the host-path messages (`fetch_host_file`, `fetch_host_folder`, `stat_host_path`; RT-1823). There is no host switch and there are no approved extra roots; the repo-relative messages are unchanged.
- **Surfaces:** both a tab (a file-manager tab per host and folder, with a location bar, placed and split like any tab and kept across restarts) and a sidebar panel (a file tree docked in the sidebar).
- **First cut is read-only:** browse; type or paste a path; open a file in its default app, the viewer or VS Code; copy a file or a folder to this machine and open it; reveal the copy; copy the path. Nothing on the host changes.
- **Later, in Backlog:** rename, delete and new folder on the host; upload from this machine to a host folder.
- **Placement:** release vNext+1, apart from the host info and listing (RT-1361), which is in vNext for the phone's files ([native-client.md](../native-client.md) "Phone shape").

## Orchestrator decisions (open to the owner)

- **Native paths from the host.** The client learns each host's path style from a new additive reply carrying the host's OS, its home folder and its roots (drives on Windows, `/` elsewhere), since `Welcome` has no OS today. The location bar parses a path by the host's style, never by this machine's: on a Windows host `\` and `/` are both separators and drive letters and UNC prefixes are accepted; on a POSIX host only `/`. A path that cannot be that host's shows an inline error. The text shown is the host's own canonical path (`\\?\` stripped for display), case-insensitive on Windows.
- **Listing:** a new paged message family, `ListDir { host path, cursor, limit }` answered with entries `{ name, kind, size, modified, link, hidden }` and a cursor, at most a few thousand entries per reply, under per-connection bounds like the fetch messages'. Links are listed as links and followed only inside what the reach allows.
- **Fetch by absolute path:** the host-path messages (`fetch_host_file`, `fetch_host_folder`, `stat_host_path`; RT-1823) reuse the `FileChunk` streaming, the folder archive and the client's fetch-and-open sinks; copies land under the remote-files folder keyed by host and path.
- **This PC uses the same file manager**, reading its own disk directly behind the same listing seam, so the tab and the panel never care which machine they show.
- **Ctrl+click** gains a fourth item, `Open in file manager`: a folder opens as itself, a file opens its containing folder with the file selected, on the host the path belongs to.

## Steps

- [x] Host info and paged listing: the reply with OS, home and roots; `ListDir` under the reach ruling above; `docs/wire-protocol.md`.
- [x] Fetch by absolute path, reusing the streaming and archive paths (RT-1823).
- [ ] The file-manager tab: `TabContent` variant (additive; `Unknown` absorbs it in older clients), location bar, listing view, the read-only actions; This PC behind the same seam.
- [ ] The sidebar panel: a docked file tree per machine over the same listing.
- [ ] `Open in file manager` in the Ctrl+click menu.

## Related

- RT-106 (phone files, [native-client.md](../native-client.md) "Phone shape") browses a host folder through the same listing.
- RT-889, RT-891, RT-893, RT-895: fetch hardening that the host-path fetch shares.
