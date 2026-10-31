# Release procedure

How a versioned release of rustling-tulip is cut, starting with `v0.1.0-alpha`. The work a release ships is chosen in Linear through the release pipeline: everything in `vNext` (Linear team RT) ships in the next release, and `vNext+1` holds what is planned after it.

## What a release publishes

- A git tag `v<version>` on `main` (`v0.1.0-alpha`, `v0.2.0-alpha`, …).
- A GitHub release of that tag, marked pre-release while the version carries a pre-release suffix, with the Windows installer and its SHA-256 file attached: `rustling-tulip-<version>-setup.exe` and `rustling-tulip-<version>-setup.exe.sha256` (a release missing either is never offered as an update), built by `.\rt.ps1 installer` (NSIS, per-user, unsigned, so SmartScreen warns; see `tools/installer/README.md`).
- The version lives in one place, `[workspace.package] version` in `Cargo.toml`, which every crate inherits; the installer reads it from there.

## Steps

1. **Scope.** `uv run --project ~/.claude/tools/linear python -m linear release status rustling-tulip`: every issue in `vNext` is Done or Canceled. An open one either lands first or moves out with `linear release add rustling-tulip --next <ID>`.
2. **Version.** Set `[workspace.package] version` in `Cargo.toml` to the release's version (`0.1.0-alpha`), run `cargo update --workspace` so `Cargo.lock` follows, rename `CHANGELOG.md`'s `## Unreleased` to `## v<version>` with a new empty `## Unreleased` above it, and commit them as `chore: release v<version>`.
3. **Gates on `main`'s head**, each through the gate wrapper:
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-fmt.log --timeout 600 --slot light -- cargo fmt --all --check`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-clippy.log --timeout 3600 --slot heavy -- cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-deny.log --timeout 600 --slot light -- cargo deny check`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-suite.log --timeout 3600 --slot heavy -- cargo nextest run --workspace --cargo-profile test-release`
   - `.\rt.ps1 native-e2e` and `.\rt.ps1 native-smoke`

   A red gate stops the release; its fix lands first.
4. **Smoke-test every new feature.** Before the cut, each bullet under the release's `### Added` in `CHANGELOG.md` gets an agent-run smoke test of the built app in its real runtime, beyond the test suites: an agent drives the feature end to end the way a user reaches it (the native client over a scripted or real daemon, `.\rt.ps1 native-shot` for what it draws, and a real second machine for anything remote, such as `builder@leftos-buildbox` over SSH or the tailnet) and records what it saw. A feature with no smoke run, or one that misbehaves, stops the release like a red gate: the fix lands first, or the feature leaves the release with the owner's say. Isolate every run from the owner's own daemon and sessions (CLAUDE.md "Traps").
5. **Installer.** `.\rt.ps1 installer`, then install it on this PC and run the hand-test list in `tools/installer/README.md`. It also writes `target/installer/rustling-tulip-<version>-setup.exe.sha256` (`<hex>  <file name>`), which the app's updater checks every download against ([auto-update.md](./auto-update.md)).
6. **README.** Refresh `README.md`'s Features and What's new sections from the release's notes (new features in, removed ones out), and re-shoot any screenshot whose view changed with `.\rt.ps1 native-shot <view>` into `docs/images/`; commit it as `docs: refresh the README for v<version>`.
7. **Tag and push.** `git tag -a v<version> -m "rustling-tulip v<version>"` on the commit the gates ran on, then `git push origin main v<version>`.
8. **Publish.** `gh release create v<version> target/installer/rustling-tulip-<version>-setup.exe target/installer/rustling-tulip-<version>-setup.exe.sha256 --repo leftos/rustling-tulip --title "rustling-tulip v<version>" --notes-file <notes> --prerelease` (drop `--prerelease` for a version with no suffix). The notes are the version's `CHANGELOG.md` section, checked against the issues `linear release status` lists in `vNext`.
9. **Linear.** Once the release is public: `uv run --project ~/.claude/tools/linear python -m linear release complete rustling-tulip --version v<version> --sha <tag commit> --repo-root .`. It files every shipped Done issue under the new release and moves `vNext+1` up into `vNext`. Never run it twice for one version; a cut that missed issues is fixed with `linear release settle`.
10. **After the first release** the repo's test mode turns `stable` (the `rustling-tulip-nextup` profile's `test mode:` line and CLAUDE.md), so every landed change runs the full suite from then on.
