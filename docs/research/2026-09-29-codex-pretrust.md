# Pre-trusting a folder and skipping the Windows sandbox chooser when spawning Codex

Researched 2026-09-29 against openai/codex tag `rust-v0.159.2` (commit `ff6aec96948b`, the latest release, published 2026-09-29). All source links below are pinned to that tag. `SRC` = `https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs`.

## Question

The daemon spawns `codex` (interactive TUI) in a PTY, usually with `--dangerously-bypass-approvals-and-sandbox` (`--yolo`). Codex still shows a "trust this folder?" screen, and for a repo opened through a `subst` drive it shows it again on every spawn and every resume. On a first Windows run it also shows a sandbox chooser. We want a per-spawn flag, `-c` override or env var that (1) pre-trusts the working folder and (2) skips the sandbox chooser, without editing `~/.codex/config.toml`.

## Answer

No flag skips the trust screen. `--yolo` only sets `approval_policy = never` and `sandbox_mode = danger-full-access`; the trust check never looks at either. A `-c` override of the `projects` table does work: the TUI's trust check reads the merged config, and `-c` overrides are merged into it.

The Windows sandbox prompt shown at startup only appears when a trust decision was *persisted during this launch*. So pre-trusting the folder also stops that prompt from appearing at startup. `-c windows.sandbox=...` is an optional second guard.

Recommended per-spawn args (new session and resume alike). `<CWD>` is the exact folder string the daemon passes to `-C`:

```
codex --dangerously-bypass-approvals-and-sandbox -C "<CWD>" -c "projects={'<CWD>'={trust_level='trusted'}}"
codex resume <SESSION_ID> --dangerously-bypass-approvals-and-sandbox -C "<CWD>" -c "projects={'<CWD>'={trust_level='trusted'}}"
```

Optional, only if the startup sandbox prompt is still seen: `-c "windows.sandbox='unelevated'"`. This marks the Windows sandbox as configured, but it also picks the unelevated sandbox for any non-yolo turn in that session.

Rules for building the override:

- **Put the path in the value, not in the dotted key.** `-c` keys are split on every `.`, and quotes in the key are not parsed. `-c projects."X:\dev\repo".trust_level=trusted` creates a key whose name includes the `"` characters, so it never matches. The unquoted form `-c projects.X:\dev\repo.trust_level=trusted` does work, but breaks on any path that contains a `.` (and on a `=` before the value).
- **The value is parsed as TOML.** Write the path as a TOML literal string (`'X:\dev\repo'`), so backslashes need no escaping and the argument contains no `"`. A path that itself contains `'` must use a basic string with every `\` doubled and every `"` escaped instead (for example `"X:\\dev\\repo"`). Upstream's own test builds that form with `serde_json::to_string(path)`.
- **Use the same spelling as the cwd.** The lookup ignores ASCII case on Windows but does not resolve `subst` drives in the spelling it compares first. Pass the same string as `-C`, for example `X:\dev\repo` for a subst drive, not the resolved `D:\...` path.
- `-c projects=...` merges into the user's `[projects]` table rather than replacing it. Nothing is written to disk.

Side effect: any `-c` key other than a short allowlist makes the TUI skip the shared background app-server daemon and run it embedded. Only the embedded path has been traced here, and it is the path these args take.

`codex exec` and `codex exec resume` have no trust screen. Their only gate is a git-repo check, which `--yolo` or `--skip-git-repo-check` already skips.

Confidence: high for the trust mechanism, the `-c` parsing and the embedded-mode path; all three were traced end to end in source. Medium for the sandbox prompt suppression: it was traced in source but not run on Windows. The claim that Windows `canonicalize` resolves `subst` drives is inferred from Windows API behaviour and matches what we observed; the Codex source does not show it.

## Evidence

### 1. Where the trust screen is decided

- Startup calls `check_directory_trust(...)` with `trust_cwd = config.cwd` (or `--cd` in remote mode) after the resume or fork destination is resolved: [`tui/src/lib.rs` L1804-L1833](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/lib.rs#L1804-L1833). The old onboarding-time trust step is now hard-wired off (`should_show_trust_screen_flag = false`, [L1258-L1259](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/lib.rs#L1258-L1259)).
- `check_directory_trust` ([`tui/src/onboarding/directory_trust.rs` L33-L159](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/onboarding/directory_trust.rs#L33-L159)) works as follows:
  - **Embedded app server (our case):** if `config.active_project.trust_level == Some(TrustLevel::Trusted)` it returns no project and shows no screen (L69-L70). Otherwise it builds a `TrustDirectoryWidget` (L103-L120). An explicitly `untrusted` entry still shows the screen in restricted mode.
  - **Connected to a daemon or remote server:** it asks the server through `read_remote_project_trust` (L61-L68, [`tui/src/config_update.rs` L197-L380](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/config_update.rs#L197-L380)).
- `config.active_project` is computed in [`core/src/config/mod.rs` L3429-L3457](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/core/src/config/mod.rs#L3429-L3457). The cwd goes through `normalize_for_native_workdir`, which is `dunce::simplified` only (no canonicalization, so a subst drive letter is kept; [`utils/path-utils/src/lib.rs` L54-L56, L162-L168](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/utils/path-utils/src/lib.rs#L54-L56)). Then `cfg.get_active_project(resolved_cwd, repo_root)` looks up the cwd first and the git repo root second ([`config/src/config_toml.rs` L874-L891](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/config_toml.rs#L874-L891)).
- The config key is `projects."<path>".trust_level = "trusted" | "untrusted"`. `ProjectConfig { trust_level }` is deserialized from the merged `projects` map.

**How the lookup key is normalized** ([`config/src/project_trust.rs` L27-L104](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/project_trust.rs#L27-L104)):

- Each path gives two candidate spellings: a *canonical* one from `normalize_for_path_comparison` and the *original* one. They are tried in that order, the cwd's before the repo root's (L28-L39, L50-L71).
- `normalize_for_path_comparison` is plain `std::fs::canonicalize` plus WSL folding ([`utils/path-utils/src/lib.rs` L18-L21](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/utils/path-utils/src/lib.rs#L18-L21)). On Windows `std::fs::canonicalize` returns the extended-length form `\\?\D:\...`. The doc comment at `project_trust.rs` L46-L49 says the canonical spelling includes "Windows extended-length prefixes".
- On Windows, keys are compared case-insensitively: they are ASCII-lowercased, and config keys that differ only in case are matched too (L81-L104; test at [`config/src/project_trust_tests.rs` L73-L91](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/project_trust_tests.rs#L73-L91)).

**How the key is written** when the user picks "Trust" (embedded): `write_trusted_project`, then `trusted_project_edit`, then `project_trust_key(path)` ([`tui/src/onboarding/onboarding_screen.rs` L787-L817](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/onboarding/onboarding_screen.rs#L787-L817); [`tui/src/config_update.rs` L76-L82](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/config_update.rs#L76-L82)). `project_trust_key` is `dunce::canonicalize` followed by lowercasing on Windows ([`config/src/loader/mod.rs` L60, L1367-L1399](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/loader/mod.rs#L1367-L1399)). The app server's auto-trust uses the same function (see §2).

**Why a subst drive asks every time (inferred):**

- The writer stores `d:\real\dev\repo`: dunce-canonical, which resolves the subst drive and strips `\\?\`.
- The reader tries `\\?\d:\real\dev\repo` (std-canonical, which keeps `\\?\`) and then `x:\dev\repo` (original).
- Neither matches the stored key. On a normal drive the *original* spelling equals the dunce key, which is why only subst and junction paths show the bug.
- The claim that `canonicalize` resolves a subst drive to its backing volume comes from Windows `GetFinalPathNameByHandleW` behaviour, not from Codex source. It agrees with the observed config entries. **Unverified here by running it.**

**`-c` overrides reach this check.**

- `-c` values go into a `SessionFlags` layer that is merged into the final config ([`config/src/loader/mod.rs` L249-L264, L415-L421](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/loader/mod.rs#L249-L264)). They are also merged before project-trust discovery (L336-L379).
- Merging is a deep table merge, so a `-c projects={...}` adds to the user's existing projects ([`config/src/merge.rs` L56-L61, L96-L98](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/merge.rs#L56-L61)).
- Upstream's own CLI test passes `-c projects={<json-string-path>={trust_level="untrusted"}}` and expects it to take effect ([`cli/tests/worktree.rs` L414-L429](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/cli/tests/worktree.rs#L414-L429)).
- The app server itself injects in-memory trust the same way, as a `("projects", Table)` override ([`app-server/src/request_processors/thread_processor.rs` L1383-L1400](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/app-server/src/request_processors/thread_processor.rs#L1383-L1400)).

**`-c` key and value syntax.**

- `parse_overrides` splits on the first `=`. The value is parsed as TOML (`_x_ = <value>`); if that fails, the raw string is used with surrounding quotes trimmed. The key is kept verbatim ([`utils/cli/src/config_override.rs` L49-L102](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/utils/cli/src/config_override.rs#L49-L102)).
- `apply_toml_override` splits the key with `path.split('.')`. There is no TOML key parsing, so quoted segments keep their quote characters ([`config/src/overrides.rs` L17-L25](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/overrides.rs#L17-L25)). That is why the path belongs in the value's inline table.
- Unverified: whether a malformed inline table (one that falls back to a string) fails config load loudly or silently. Deserializing `projects` as a string should fail with a type error, but this was not run.

**Daemon vs embedded.**

- `daemon_startup::config_exclusion` turns off reuse of the shared local daemon for any `-c` key outside a small allowlist (`tui.fullscreen_transcript`, some `features.*`, …) ([`tui/src/daemon_startup.rs` L25-L89](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/daemon_startup.rs#L25-L89)).
- With that exclusion `app_server_target_for_launch` returns `Embedded` ([`tui/src/lib.rs` L1013-L1042](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/lib.rs#L1013-L1042); [`tui/src/startup_orchestration.rs` L176-L183, L302-L315](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/startup_orchestration.rs#L176-L183)). `--no-daemon` does the same explicitly.
- The embedded app server gets the same `-c` overrides ([`app-server/src/in_process.rs` L143, L430](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/app-server/src/in_process.rs#L143); `config_manager.rs` loads with `current_cli_overrides()`, [L116-L119, L429-L457](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/app-server/src/config_manager.rs#L429-L457)).

### 2. No flag skips the trust screen; why `--yolo` does not

- `--yolo` is an alias of `--dangerously-bypass-approvals-and-sandbox` ([`utils/cli/src/shared_options.rs` L52-L59](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/utils/cli/src/shared_options.rs#L52-L59)). In the TUI it only sets `approval_policy = Never` and `sandbox_mode = DangerFullAccess` ([`cli/src/main.rs` L1988-L1997](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/cli/src/main.rs#L1988-L1997)). `check_directory_trust` reads neither.
- `--full-auto` / `--approve-for-me`, `-s` and `-a` are the same kind of setting (approval and sandbox), with no trust input.
- `--skip-git-repo-check` exists only on `codex exec` ([`exec/src/lib.rs` L975-L983](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/exec/src/lib.rs#L975-L983)).
- `--dangerously-bypass-hook-trust` affects hook trust only.
- What `--yolo` *does* cause: once the thread starts, the app server sees full-access permissions with no trust entry and **persists** trust to `config.toml` with `set_project_trust_level`, which uses `project_trust_key`, the dunce-canonical key ([`thread_processor.rs` L1350-L1410](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/app-server/src/request_processors/thread_processor.rs#L1350-L1410); [`core/src/config/mod.rs` L2350](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/core/src/config/mod.rs#L2350)). That runs after the TUI trust screen, and for subst paths it writes the entry that never matches (see §1). With the `-c` pre-trust, `active_project.trust_level` is already set, so this write is skipped (condition at L1363-L1367), and the global config stops collecting subst-resolved entries.

### 3. The Windows sandbox chooser

- **The startup prompt.** `should_prompt_windows_sandbox_nux_at_startup = trust_decision_was_made` ([`tui/src/lib.rs` L1954-L1958](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/lib.rs#L1954-L1958)). The flag is set only when the trust screen persisted a decision in this launch (L1348-L1352, L1872-L1875). It is passed on as `WindowsSandboxState.prompt_after_trust` ([`tui/src/app/startup.rs` L805-L808](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/app/startup.rs#L805-L808)).
- **When it opens.** In `refresh_windows_sandbox_for_thread`: `show_nux = (prompt_after_trust && !config.is_enabled()) || config.requires_elevated()`, followed by `maybe_prompt_windows_sandbox_enable(...)` ([`tui/src/app/platform_actions.rs` L106-L112](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/app/platform_actions.rs#L106-L112); [`tui/src/chatwidget/windows_sandbox_prompts.rs` L276-L287](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/chatwidget/windows_sandbox_prompts.rs#L276-L287)). The trust widget's own sandbox hint is hard-wired off (`show_windows_create_sandbox_hint: false`, `directory_trust.rs` L115).
- **Config key that records the choice.** `windows.sandbox = "elevated" | "unelevated" | "mxc"` ([`config/src/types.rs` L164-L176](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/config/src/types.rs#L164-L176)). The TUI reads it from the effective config over `config/read`, and falls back to the legacy flags `features.elevated_windows_sandbox`, `features.experimental_windows_sandbox` and `features.enable_experimental_windows_sandbox` ([`tui/src/windows_sandbox.rs` L23-L63](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/windows_sandbox.rs#L23-L63)). `is_enabled()` is true once any mode is set (L99-L101).
- **Pre-answering it.** With a pre-trusted folder, `trust_decision_was_made` stays false, so `show_nux` is false unless `requires_elevated()` is true. That needs `windows.sandbox = "elevated"` *and* managed requirements that constrain the implementations (L119-L122).
  - `-c windows.sandbox='unelevated'` also makes `is_enabled()` true. Because `config/read` on the embedded server includes the `-c` layer, this should suppress the prompt even when a trust decision is made. That last step (the `config/read` response reflecting `SessionFlags`) was inferred from the config manager loading with CLI overrides, not run.
  - `windows.sandbox` is not on the daemon-reuse allowlist, so it also forces embedded mode.
- The prompt can still be opened on purpose later (for example by switching permission presets): [`tui/src/app/event_dispatch.rs` L2180, L2200](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/app/event_dispatch.rs#L2180).

### 4. `codex resume` / `codex exec resume`

- `codex resume <id>` takes the full `TuiCli`, including `-C`, `--yolo` and `-c` ([`cli/src/main.rs` L349-L373, L437-L457](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/cli/src/main.rs#L349-L373); `-c` is `global = true`, [`utils/cli/src/config_override.rs` L29-L36](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/utils/cli/src/config_override.rs#L29-L36)).
- It goes through the same startup `check_directory_trust` (`lib.rs` L1804-L1833). Before that, `resolve_startup_resume_or_fork_cwd` may show a *second* prompt, "resume in session folder or current folder?", when the recorded session cwd differs from the current one ([`tui/src/lib.rs` L931-L987](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/lib.rs#L931-L987); [`tui/src/session_resume.rs` L61-L116](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/session_resume.rs#L61-L116)). Passing `-C` sets the mode to `Current` and skips that prompt ([`session_resume.rs` L34-L43](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/session_resume.rs#L34-L43)). The trust check then uses the `-C` folder, which is why the recommended resume args include `-C`.
- In-app resume (picking a session inside the TUI) calls the same check through `confirm_directory_trust` ([`tui/src/app/resume_config.rs` L132-L178](https://github.com/openai/codex/blob/rust-v0.159.2/codex-rs/tui/src/app/resume_config.rs#L132-L178)). There the folder is the session's cwd, so a session recorded under a different spelling can still prompt.
- `codex exec` and `codex exec resume` never show a trust screen: nothing in `exec/src` references `TrustDirectory`, `check_directory_trust` or `trust_level`. They only have the git-repo gate (`exec/src/lib.rs` L975-L983).

### 5. Version and recent changes

- Latest release: `rust-v0.159.2` (2026-09-29). Everything above is from that tag.
- The trust lookup was rewritten recently:
  - #47620 "Add portable project trust lookup APIs" (2026-09-23).
  - #47924 "Make project trust lookup paths explicit and defer root resolution" (merged 2026-09-24).
  - Both shipped in 0.158.0 ([release notes](https://github.com/openai/codex/releases/tag/rust-v0.158.0)). They introduced the std-canonical `\\?\` key vs dunce-written key split described in §1 (inferred from the current code; the pre-0.158 lookup was not diffed).
- Folder consent moved after destination resolution in #44746 and #44755 (2026-09-11).
- 0.159.0 added "restrictive launchers can fall back to embedded mode" (#48491).
- No open upstream issue for the subst/`\\?\` trust mismatch was found (`gh search issues` for "trust subst" and "subst drive" returned only the unrelated #27243, about `\\?\` in thread cwds).

### Unverified / to check on a real spawn

- **Argument passing.** The daemon must pass the `-c` argument as one argv element. The single-quoted TOML form contains no `"`, so standard Windows argv quoting only has to handle spaces. If `codex` resolves to the npm `codex.cmd` shim rather than `codex.exe`, cmd.exe re-parses the line. Its metacharacters (`& | < > ^ %`) do not appear in the recommended form, but this was not tested.
- **The subst mechanism in §1** was inferred, not reproduced.
- **Sandbox prompt suppression in §3** was traced in source, not run on Windows.
