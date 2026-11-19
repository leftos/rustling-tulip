# Phone files

Fetch a host's files to the phone, open them there, and pass them on: a tapped path in a session's chat or terminal opens the file in the phone's viewer, or a folder in a browser that walks the whole host. Copies are read-only snapshots kept on the phone, visible in the iOS Files app, and refreshed when the host's file has changed. Nothing on the host changes. This is RT-106 (PW.12) for the native phone app ([mobile-app.md](./mobile-app.md), "After the MA phases").

The phone reuses the desktop's host-path fetch (RT-1823) and the file manager's host info and listing (RT-1361, [remote-file-manager.md](./remote-file-manager.md)); it adds no daemon message of its own.

## Owner rulings

- **Read-only fetch.** "Syncing" means fetching a copy of a host file or folder to the phone to open, share or save. Nothing goes back to the host: no upload, no two-way folder mirror, no edit on the phone.
- **Reach: the whole host.** Any path the host's daemon user can read, as the desktop file manager's reach ruling has it, through `stat_host_path`, `fetch_host_file`, `fetch_host_folder` and RT-1361's listing.
- **Entry: tapped paths.** A path tapped in a session's chat or terminal is where browsing starts. A file path fetches the file and opens it in the viewer; a folder path opens the browser at that folder, and from there the parent row and the folder rows reach any path on the host. There is no Files button on the session page and no host browser on Home.
- **Opening:** the in-app viewer (text and code highlighted, Markdown rendered, images shown), the iOS share sheet (Save to Files, AirDrop, Open in another app), and fetched copies visible in the Files app under On My iPhone › Rustling Tulip. No File Provider extension.
- **Freshness: check, refetch if changed.** Opening a copy while its host is connected stats the host file and refetches it when its size or modified time differs. With the host unreachable, the copy opens as it is under a line saying it may be out of date.
- **Retention: auto-clear.** A copy not opened for 30 days is removed. Settings shows the space fetched copies use, with a Clear button.
- **Placement:** release vNext, with RT-1361 (host info and listing) pulled into vNext for it. The desktop file-manager tab and panel (RT-1363, RT-1364) stay out of vNext.
- **Out of this plan:** sending photos and files from the phone into a session (RT-106's original "send into a session") is its own Backlog issue.

## Orchestrator decisions (open to the owner)

- **Paths are links on the phone without a modifier.** The phone's chat and terminal find paths with the desktop's `links::detect_links` and draw them underlined at all times, since the phone has no Ctrl. A single tap on a path opens it; a tap anywhere else does what it does today (focus the terminal and raise the keyboard, expand a tool card). A URL tap is unchanged by this plan.
- **Resolution as the desktop's Ctrl+click.** A relative path resolves against the session's folders as `RootView::open_link_click` resolves it on a remote pane; an absolute path goes straight to the host-path messages. `stat_host_path` decides file or folder before anything is fetched; a missing path shows "Not on <host>: <path>" inline, never a fetch.
- **Copies live in the app's Documents folder.** `daemon_client::remote_files_dir()` on iOS resolves to `Documents/`, keyed by host and path as the desktop's remote-files folder is, and `Info.plist` gains `UIFileSharingEnabled` and `LSSupportsOpeningDocumentsInPlace`, which make that folder the app's Files location. A fetched folder lands as a folder there.
- **The fetch index is not in Documents.** Each copy's host, host path, size, modified time and last-opened time live in `fetched.json` in the client's config dir, beside `native-ui.json`, written atomically, so the Files app never shows it. A copy the user deleted in Files drops out of the index at the next read; the index never brings it back.
- **The 30-day sweep runs on start and on return to the foreground,** after the lifecycle's redial, removing every copy whose last-opened time is older than 30 days, and a folder copy as one item.
- **The viewer is a phone page.** It opens over the session page (Back returns), and on a two-pane layout ([tablet-layout.md](./tablet-layout.md)) in the session pane. It reuses the desktop viewer's text rendering (`viewer_text`, highlighted) and Markdown view (`viewer_md_view`) and shows images scaled to fit with pinch to zoom; any other type shows a card with its name, size and Share. The header names the file, its host and path, the stale line when it applies, and Share.
- **The browser is a phone page over RT-1361's listing.** A header with the host and the folder's path, a parent row, then one row per entry (name, size, modified), folders first, hidden entries left out unless Show hidden is on. A folder row enters it; a file row fetches and opens it; a long press on a folder offers Fetch folder, which fetches it whole into Documents. Pages of the listing load as the list scrolls.
- **Share is a seam.** A `ShareSheet` seam in the phone module presents `UIActivityViewController` for a copy's file URL on iOS (objc2, as `push_ios.rs` does) and records the request on every other platform, so the UI specs prove what is shared on Windows. A folder copy is shared as its folder URL, which iOS zips for AirDrop and Save to Files.

## Steps

- [x] Copies on the phone: `remote_files_dir` on iOS in Documents, the two `Info.plist` keys, `fetched.json`, the fetch seam that records a saved fetch, the 30-day sweep, and Settings' Storage row with Clear.
- [x] Opening a copy again: the freshness check (stat, refetch when changed, the stale line offline), and the open that sets a copy's last-opened time (RT-1935).
- [x] The viewer page's highlighted text and rendered Markdown, the header and its stale line (RT-1900).
- [x] The viewer page's images and the other-type card; `native-shot` views `phone-viewer` and `phone-viewer-md` (RT-1946; pinch to zoom stays RT-1931, Share RT-1901).
- [x] The share sheet seam and Share in the viewer header, proved in the simulator by `tools/ios/sim-share.ps1`'s log (RT-1901; sharing a folder copy waits for RT-1956).
- [ ] Tapped paths: links in the phone's chat and terminal, resolved and stat-ed, opening the viewer or the browser, and the inline not-on-host line.
- [x] The browser page over RT-1361's listing, with paging and Show hidden; `native-shot` view `phone-browser` (RT-1955).
- [ ] Fetch folder from a long press on a folder row (RT-1956).
- [ ] Hand check on the phone through TestFlight: a tapped file opens, a changed file refetches, an offline open says so, a copy shows in Files, Share saves to Files.
