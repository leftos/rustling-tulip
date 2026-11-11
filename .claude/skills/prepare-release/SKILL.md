---
name: prepare-release
description: Use when cutting a rustling-tulip release, when the owner invokes /prepare-release or a /nextup run finds every item of vNext (or vHotfix) Done. Walks scope, version, changelog, gates, feature smoke tests, README, the signed Windows and macOS builds, tag, GitHub release, the Linear cut and the next release's scope.
---

Cut and publish a rustling-tulip release from the repo root. Walk the phases in order; each ends on its **Done when** line. A red anywhere stops the cut until its fix lands as an ordinary item, and the cut resumes at Phase 4 on the new head.

What a release publishes (the tag, the five assets and their names, the notes) and the breaking-flag marker are reference in [docs/release.md](../../../docs/release.md): read it before Phase 3.

**Ground rules for every phase:**

- `linear` is the command `~/.claude/docs/plan-operations.md` defines (`uv run --project ~/.claude/tools/linear python -m linear`).
- Every bare cargo command runs through `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-<step>.log --timeout <n> --slot <heavy|light> -- <command>`, one gate call per step. An `rt.ps1` verb runs bare (`pwsh -NoProfile -File ./rt.ps1 <verb>`), since it gates its own steps (CLAUDE.md, "Common commands"). A verdict is read unpiped.
- A commit message is written with Write to `.tmp/msg-<slug>.txt`, a file new to that commit, and committed in a later turn through the gate (`--slot light --timeout 1200`), naming its files in the pathspec.
- **Who decides.** When the owner invoked the skill, the version (Phase 2) and the changelog audit (Phase 3) go to them with `AskUserQuestion`. Inside a `/nextup` run, which cuts without asking (the `rustling-tulip-nextup` profile, "Releases"), the orchestrator decides both by the same rules and names what decided them at its checkpoint. A step that needs the owner's hands (an Azure sign-in, the Mac mini, a feature leaving the release) is asked at once either way.

## Phase 1: Scope

1. `git fetch origin`, then `git status -sb`: on `main`, clean, level with `origin/main`.
2. `linear release status rustling-tulip`. Every issue in `vNext` is Done or Canceled; an open one lands first, or moves out with `linear release add rustling-tulip --next <ID>` on the owner's say.

**Done when** the tree is level with `origin/main` and `vNext` holds no open issue.

## Phase 2: Version

1. Find the last release with `git tag --sort=-v:refname | head -3`, and choose `<version>` by the **release** row of `plan-operations.md`: the next minor (`0.12.0-alpha` → `0.13.0-alpha`) when the release adds a feature or changes behaviour on purpose, the next patch for fixes alone. The version carries no `v`; the tag is `v<version>`.
2. Set `[workspace.package] version` in `Cargo.toml`, the one home every crate inherits, then run `cargo update --workspace` through the gate (`--slot light`) so `Cargo.lock` follows.

**Done when** `Cargo.toml` and every workspace crate in `Cargo.lock` read `<version>`, and both `git tag -l v<version>` and `git ls-remote --tags origin v<version>` print nothing.

## Phase 3: Changelog

1. The topmost heading of `CHANGELOG.md` is `## Unreleased` with bullets under it. A released version's heading on top means the changelog is stale: write the missing bullets from `git log v<last>..HEAD --oneline` by `changelog-and-commit` first.
2. **Audit the section** as the difference from the last release (`changelog-and-commit`, "iterate mode" and "Write the bullets", with the profile's `## Changelog`):
   - A `### Fixed` bullet that fixes something added or changed in this same section is not news: fold what it makes true into that feature's bullet and drop it.
   - Two bullets on one feature's successive states fold into the state that ships.
   - Every bullet is one sentence of 25 words at most, in the app's own words, with no issue number, crate or type name.
   - Every Done issue in `vNext` with a user-visible change has its bullet.

   Owner-invoked, show the folds and drops as a short list (drop, fold into which bullet, keep) and apply what they agree to.
3. **Mark what it breaks.** Run `git diff v<last> -- protocol-version.json crates/tracer-protocol` and read the commits for a session sidecar change. A `supported` range that drops a version the last release spoke, a tracer ABI bump, or anything else that ends live sessions across an update puts `<!-- breaks: sessions, protocol -->` (the words that apply) as the section's first line (`docs/release.md`, "Mark what a release breaks").
4. **Highlights.** A section of more than four bullets opens with `### Highlights` before `### Added`: up to four bullets, biggest first, saying only what the bullets under it say.
5. Rename `## Unreleased` to `## v<version>` and open a new empty `## Unreleased` above it.
6. Commit `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md` as `chore: release v<version>`.

**Done when** `git log -1 --oneline` shows the release commit and every audit point above holds for the section.

## Phase 4: Gates

On `main`'s head, one at a time:

1. `cargo fmt --all --check` through the gate (`--slot light --timeout 600`).
2. `pwsh -NoProfile -File ./rt.ps1 clippy`: the agent seam check, then clippy with `-D warnings`.
3. `cargo deny check` through the gate (`--slot light --timeout 600`).
4. `pwsh -NoProfile -File ./rt.ps1 test`: the full suite, the driver specs and the phone app's checks.
5. `pwsh -NoProfile -File ./rt.ps1 native-e2e`, then `pwsh -NoProfile -File ./rt.ps1 native-smoke`.

**Done when** every step's verdict is passed on the same head.

## Phase 5: Smoke-test every new feature

Each bullet under the section's `### Added` gets an agent-run smoke test of the built app in its real runtime, beyond the suites: an agent drives the feature end to end the way a user reaches it (the native client over a scripted or real daemon, `.\rt.ps1 native-shot` for what it draws, and a real second machine for anything remote, such as `builder@leftos-buildbox` over SSH or the tailnet) and reports what it saw.

Every run is isolated from the owner's own daemon and sessions by the isolated dirs the live tier uses (`docs/testing.md`; all of `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` under `.tmp`). A feature that misbehaves stops the cut like a red gate: its fix lands, or the feature leaves the release on the owner's say and its bullet goes with it.

**Done when** every `### Added` bullet has a passing smoke run on the current head, each recorded as one line: what was driven, and what it showed.

## Phase 6: README

Refresh `README.md`'s Features and What's new sections from the release's section (new features in, removed ones out), and re-shoot every screenshot whose view changed with `.\rt.ps1 native-shot <view>` into `docs/images/`. Commit them as `docs: refresh the README for v<version>`.

**Done when** each README claim holds at `<version>` and every changed view's screenshot is re-shot.

## Phase 7: Build

The assets are built from `HEAD`, which is the commit Phase 8 tags: nothing lands from here to the tag, and a fix that must land sends the cut back to Phase 4.

1. **Windows.** `pwsh -NoProfile -File ./rt.ps1 installer`, signed in to Azure and never with `-Unsigned` (prerequisites: `tools/installer/README.md`, "Code signing"). It also builds the phone app into the install folder, so it needs pnpm. `Get-AuthenticodeSignature target\installer\rustling-tulip-<version>-setup.exe` must show `Valid` with a timestamp. A release runs no hand test of the installer.
2. **macOS.** `pwsh -NoProfile -File tools/installer/pack-macos-signed.ps1`. The pack refuses unless codesign, spctl and stapler pass, so check only that the dmg, zip and `.zip.sha256` are back in `target/installer` and that the `.zip.sha256` names the zip. When the Mac mini is unreachable, ask the owner whether the release goes out Windows-only, its macOS assets added later with `gh release upload v<version> <the three files> --repo leftos/rustling-tulip`.

**Done when** the setup exe (signature `Valid`), its `.sha256`, and the macOS dmg, zip and `.zip.sha256` are in `target/installer`, each `.sha256` naming its file, or the owner has approved a Windows-only release.

## Phase 8: Tag and push

`git tag -a v<version> -m "rustling-tulip v<version>"` on `HEAD`, then `git push origin main v<version>` bare: its pre-push clippy hook gates itself.

**Done when** `git ls-remote --tags origin v<version>` shows the tag and `git rev-parse "v<version>^{commit}"` is `HEAD`.

## Phase 9: Publish

1. Write the version's `CHANGELOG.md` section without its `## v<version>` heading to `.tmp/release-notes-v<version>.md`, the breaking marker kept as its first line.
2. `gh release create v<version> target/installer/rustling-tulip-<version>-setup.exe target/installer/rustling-tulip-<version>-setup.exe.sha256 target/installer/rustling-tulip-<version>-macos-arm64.dmg target/installer/rustling-tulip-<version>-macos-arm64.zip target/installer/rustling-tulip-<version>-macos-arm64.zip.sha256 --repo leftos/rustling-tulip --title "rustling-tulip v<version>" --notes-file .tmp/release-notes-v<version>.md --latest`. Every build is an alpha, and the pre-release flag would hide the newest from the repo's home page, so the release is always a regular one.

**Done when** `gh release view v<version> --repo leftos/rustling-tulip --json isLatest,isPrerelease,assets` shows it latest, not pre-release, with the five assets (two on an approved Windows-only release).

## Phase 10: Linear cut

1. `linear release complete rustling-tulip --version v<version> --sha <tag commit> --repo-root .`, where `<tag commit>` is `git rev-parse "v<version>^{commit}"`. Run it once per version; shipped issues it left out go through `linear release settle`, as the **release** row of `plan-operations.md` gives.
2. Take the plan **snapshot**: `linear snapshot rustling-tulip --repo-root .`, committed on its own as `docs: plan snapshot`, and push it.

**Done when** the completed release `v<version>` holds every shipped issue and the snapshot is on `origin/main`.

## Phase 11: Next release

Run "Plan the next release with the owner" from the **release** row of `plan-operations.md`: one `AskUserQuestion` round, applied with `linear release add`, ending with `linear release planned rustling-tulip --repo-root .`. When the owner is away, the `pendingReleasePlan` note stays and the next `/nextup` plans `vNext` first.

**Done when** every candidate group is in `vNext`, in `vNext+1` or in no release, as the owner answered.

## Hotfix

A hotfix follows `plan-operations.md`, "Hotfix": Phase 1 reads `vHotfix` in place of `vNext`, the phases run on `hotfix/v<version>` cut from the last tag with each item cherry-picked (`git cherry-pick -x`, oldest first), Phase 8 pushes that branch's tag, and Phase 10 adds `--hotfix` to `release complete`. The branch is deleted locally and on the remote once the tag is pushed.

Back on `main`, the hotfix's section is recorded in `CHANGELOG.md` below `## Unreleased`, and each shipped bullet moves out of `## Unreleased` into it, so the next release does not list it again; commit it as `docs: record the v<version> hotfix in the changelog`. Phase 11 follows a plain cut only.
