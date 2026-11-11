# Release procedure

What a versioned release of rustling-tulip publishes, starting with `v0.1.0-alpha`. The work a release ships is chosen in Linear through the release pipeline: everything in `vNext` (Linear team RT) ships in the next release, and `vNext+1` holds what is planned after it.

## What a release publishes

- A git tag `v<version>` on `main` (`v0.1.0-alpha`, `v0.2.0-alpha`, …).
- A GitHub release of that tag, never marked pre-release (every build is an alpha, and the flag would hide the newest one from the repo's home page) and marked Latest, with the Windows installer and its SHA-256 file attached: `rustling-tulip-<version>-setup.exe` and `rustling-tulip-<version>-setup.exe.sha256`. A release missing either is never offered to a Windows client as an update. Both are built by `.\rt.ps1 installer` (NSIS, per-user, code-signed with Azure Artifact Signing; see `tools/installer/README.md`).
- The macOS (Apple silicon) assets: `rustling-tulip-<version>-macos-arm64.dmg` (what a person downloads), and `rustling-tulip-<version>-macos-arm64.zip` with `rustling-tulip-<version>-macos-arm64.zip.sha256` (what the app's updater downloads and checks on a Mac). They are signed with the Developer ID, notarized and stapled, built by `tools/installer/pack-macos-signed.ps1` (`tools/installer/README.md` "macOS code signing").

  A release missing the macOS pair stays valid for Windows and is simply not offered to a Mac as an update ([auto-update.md](./auto-update.md)).
- The release's notes, which are the version's `CHANGELOG.md` section without its `## v<version>` heading (the client's What's new window shows each release's body under its own heading, [auto-update.md](./auto-update.md)), opening with the breaking marker when the release has one.
- The version lives in one place, `[workspace.package] version` in `Cargo.toml`, which every crate inherits; the installer reads it from there.

## Mark what a release breaks

A release that breaks live sessions or the protocol says so in a marker line, which the app reads from the release's notes on the feed (`update_core::breaks_of`): `<!-- breaks: sessions, protocol -->`, with either word or both. It is the first line of the version's `CHANGELOG.md` section, right under its `## v<version>` heading, so it reaches the notes file the publish step uploads; the app leaves the line out of the notes it shows.

- `sessions`: an update to this release ends the live sessions on the machine it updates: a tracer ABI break ([tracer-abi.md](./tracer-abi.md)), a session sidecar change an older daemon's sessions cannot be read back from, or anything else that ends live sessions across an update.
- `protocol`: `protocol-version.json`'s `supported` range drops a version the last release spoke, so a client of the last release can no longer connect to this one.

A release with no marker breaks nothing. The Update host dialog warns from the markers of every release between the host's version and the newest, and after an update that breaks either, the app that asked updates itself right after the host when its own release check succeeded and it is older than the newest release.

Before publishing, check what changed since the last release: `git diff <last tag> -- protocol-version.json crates/tracer-protocol`. A change to `supported` that drops a version, or a tracer ABI bump, needs its word in the marker.

## Cutting a release

The `prepare-release` skill ([.claude/skills/prepare-release/SKILL.md](../.claude/skills/prepare-release/SKILL.md)) holds the steps, from the scope check to planning the next release. A person starts it as `/prepare-release`, and a `/nextup` run follows it once `vNext` is done.
