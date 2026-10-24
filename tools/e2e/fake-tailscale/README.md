# fake-tailscale

A deterministic stand-in for the `tailscale` CLI the daemon's tailnet listener runs (`crates/tailnet/src/cli.rs`), so the native client's live tier can turn remote access on without a real tailnet, a real certificate or the user's Tailscale. `LiveDaemon::start_remote` (`apps/native/tests/support/live.rs`) points the daemon at it, and every live daemon gets it in place of the real CLI.

It answers from a JSON state file named by `RUSTLING_TULIP_FAKE_TAILSCALE_STATE`: `user_id` (the owner's numeric Tailscale user id), `cert_file` and `key_file` (PEM files for `localhost`), and optionally `login` and `whois_user_id` (the peer's user id, the owner's by default, for a non-owner peer).

- `whoami --json`: the host node `localhost.`, the owner's, untagged, at `127.0.0.1/32`, so the listener binds loopback and asks for a certificate for `localhost`.
- `whois --json <ip:port>`: the peer node, another node id, untagged, at `100.101.102.103/32`: the address a daemon built with the `e2e-loopback-peers` feature judges a loopback peer as. Without that feature the listener drops a loopback peer before it asks.
- `status --json`: this node alone, online, with no peers.
- `cert --cert-file F --key-file K [--min-validity Ns] -- <name>`: copies the state's `cert_file` and `key_file` to F and K.

Any other command, an unset variable, an unreadable state file or a failed copy exits 1 with a `[fake-tailscale]` line on stderr.

`fake-tailscale.cmd` (Windows) and `fake-tailscale.sh` (POSIX) run `index.mjs` under `node`, forwarding every argument. Node must be on `PATH`.
