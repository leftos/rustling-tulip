PROTOCOL: 1 overlay: none (docs/agents/explore.md absent), read CLAUDE.md; 2 README: skipped, docs/README.md does not exist; 3 Task Index: done (docs/architecture.md:69-129; no row for "what a path link opens in", nearest rows are "remote host" l.93 and RT-35's "Fetch a file" row); 4 ck: skipped, the ck tools were deferred and the Task Index plus native-client.md answered the where-to-look question; 5 design docs: done (docs/plans/remote-file-transfer.md); 6 source: done; 7 tree search: done for markdown crates, text-selection and tab-model fan-out.

# RT-331 plan: built-in text and Markdown viewer (rustling-tulip native client)

Anchored at X:/dev/rustling-tulip, branch main. While this ran, RT-35 landed on main as 88daeed4 ("save files fetched from a remote host and open them here"). So `Origin`, `Route`, `run_open`, `open_by`, `open_fetched_job`, `file_fetch.rs` and `file_fetch_view.rs` are on main now. The worktree X:/dev/rustling-tulip.wt/rt35-ft2 is the same commit and can be dropped.

## ANSWER IN SHORT

- Viewer = a new daemon-stored tab kind `TabContent::Viewer { path }`, copying the diff tab's mechanism. Diff tabs are NOT client-only, so the premise in the brief is wrong. A purely client-side tab would have to touch about 113 `tabs()/active_id()/active_tab()` call sites across 16 files, while a daemon-stored kind reuses all of that (pill, activation, close, restore, undo).
- The routing seam is two places: `RootView::after_resolve` (`apps/native/src/open_view.rs`, link path) and `RootView::open_fetched` (`apps/native/src/file_fetch_view.rs:276`, fetched path). Each asks one new plain-Rust decision: viewer, or today's VS Code / default-app route.
- The setting is `general.open_text_externally` (bool, default false) in `GeneralSettings` (`apps/native/src/sidebar.rs:472`, native-ui.json). When on, behaviour is exactly today's: `:line` goes to VS Code, a plain path goes to the OS default app.
- The viewer's "Open in default app" button reuses `open_by` / `run_open` with the tab's recorded `Origin`. That keeps the remote gate (`dispatch_open`'s `local_files_refusal`) and the fetched-copy check (`is_fetched_copy`) unchanged.
- Markdown: add `pulldown-cmark` (not in Cargo.lock; I searched pulldown-cmark, comrak and markdown, no hits). Rendered view by default, Source toggle, a first-cut subset, no images.
- Text features in the first cut: line numbers, jump to `:line`, syntax highlighting reusing `syntax.rs`, copy-all, copy-path, the two open buttons. Later: line selection and copy, find, live reload, wrap.

## 1. WHERE A VIEWER FITS

### What exists (evidence)

- The tab strip is rendered from the daemon's `TabEntry` list. `TabContent` is `Grid | Diff` (`crates/protocol/src/lib.rs` near 1504), per client id (`docs/native-client.md` "Tab layouts"). A diff tab is `OpenDiffTab { id, repo_id, path, against, worktree_path }` -> `DiffTabOpened { id, tab_id }` (`protocol/lib.rs` near 2566 and 3603). The client then keeps an `Entity<DiffTabView>` per tab id in `RootView.diff_tabs` (`apps/native/src/lib.rs:675`).
- `reconcile_diff_tabs` (`diff_tab_view.rs:688`) builds those views from the tab list. `grid_area` (`grid_view.rs:860-880`) matches on `TabContent` to draw the body. The pill label is `Δ name` (`tab_bar.rs:1088`).
- `TabContent` has no `#[serde(other)] Unknown`. `TabContent::` appears 20 times in the daemon (`tabs.rs` 10, `server/tabs.rs` 5, `presets.rs` 2, `state.rs`, `recovery.rs`, `test_support.rs`) and 10 times in native non-test code (`grid_view.rs:864`, `tab_bar.rs:1088`, `tab_menu.rs:175`, `diff_tab_view.rs:694`, `pane_menu.rs:139`, `undo.rs:569`). I counted occurrences only; I did not read the daemon sites.
- The unit of "client-only vs daemon-stored": diff and grid are both daemon-stored. There is no client-only tab kind today.

### Recommendation: Option A, `TabContent::Viewer { path: String }`

- Identity is the absolute path, so a second open of the same path focuses the existing tab. This mirrors `OpenDiffTab`'s "open or focus".
- New messages: `OpenViewerTab { id, path }` -> `ViewerTabOpened { id, tab_id }`. This is additive, so no protocol bump (CLAUDE.md "When to bump"). Do not add `Unknown` to `TabContent`; it follows the Diff precedent and the no-back-compat rule.
- The path is this PC's path (a fetched copy, or a local file). It is stored in the connected daemon's layout for this client id. On a remote host that puts a local path into the host's `state.json`. It is harmless, since the daemon never reads it, but note it in the doc.
- Body: `ViewerTabView` entity per tab id, reconciled the way `reconcile_diff_tabs` does, in `viewer_tab_view.rs` (new).
- `Origin` is NOT stored in the tab. It is re-derived at reconcile time:
  - if `is_fetched_copy(path)` then `Origin::Fetched`;
  - else if `capabilities().opens_local_files()` then `Origin::Link`;
  - else the tab shows a placeholder, "Not available on a remote host: this opens files on this PC", and reads nothing.
  - This closes the restore hole: a restored viewer tab must not read a local path while connected to a remote host.

### Alternatives

- B, client-only local tab list: no protocol change, but the 113 call sites above make it the most expensive. Rejected.
- C, modal overlay (like `run_confirm_layer`, `open_view.rs`): cheapest, no daemon change. But it is not a tab, it blocks the terminals, and it cannot sit beside them. Good as a fallback if A's daemon work balloons, or as a spike.

### How it opens

| Entry | Path | Change |
|---|---|---|
| Ctrl+click path link, local | `grid_view.rs:499` -> `open_link` -> `resolve_path` -> `open::resolve_link` -> `after_resolve` | In `after_resolve`, `Resolution::Open(OpenAction::DefaultApp(path))` and `OpenAction::VsCode{..}` first ask the viewer decision (below). |
| Fetched file (FT.2, landed) | `after_write` -> `open_fetched` (`file_fetch_view.rs:276`) -> `file_fetch::open_job` -> `open_fetched_job` | Ask the same decision in `open_fetched`, with `Origin::Fetched`. |
| Link-click fetch (RT-36 FT.3) and Fetch file… popup (RT-37 FT.4) | both call `fetch_remote_file` with `open_when_done` | Nothing extra. They arrive through `open_fetched`. |

- The decision does file IO (read the head, stat), so it runs on the background executor, as `resolve_path` already does for resolution. Add `viewer::probe(path) -> Probe { size, head }`, then `viewer::decide(setting, action, &probe) -> Verdict`.
- `Verdict` is `Viewer { path, line, column } | External(reason)`. External then follows today's route unchanged: the run confirm, `SayVsCodeMissing` and the rest are untouched.

### What counts as text

Plain-Rust, `viewer_text.rs`:

1. Size cap: 2 MiB. This matches the daemon's `SNAPSHOT_MAX_BYTES` (`crates/daemon/src/git_inspect.rs:224`) and the diff engine's limit (`native-client.md:174`). Over the cap -> External with a toast, "too large for the viewer".
2. Binary sniff: a NUL byte in the first 8000 bytes means binary (the daemon's `looks_binary`, `git_inspect.rs:228,391`). No extension allowlist gate: the sniff decides, and the extension list is only a hint for the language id and for the Markdown mode.
3. Encoding: UTF-8, BOM stripped; UTF-16 LE/BE only with a BOM, via `encoding_rs`. That crate is already in Cargo.lock at 0.8.35 as a transitive dependency; adding it direct is cheap. Other bytes that are not valid UTF-8 -> External. The daemon makes the same refusal ("bytes that are not valid UTF-8", `protocol/lib.rs:3102`). A lossy Windows-1252 fallback with a banner is a later option.
4. Still External: images, PDFs, archives, Office files, anything NUL-bearing, and anything over the cap.
5. Files in `EXECUTABLE_EXTENSIONS` (`open.rs:23`: `.js .py .sh .ps1 .bat` and the rest) are viewed, not run. The viewer is safer than today. Its "Open in default app" button still goes through `OpenJob::DefaultApp { confirmed: false }`, so the run confirm applies as now.

Language id: the daemon's `language_for_path` (`git_inspect.rs:429`) is in the daemon crate and not reachable from the client. `syntax.rs` takes the daemon's ids (`LANGUAGES`, `syntax.rs:87`). Add a small client map ext -> id in `viewer_text.rs` and pin it with a test that every id it returns satisfies `syntax::has_grammar`. Alternative: move `language_for_path` into `crates/protocol`.

## 2. THE SETTING

- Field: `GeneralSettings.open_text_externally: bool` (`sidebar.rs:472`), `Default` false, and `#[serde(default)]` on the struct already covers a file that predates it. Saved as `general.open_text_externally` in native-ui.json. Add it to the field list in CLAUDE.md "Where things live on disk" (the `general` entry) and to `docs/native-client.md` Settings.
- UI: a row in a new "Files" section of Settings -> General, between "Sidebar" and "Terminal behavior", "Open text files in the external app", as a toggle like `CopyOnSelect` (`SettingsControl::CopyOnSelect`, `settings_view.rs:405,581`; the Tab-order stop list at `settings_controls` `:399`). Add `SettingsControl::OpenTextExternally`, its `toggle_*` method and a spec.
- Meaning of "external editor": recommend that ON restores today's behaviour exactly (a link with `:line` goes to VS Code via `-g path:line:col`; a plain path goes to the OS default app), so the viewer is simply bypassed. No new editor-choice semantics in this ticket.
- The viewer header's overflow menu can later offer "Always open text files in the external app" to flip the setting in place.
- Gate order in `viewer::decide`: setting ON -> External; else not text / too large -> External; else Viewer.

### "Open in default app" button and the remote gate

- Header buttons: Open in default app, Reveal, Copy path. Reveal and Open dispatch `OpenJob::DefaultApp { path, confirmed: false }` and `OpenJob::Reveal(path)`.
- Both go through `open_by(job, Route { then: Then::Nothing, origin }, ..)` (`open_view.rs:210`), with `origin` as re-derived above.
  - `Origin::Link` -> `dispatch_open`, which refuses everything but a URL while `capabilities().local_files_refusal()` is set (`open_view.rs:278`). A Link-origin viewer can only exist while connected locally.
  - `Origin::Fetched` -> `open_fetched_job`, which refuses a path outside `remote_files_root` via `is_fetched_copy` (`file_fetch_view.rs:307,328`).
- `open_by` is currently private (`fn open_by`, `open_view.rs:210`); make it `pub(crate)`. Nothing else changes in the open path.
- Disable both buttons, with `LOCAL_FILES_REASON` as the tooltip, when neither check passes (the same pattern as other disabled local-file actions, `remote_mode.rs:10`).
- The viewer's own file read is gated by the same two conditions (see reconcile above).

### GetFileSnapshot as an alternative source: not recommended

`GetFileSnapshot` (`protocol/lib.rs:2407`) returns `old` and `new` for a path under a registered repo (the confined helper, `crates/daemon/src/file_fetch.rs:76`). It carries 2 MiB and binary/UTF-8 refusals, and a language hint. It is a diff, so it ships two sides, and for a file with no change both are the same text. It would allow a "peek" with no disk write. But the RT-35 ruling is fetch-then-open, which also gives the external-app buttons a real file. Keep one path.

## 3. MARKDOWN

- Crate: `pulldown-cmark`, `default-features = false`. Pure Rust, MIT, event stream. Binary-size cost: unmeasured (syntect's was measured at +1.29 MB, `native-client.md:177`; measure this one in the step and record it). Run `cargo deny check` in the step, since licences and advisories are gated.
- Rendered by default, with a Rendered | Source toggle for `md`/`markdown` files. The toggle persists in `UiState` as `viewer_markdown_rendered` (default true), following `diff_highlight` (`sidebar.rs:363`, `set_diff_highlight` `:876`) and appended to the native-ui.json field list in CLAUDE.md. Files over 512 KiB open as Source only (a render cap; tune in the step).
- First-cut subset GPUI can draw:
  - Headings: `div` with text size and weight.
  - Paragraphs and inline strong, emphasis, code, strikethrough and links: `StyledText::with_highlights` (already used at `diff_view.rs:684`, `HighlightStyle` carries weight, style, colour, underline, background).
  - Lists: nested `div` with a bullet or number prefix; task-list boxes as glyphs.
  - Blockquote: left border.
  - Code blocks: a monospace `div`, coloured by `syntax::highlight` when the fence language has a grammar (`syntax::has_grammar`).
  - Horizontal rule.
  - Tables: flex rows with equal column tracks and alignment from the header row. Later: measured columns.
  - Raw HTML: shown as literal text.
  - Images: NOT in the first cut. Show the alt text as a muted label. No network fetch from a viewer, ever (privacy). Local relative images later, via GPUI `img()`; `image`, `png` and `resvg` are in Cargo.lock.
- Link clicks inside it. The target classifier is plain Rust, `classify_link(target, file_dir) -> Url | File(path) | Anchor(slug) | Refused`:
  - `http(s)`: `open::validate_http_url`, then `dispatch_open(OpenJob::Url)`. This works on a remote host because a URL is the one job `dispatch_open` allows.
  - Relative file link: join to the file's folder and feed `open::resolve_link` with `base_dirs = [file's folder]`. It lands in `after_resolve` again, so another `.md` opens in the viewer. On a fetched file the sibling is not fetched, so this ends in NotFound until FT.3 can fetch it. State that limit in the doc.
  - `#anchor`: scroll to the heading by slug.
  - `file:`, `javascript:` and any other scheme: Refused, with a toast, `COULD_NOT_OPEN`.
- Needs a spike, not verified here: `gpui::InteractiveText` (per-range click on a `StyledText`) in 0.2.2, and a variable-height virtualized `list`. If either is missing, links become separate inline `div` spans and the rendered body is a plain scroll container (cap handles size).

## 4. TEXT FEATURES

| Feature | Cut | Note |
|---|---|---|
| Line numbers gutter | first | `uniform_list` rows like `diff_view.rs:796`; fixed row height |
| Jump to `:line` | first | `ScrollStrategy::Center` via `scroll_to_item_strict` (`diff_view.rs:429`); highlight that line |
| Mono font | first | `UiState.terminal_font` (`FontSettings`, as `DiffTabView` takes) |
| Syntax highlighting | first | `syntax::highlight` + `syntax_for` on the background executor; same limits (20,000 lines, 10,000-byte lines, 2 s). Reuse the six token classes |
| Copy all, Copy path | first | GPUI clipboard API (`native-client.md:12`) |
| Selection and copy | second | GPUI has no selectable-text element; the diff view has none (no select/copy symbols in `diff_view.rs`). Do a line-granular selection in a plain `LineSelection` model: click the gutter, Shift-click for a range, Ctrl+C. Character selection later. Fallback for small files: `TextInput::multi_line` + `set_read_only` (`text_input.rs`) has char selection and copy but no gutter, highlight or virtualization |
| Find | second | Ctrl+F, single-line `TextInput`, plain `find_matches(lines, query, case)`, next/prev, match count |
| Live reload | second | no watcher: on tab activation compare mtime and size, then re-read, keeping scroll. A fetched copy changes only when refetched (`done_replaces_an_existing_file` already pinned in `file_fetch.rs`) |
| Word wrap | later | variable row heights break `uniform_list`; no horizontal wrap in first cut (long lines clipped; how the diff view handles them I did not check) |
| Encoding picker, hex view, editing | not planned | read-only |

## 5. STEPS (each sized for at most about 100 implementer calls)

Order is chosen so each step lands green on its own. Tests: Rust names, red first.

- **S1. `viewer_text.rs` (plain Rust). Landed.** Probe/decide core with no GPUI, no I/O except `probe_file`.
  Files: `apps/native/src/viewer_text.rs`, `lib.rs` mod line, `tools/test-map.json` row.
  Red tests: `a_nul_byte_in_the_head_is_binary`, `utf8_bom_is_stripped`, `utf16_with_bom_decodes`, `invalid_utf8_is_not_text`, `a_file_over_two_mib_is_not_text`, `an_unknown_extension_with_text_is_text`, `every_language_id_has_a_grammar`, `crlf_lf_and_lone_cr_split_lines`, `a_final_newline_adds_no_empty_line`.
- **S2. The setting and the decision. Landed.** The row sits in a new "Files" section between "Sidebar" and "Terminal behavior". `.ps1` and `.psm1` map to no language id, since `syntax::LANGUAGES` has no PowerShell grammar. `check` also refuses a head that does not decode (`NotDecodable`), allowing a head cut mid-character.
  Files: `sidebar.rs` (`GeneralSettings.open_text_externally`), `settings_view.rs` (row, `SettingsControl::OpenTextExternally`, stops, toggle), `viewer_text.rs` (`decide`), CLAUDE.md field list.
  Red tests: `open_text_externally_defaults_off`, `a_file_without_the_key_loads_off`, `the_setting_on_sends_a_line_ref_to_vscode_and_a_plain_path_to_the_default_app` (unit, `decide`), `text_goes_to_the_viewer_when_the_setting_is_off`, and UI `ui_settings`: `the_open_text_externally_row_toggles_and_saves`, plus a focus-ring stop check.
- **S3a. Protocol + daemon. Landed.** `TabContent::Viewer { path }`, `OpenViewerTab`, `ViewerTabOpened`, the dispatch arm, the handler in `server/tabs.rs` (open or focus by exact path), and the daemon's `TabContent::` matches; the native client draws a placeholder body and plain pill until S3b.
  Files: `crates/protocol/src/lib.rs`, `crates/daemon/src/server.rs` (dispatch or-pattern), `server/tabs.rs`, `tabs.rs`, `state.rs` (tests), and one-line arms in native `diff_tab_view.rs`, `grid_view.rs`, `tab_bar.rs`. No edits to `net.rs`, `presets.rs`, `recovery.rs` or `test_support.rs`.
  Red tests: `a_viewer_tab_round_trips` (protocol), `opening_the_same_viewer_path_twice_focuses_one_tab`, `a_viewer_tab_survives_a_state_reload`, `a_viewer_tab_is_not_a_pane_target` (daemon `tabs.rs mod tests`).
  CLAUDE.md "Add a daemon/client message" and the `add-protocol-message` skill apply.
- **S3b. Native model + tab plumbing.** `viewer_tab.rs` (plain state: path, origin, load state, line target, markdown mode, `DiffOpens`-style open-id and wait-for-tab bookkeeping), `reconcile_viewer_tabs`, `tab_bar.rs` pill, `grid_view.rs` `grid_area` arm, `tab_menu.rs`.
  Carry from S3a review: the client handles `ViewerTabOpened` and its `Error` reply (per-message handling is in `RootView` and `diff_tab_view.rs` `on_diff_message`; `net.rs` only forwards); an activated viewer tab takes keyboard focus as a diff tab does (`focus_active_diff_tab`); `tab_menu.rs` merge gating treats any tab whose `grid()` is `None` as unmergeable; the client trims and normalises the path before sending; an undo `RestoreTab` of a viewer tab can duplicate an open path.
  Red tests: `an_open_answered_before_its_tab_arrived_waits_for_it`, `a_second_open_moves_the_target_line`, `a_restored_viewer_tab_on_a_remote_host_reads_nothing`, `origin_is_derived_fetched_under_the_download_root`.
- **S4. The text view and the routing.** `viewer_tab_view.rs`: header (name whole, folder dimmed, via `ellipsis::ellipsized`), the three buttons, gutter, `uniform_list`, jump to line, syntax colours, banners (binary / too large / not available). Wire `after_resolve` and `open_fetched` to `decide`; make `open_by` `pub(crate)`.
  New spec `apps/native/tests/ui/ui_viewer.rs` (+ `mod` line in `tests/ui/main.rs`) over `OpenRecorder` (`tests/support/mod.rs:749`).
  Red tests: `a_plain_path_link_opens_a_viewer_tab_not_the_default_app`, `a_line_ref_opens_the_viewer_scrolled_to_the_line`, `a_binary_file_still_goes_to_the_default_app`, `a_big_file_still_goes_to_the_default_app`, `with_the_setting_on_a_link_opens_as_before`, `the_open_in_default_app_button_dispatches_a_default_app_job`, `an_executable_script_is_viewed_and_the_button_still_asks_first`, `on_a_remote_host_a_fetched_file_opens_in_the_viewer`, `on_a_remote_host_the_button_takes_the_fetched_route_and_refuses_a_path_outside_the_download_root`, `on_a_remote_host_a_local_path_link_is_still_refused`.
  Add a `viewer` view to `apps/native/examples/shot.rs` (and the `rt.ps1 native-shot` help list) and read before and after PNGs.
- **S5. Markdown core (plain Rust).** `pulldown-cmark` dependency; `md_blocks.rs`: events -> `Vec<Block>` with inline spans (style flags, link targets), heading slugs, `classify_link`.
  Red tests: `headings_lists_and_code_blocks_become_blocks`, `a_javascript_link_is_refused`, `a_file_link_resolves_against_the_files_folder`, `an_anchor_names_its_heading_slug`, `html_is_kept_as_text`, `a_table_keeps_column_alignment`, `an_image_becomes_its_alt_text`, `a_large_document_is_source_only`. Run `cargo deny check`; record the exe-size delta in `native-client.md`.
- **S6. Markdown view.** `viewer_md_view.rs`, toggle + `UiState.viewer_markdown_rendered`, link clicks routed to `dispatch_open` / `resolve_link` / scroll. Spike `InteractiveText` and `list` first.
  Red tests (UI): `a_markdown_file_opens_rendered`, `the_source_toggle_shows_the_text_and_saves`, `an_https_link_click_opens_the_browser_job`, `a_relative_md_link_opens_another_viewer_tab`, `a_file_scheme_link_toasts_and_opens_nothing`, `a_heading_anchor_scrolls`.
- **S7. Selection, find, reload.** `viewer_find.rs` (`find_matches`), `LineSelection`, mtime/size reload on activation.
  Red tests: `find_matches_are_case_insensitive_by_default`, `next_wraps_at_the_last_match`, `a_shift_click_selects_the_line_range`, `ctrl_c_copies_the_selected_lines_joined_with_newlines`, `an_activation_with_a_changed_mtime_reloads_and_keeps_the_scroll`.
- **S8. Docs.** `docs/native-client.md` new "Text and Markdown viewer" section (and the Links bullet, `native-client.md:149`, which currently says a plain path opens the OS default handler); `docs/architecture.md` Task Index row (below); `docs/plans/native-client-parity.md` tick; `docs/plans/remote-file-transfer.md` Rulings "Viewing" (`:18`) note; CLAUDE.md field list; test-map rows. Fold small doc edits into each step's own commit and do S8 only for what is left.

Each step touches `apps/native/src/lib.rs` (mod lines and `RootView` fields, the rebase hotspot, `native-client.md:11`) and `tools/test-map.json` (`.\rt.ps1 test -Validate`).

## DEPENDS ON

- RT-35 FT.2 fetched files: landed on main (88daeed4). S4's fetched route needs only `open_fetched` / `open_fetched_job` / `Origin` / `run_open`, all present. No wait.
- RT-36 FT.3 link-click fetch: not needed to start. When it lands, a remote link click ends in `open_fetched`, and so in the viewer with no further change. A relative Markdown link on a fetched file stays NotFound until FT.3 can fetch the sibling.
- RT-37 FT.4 Fetch file… popup: not needed. It calls `fetch_remote_file` with `open_when_done`.
- RT-325 UI font (worktree X:/dev/rustling-tulip.wt/rt325-ui-font, branch rt325-ui-font): not read here. The viewer body uses the terminal mono font; the chrome and rendered Markdown use `fonts::UI_FAMILY`. Land S4 and S6 after RT-325, or rebase over it. `fonts::UI_FAMILY` is the only name I rely on and I did not check what RT-325 changes.
- RT-324 headless driving (`docs/plans`, commit 6d9e161e): not read. It is a way to drive the viewer in an end-to-end check; the UI specs in `tests/ui/ui_viewer.rs` over `OpenRecorder` do not need it.

## BRANCH

New branch and worktree off main (88daeed4 or later): `rt331-viewer` at X:/dev/rustling-tulip.wt/rt331-viewer. S1 and S2 are independent of everything and can merge first. S3a changes the protocol and daemon tab code, so rebase carefully against any concurrent tab or protocol work (the `add-protocol-message` skill). Rebase hotspots: `apps/native/src/lib.rs`, `settings_view.rs`, `sidebar.rs`, `tab_bar.rs`, `open_view.rs`, `tools/test-map.json`.

## OPEN DECISIONS

1. **Tab kind: daemon-stored `TabContent::Viewer` (recommended) vs modal overlay.** Worst case for the tab: a protocol and daemon change in a busy file (`crates/daemon/src/tabs.rs`), and a restored tab pointing at a file that has since gone (the view shows a "file not found" banner). Worst case for the overlay: it is not a tab and blocks the terminals, so the owner asks for a tab later and the work is redone.
2. **Meaning of the external setting: ON = today's behaviour (VS Code for `:line`, default app otherwise) (recommended), vs ON = always VS Code.** Worst case: the owner expected an editor for plain text too and gets Notepad. Fix: add an "External editor" choice row later; no stored-data change.
3. **Cap and encoding: 2 MiB, UTF-8 / BOM'd UTF-16 only, else External (recommended), vs lossy decoding with a banner.** Worst case: a Windows-1252 log refuses to view and goes to the default app. That is no worse than today.
4. **Markdown: rendered by default with a persisted Source toggle (recommended), vs source only first.** Worst case: a rendering gap (unsupported construct, GPUI list/`InteractiveText` not as assumed) makes a file look wrong; the toggle is the escape. The render cap (512 KiB) bounds slowness.
5. **Markdown crate: pulldown-cmark (recommended), vs comrak.** pulldown-cmark is smaller with an event model that suits direct mapping. Worst case: a licence or advisory finding in `cargo deny check`, or a bigger exe than expected; both are measured in S5 before the view is built.
6. **Selection: line-granular first (recommended), vs character selection via read-only `TextInput`.** Worst case: users want to copy half a line and cannot; Copy all and the later char selection cover it.
7. **Executable-extension files (`.js .py .sh .ps1 .bat`): view them (recommended), vs send to the default app as now.** Worst case: someone expects a double-click to run; the button still asks to run, so nothing runs unconfirmed.
8. **Where the extension -> language map lives: client module (recommended), vs move `language_for_path` into `crates/protocol`.** Worst case of the client copy: two lists drift; the test `every_language_id_has_a_grammar` and a comment name the daemon list.

## DOC DRIFT

- `crates/protocol/src/lib.rs:1327-1336` and `:2404-2406` call the diff tab "Monaco-backed" and `GetFileSnapshot`'s payload what "Monaco's DiffEditor needs". The Tauri/Monaco app is retired and the native client uses `similar` (`docs/native-client.md:174`). Code comment is stale; owner of the fix: the protocol crate's doc comments. Same for `language` "set Monaco's model language" at `:3426-3429` and `git_inspect.rs:426` ("Monaco language id").
- `crates/protocol/src/lib.rs:1393-1395` says "legacy `state.json` files … still load", against CLAUDE.md "No backward compatibility before the first release". Code comment is stale; fix in the protocol crate.
- `docs/native-client.md:149` ("A path with `:line` opens in VS Code with `-g`, otherwise in the OS default handler") will be wrong once the viewer lands; S8 owns it.
- `docs/plans/remote-file-transfer.md` Rulings, "Viewing" (`:18`), says the saved file opens "the way a local link click does: `code -g`, otherwise the OS default handler". Same update.
- The brief named `Origin`/`Route` as "if present on main" and RT-35 as "about to land": it has landed (88daeed4), so any plan text that says "no UI trigger calls the sink yet" in `docs/plans/remote-file-transfer.md` Status is still accurate for triggers (FT.3/FT.4 are open), but "the worktree rt35-ft2" is now merged.
- Not checked: `docs/native-client.md:80` says the protocol version is 23; I did not compare it with `protocol-version.json`.

## TASK INDEX GAP

The answer took about 25 reads and searches beyond the Task Index row and `native-client.md`. Missing row:

| Task | Files |
|---|---|
| Change what a path link, a fetched file or an "open" button opens in (default app, VS Code, built-in viewer) | `docs/native-client.md` (Links, Fetched files, Text and Markdown viewer) -> `apps/native/src/links.rs` (candidates) -> `apps/native/src/open.rs` (`resolve_link`, `OpenAction`, `OpenJob`, `Opener`, `runs_code`) -> `apps/native/src/open_view.rs` (`open_link`, `after_resolve`, `dispatch_open`, `run_open`, `open_by`, `Origin`, `Route`) -> `apps/native/src/file_fetch_view.rs` (`open_fetched`, `open_fetched_job`, `is_fetched_copy`) -> `apps/native/src/remote_mode.rs` -> `apps/native/src/viewer_text.rs` (`decide`) -> `apps/native/src/sidebar.rs` (`GeneralSettings`) -> `apps/native/src/settings_view.rs` -> `apps/native/tests/ui/ui_viewer.rs`, `ui_remote.rs`, `ui_file_fetch.rs`, `tests/support/mod.rs` (`OpenRecorder`) |

A second row would help: "Add a tab kind" -> `crates/protocol/src/lib.rs` (`TabContent`, `TabEntry`) -> `crates/daemon/src/tabs.rs`, `server/tabs.rs` -> `apps/native/src/diff_tab_view.rs` (`reconcile_diff_tabs`, the model to copy) -> `grid_view.rs` (`grid_area`) -> `tab_bar.rs`.

## MISSING ENTRY POINT

- `docs/README.md` does not exist (also `docs/agents/explore.md`). CLAUDE.md "Project shape" plays the index role.
- No glossary. Project-sense terms used in code and docs with no entry: "fetched copy" (a file under the `remote-files` root), `Origin` / `Route` (open_view.rs), "remote client" vs "host" (defined only in `docs/plans/remote-file-transfer.md`), "reconcile" (diff tabs).
- `docs/architecture.md` has a Task Index (l.69), so that part is fine.

## UNVERIFIED (said so where it matters)

- `gpui::InteractiveText` and a variable-height `list` in GPUI 0.2.2.
- pulldown-cmark's size and deny result.
- How the daemon's 20 `TabContent::` sites behave (counted, not read).
- RT-325 and RT-324 contents.
- Whether the diff view handles long lines by clipping or scrolling.
