# fake-ssh

A deterministic stand-in for the `ssh` client the native client runs for an SSH host (`apps/native/src/ssh.rs`), so the live tier can connect over the SSH transport without an sshd, a key or a second machine.

`FakeSsh` (`apps/native/tests/support/live.rs`) writes its state for a test daemon, and `apps/native/tests/e2e_ssh.rs` and `apps/native/tests/e2e_multi_host.rs` point the client at it through `RUSTLING_TULIP_SSH`. The state names one daemon, so every host string the client passes reaches that one, and a spec has one fake ssh per process.

It reads a JSON state file named by `RUSTLING_TULIP_FAKE_SSH_STATE` on every run:

- `bridge`: the absolute path of the `rustling-tulipd` binary to run as the remote command.
- `env`: variables merged over the shim's own environment for the bridge, so the bridge (and a daemon it starts) reaches only the test daemon's isolated dirs.
- `fail`: `null`, or `{"code": <exit code>, "stderr": "<text>"}` to act as a refused login.
- `log`: the absolute path of the argv log.

Each run:

1. Appends `{"pid": <the shim's node process>, "argv": [...]}` as one JSON line to `log`. A spec kills that pid's tree to cut the connection.
2. Checks the argv shape: `-T` and `-o BatchMode=yes` among the options, and `--` directly before the host and exactly one remote-command argument. A mismatch writes `fake-ssh: unexpected argv: <argv as JSON>` to stderr and exits 2.
3. With `fail` set, writes its `stderr` and exits with its `code`.
4. Otherwise ignores the host, splits the remote command on whitespace, requires its first word to be `rustling-tulipd` (else exits 2), and runs `bridge` with the remaining words as arguments, stdio inherited, exiting with the bridge's code.

An unset variable, an unreadable state file, a failed log write or a bridge that cannot start exits 1 with a `fake-ssh:` line on stderr.

`fake-ssh.cmd` (Windows) and `fake-ssh.sh` (POSIX) run `index.mjs` under `node`, forwarding every argument. Node must be on `PATH`.
