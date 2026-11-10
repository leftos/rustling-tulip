# Release procedure

How a versioned release of rustling-tulip is cut, starting with `v0.1.0-alpha`. The work a release ships is chosen in Linear through the release pipeline: everything in `vNext` (Linear team RT) ships in the next release, and `vNext+1` holds what is planned after it.

## What a release publishes

- A git tag `v<version>` on `main` (`v0.1.0-alpha`, `v0.2.0-alpha`, …).
- A GitHub release of that tag, never marked pre-release (every build is an alpha, and the flag would hide the newest one from the repo's home page) and marked Latest, with the Windows installer and its SHA-256 file attached: `rustling-tulip-<version>-setup.exe` and `rustling-tulip-<version>-setup.exe.sha256` (a release missing either is never offered to a Windows client as an update), built by `.\rt.ps1 installer` (NSIS, per-user, code-signed with Azure Artifact Signing; see `tools/installer/README.md`).
- The macOS (Apple silicon) assets: `rustling-tulip-<version>-macos-arm64.dmg` (what a person downloads), and `rustling-tulip-<version>-macos-arm64.zip` with `rustling-tulip-<version>-macos-arm64.zip.sha256` (what the app's updater downloads and checks on a Mac). They are signed with the Developer ID, notarized and stapled, built by `tools/installer/pack-macos-signed.ps1` (`tools/installer/README.md` "macOS code signing").

  A release missing the macOS pair stays valid for Windows and is simply not offered to a Mac as an update ([auto-update.md](./auto-update.md)).
- The version lives in one place, `[workspace.package] version` in `Cargo.toml`, which every crate inherits; the installer reads it from there.

## Mark what a release breaks

A release that breaks live sessions or the protocol says so in a marker line, which the app reads from the release's notes on the feed (`update_core::breaks_of`): `<!-- breaks: sessions, protocol -->`, with either word or both. It is the first line of the version's `CHANGELOG.md` section, right under its `## v<version>` heading, so it reaches the notes file the publish step uploads; the app leaves the line out of the notes it shows.

- `sessions`: an update to this release ends the live sessions on the machine it updates: a tracer ABI break ([tracer-abi.md](./tracer-abi.md)), a session sidecar change an older daemon's sessions cannot be read back from, or anything else that ends live sessions across an update.
- `protocol`: `protocol-version.json`'s `supported` range drops a version the last release spoke, so a client of the last release can no longer connect to this one.

A release with no marker breaks nothing. The Update host dialog warns from the markers of every release between the host's version and the newest, and after an update that breaks either, the app that asked updates itself right after the host when its own release check succeeded and it is older than the newest release.

Before publishing, check what changed since the last release: `git diff <last tag> -- protocol-version.json crates/tracer-protocol`. A change to `supported` that drops a version, or a tracer ABI bump, needs its word in the marker.

## Steps

1. **Scope.** `uv run --project ~/.claude/tools/linear python -m linear release status rustling-tulip`: every issue in `vNext` is Done or Canceled. An open one either lands first or moves out with `linear release add rustling-tulip --next <ID>`.
2. **Version.** Set `[workspace.package] version` in `Cargo.toml` to the release's version (`0.1.0-alpha`), run `cargo update --workspace` so `Cargo.lock` follows, rename `CHANGELOG.md`'s `## Unreleased` to `## v<version>` with a new empty `## Unreleased` above it, open the section with `### Highlights` (up to 4 bullets, before `### Added`) when it has more than 4 bullets, and commit them as `chore: release v<version>`.
3. **Gates on `main`'s head**, each through the gate wrapper:
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-fmt.log --timeout 600 --slot light -- cargo fmt --all --check`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-clippy.log --timeout 3600 --slot heavy -- cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-deny.log --timeout 600 --slot light -- cargo deny check`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-suite.log --timeout 3600 --slot heavy -- cargo nextest run --workspace --cargo-profile test-release`
   - `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/release-driver.log --timeout 2400 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --features driver -E "test(/^driver::/) | test(/^ui_driver_registry::/)"` (the driver specs, which compile only with the off-by-default `driver` feature)
   - `.\rt.ps1 native-e2e` and `.\rt.ps1 native-smoke`

   A red gate stops the release; its fix lands first.
4. **Smoke-test every new feature.** Before the cut, each bullet under the release's `### Added` in `CHANGELOG.md` gets an agent-run smoke test of the built app in its real runtime, beyond the test suites: an agent drives the feature end to end the way a user reaches it (the native client over a scripted or real daemon, `.\rt.ps1 native-shot` for what it draws, and a real second machine for anything remote, such as `builder@leftos-buildbox` over SSH or the tailnet) and records what it saw. A feature with no smoke run, or one that misbehaves, stops the release like a red gate: the fix lands first, or the feature leaves the release with the owner's say. Isolate every run from the owner's own daemon and sessions (the isolated dirs the live tier uses, [testing.md](testing.md)).
5. **Installer.** `.\rt.ps1 installer`; a release runs no hand test of it. It also builds the phone app (`apps/pwa`) and ships it as the install folder's `pwa`, so the PC needs Node.js and pnpm (`.\rt.ps1 setup` installs both) and the step fails without them. The step signs the three binaries and the setup exe (prerequisites: `tools/installer/README.md` "Code signing"), so run it signed in to Azure, never with `-Unsigned`. Before publishing, `Get-AuthenticodeSignature target\installer\rustling-tulip-<version>-setup.exe` must show `Valid` with a timestamp, so a test build made with `-Unsigned` (same file name) is never uploaded. It also writes `target/installer/rustling-tulip-<version>-setup.exe.sha256` (`<hex>  <file name>`), which the app's updater checks every download against ([auto-update.md](./auto-update.md)).
6. **macOS assets.** `pwsh -NoProfile -File tools/installer/pack-macos-signed.ps1` on this PC. It reads the signing set from Private Config into its own process only and runs the signed pack on `builder@mac-mini` as one pinned gate run; the three assets come back to `target/installer`. The Mac mini needs Node and pnpm, since the pack builds the phone app into the bundle.

   The pack itself refuses unless `codesign --verify`, `spctl` (app: `accepted`, `source=Notarized Developer ID`; dmg: `accepted`) and `stapler validate` pass on both, so a release only checks that the three files exist and that the `.zip.sha256` names the `.zip`.

   If the Mac mini is unreachable, the release goes out with the Windows assets only, and the macOS assets are added later with `gh release upload v<version> <the three files> --repo leftos/rustling-tulip`.
7. **README.** Refresh `README.md`'s Features and What's new sections from the release's notes (new features in, removed ones out), and re-shoot any screenshot whose view changed with `.\rt.ps1 native-shot <view>` into `docs/images/`; commit it as `docs: refresh the README for v<version>`.
8. **Tag and push.** `git tag -a v<version> -m "rustling-tulip v<version>"` on the commit the gates ran on, then `git push origin main v<version>`.
9. **Publish.** `gh release create v<version> target/installer/rustling-tulip-<version>-setup.exe target/installer/rustling-tulip-<version>-setup.exe.sha256 target/installer/rustling-tulip-<version>-macos-arm64.dmg target/installer/rustling-tulip-<version>-macos-arm64.zip target/installer/rustling-tulip-<version>-macos-arm64.zip.sha256 --repo leftos/rustling-tulip --title "rustling-tulip v<version>" --notes-file <notes> --latest`, never `--prerelease`.

   The notes are the version's `CHANGELOG.md` section without its `## v<version>` heading (the client's What's new window shows each release's body under its own heading, [auto-update.md](./auto-update.md)), checked against the issues `linear release status` lists in `vNext`. A release that breaks sessions or the protocol keeps its marker line as the notes' first line ([Mark what a release breaks](#mark-what-a-release-breaks)).
10. **Linear.** Once the release is public: `uv run --project ~/.claude/tools/linear python -m linear release complete rustling-tulip --version v<version> --sha <tag commit> --repo-root .`. It files every shipped Done issue under the new release and moves `vNext+1` up into `vNext`. Never run it twice for one version; a cut that missed issues is fixed with `linear release settle`.
11. **After the first release** the repo's test mode turns `stable` (the `rustling-tulip-nextup` profile's `test mode:` line and CLAUDE.md), so every landed change runs the full suite from then on.
