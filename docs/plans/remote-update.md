# Remote update

A client on another computer updates a host, and a host updates the clients connected to it. Without this, a release with a client half and a daemon half is only finished once every computer has been updated by hand.

Issue: RT-1298. It replaces the rule in `docs/auto-update.md` decision 11 that an update is for this PC only.

## The rule everything rests on

**The machine being updated fetches the newest official release itself and checks its hash.** The requester names no file and no version. So asking gives the requester one power: to bring another machine up to the newest release, at a moment of its choosing. A compromised requester can force a restart and nothing more, which is less than a paired device can already do by running commands.

The check is the one the client's updater makes: the release's `.sha256`, from the same feed. No signature check is added.

## Owner rulings

- **Who may ask:** any paired tailnet device or an SSH client.
- **Consent:** the requester confirms in a dialog listing the host's running sessions. The host's own client, if open, shows who asked. No approval at the host and no host setting.
- **App open on the host:** it restarts itself after a 10-second countdown with Cancel, which the daemon runs and the app shows; Restart to update has no countdown.
- **SSH hosts:** a detached helper started over a second SSH command runs on after the channel drops, and the client reads the result on reconnect. Tailnet hosts are built first.
- **Host to client:** the same power both ways. From the host, "Update connected clients" gives each native client the same countdown with Cancel, and it updates itself by the same rule. Phone apps just reload.
- **A release that breaks sessions or the protocol:** the release carries a machine-readable flag. The confirm dialog says what will end and lets it go ahead, and the host goes first, the requesting client updating itself right after.
- **Branch:** `feat/remote-update`, merged when the feature works end to end.

## Orchestrator decisions

- **Another OS:** the host picks its own asset. A host with no asset for its OS (only Linux and Intel macOS now; macOS on Apple silicon updates by staging the app bundle and running the staged daemon's `apply-update`) or with no install (a dev build) answers with why, and the requester shows that instead of offering an update.
- **Version and kind on the wire:** `Welcome` gains the daemon's build version, and `Hello` gains the client's version and kind, all additive, so each side can see who is behind and a phone is told apart from a native client. The PWA sends its kind too.
- **Failure:** the new daemon sends the requester the result from `last-result.json` after it starts. A requester whose host does not come back on the new version in a bounded time says so.
- **Refusals:** the new messages are allowed to a tailnet device on purpose. `host_only_refusal` stays a deny-list, so the review checks that this is deliberate rather than a gap.

## What the code does not do yet

- A running `bridge` blocks the swap with exit 3, since it runs from the install folder.
- macOS has no cross-process update lock, so a client's own update and a daemon's remote update can race; each stages into its own folder and `apply-update` refuses an in-use bundle.
- Leftover cleanup of the headless-daemon path is open as RT-1353.
- A host has no per-connection record of its clients, and no addressed send to one.
- A release carries no machine-readable note of what it breaks.

## Parts

Each part lands on `feat/remote-update`, and each is a sub-issue of RT-1298.

- [x] **Versions on the wire.** `Welcome.build_version`, `Hello.client_version` and `client_kind`, the host's version and "behind" state in the Connections dialog and Settings > Hosts. Regenerate `apps/pwa/src/protocol.ts`.
- [x] **The shared updater core.** Move finding, downloading and verifying a release out of `apps/native/src/update.rs` into a crate both the client and the daemon link, with no behaviour change.
- [x] **A host updates itself on request.** A new client message, a `remote_update` feature string listed once a host can honour it, the daemon fetching and verifying its own asset and running the swap detached from itself, restarting the daemon afterwards when no client will, and answering with why when it cannot. The host's own app restarts on a countdown with Cancel. Windows and macOS (Apple silicon) are both done.
- [x] **Client to host, in the client.** The update action per host, the confirm dialog listing that host's sessions, progress, reconnect, and the result or failure shown to the requester.
- [ ] **The breaking flag.** A machine-readable note in each release of what it breaks, read by the confirm dialog, and the requester updating itself right after the host. `docs/release.md` says when to set it.
- [ ] **Host to client.** A per-connection record on the host, an addressed message, "Update connected clients", and the client's countdown with Cancel.
- [ ] **SSH hosts.** The detached helper over a second SSH command, surviving the bridge's exit, with the result read on reconnect.
- [ ] **Proof.** A live spec against a fake release feed, and a real update on a second machine (`docs/release.md` smoke test), both directions, on Windows and macOS.

## Trust and safety checks for each brief

- A part that runs a program or replaces files gets an invariant pass first: who can reach it, what a hostile peer can name, and which paths it writes.
- A saved file's shape changes only with a load test from `v0.5.1-alpha` on (CLAUDE.md "Saved user data survives every update").
- The new wire fields are additive, with no `protocol-version.json` bump.
