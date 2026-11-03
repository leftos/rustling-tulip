# Elevated daemon isolation (RT-784)

A daemon running elevated (started from an elevated terminal, or by `rustling-tulipd bridge` over an administrator's SSH logon) hosts sessions that inherit its full token. Today any medium-integrity process of the same account can reach it: read the token in `daemon.json`, plant files the daemon acts on without a client (session sidecars it auto-resumes, a live agent's hook settings, the binary cache), or talk to its tracers' pipes. This design closes those paths. A daemon that is not elevated is unchanged.

All rulings below are orchestrator decisions (ship-fast profile), open to the owner.

## Invariants

1. A process below the daemon's integrity never authenticates on its loopback listener (`/ws`, `/shutdown`, `/hook`) unless the elevated profile itself opts in.
2. The elevated daemon's token is unreadable below its integrity.
3. An elevated daemon never acts on a file a lower-integrity process could have written (sidecars, hook settings, binaries, `state.json`, `devices.json`, `remote.json`, `daemon-id`).
4. No lower-integrity process and no other account opens a tracer pipe, and the daemon talks only to the tracer its own sidecar recorded.
5. Supervision never crosses integrity: `ensure_running` never reaps, retires or kills a daemon of another integrity, and never starts a second daemon into a profile one already owns.

## Decisions

- **A protected profile.** A daemon whose token is above Medium integrity uses its own root for config, binary cache and `daemon-id`, and a Credential Manager service name with its own suffix. The root is created with `D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)S:(ML;OICI;NRNWNX;;;HI)`, so everything under it inherits a High label that refuses reads and writes from below. The daemon refuses to start when the root already exists with another owner than Administrators or SYSTEM, or another DACL (an ordinary process could create it first), and applies the same check to the config and binaries overrides. The integrity-to-profile rule lives in one function in `daemon-client`, used by the daemon's `Dirs` and by `ensure_running`.
- **The elevated profile starts empty.** Presets, history, devices and sessions of the ordinary profile are not imported: importing files an ordinary process can write is the write-up this design removes. The ordinary profile's data stays where it is, and an ordinary client still sees it. The release notes say so.
- **Callers by integrity.** An elevated caller (client, CLI verb, SSH bridge) reuses a running ordinary daemon of its account, which gives no elevation, so the SSH workaround of starting the desktop daemon first keeps working; only when none runs does it start a daemon, which then uses the elevated profile. An ordinary caller uses the ordinary profile only and never sees the elevated one; finding `daemon.json` unreadable is a typed refusal, never "no daemon" (no reap, no spawn over it).
- **A connection check, elevated daemons only.** For each connection to `/ws`, `/shutdown` and `/hook`, the daemon finds the peer's pid from the TCP table (`GetExtendedTcpTable`, `TCP_TABLE_OWNER_PID_CONNECTIONS`) and reads its token's integrity; it refuses a lower peer and refuses when any step fails (an ordinary process can block the lookup, so the check fails closed). The refusal is `AuthFailed` with a reason naming how to start the client elevated. An opt-in `allow_unelevated_clients` setting lives in the protected profile, so only an elevated process can turn it on. Ordinary daemons run no check.
- **Tracer pipes.** Every tracer pipe gets an explicit security descriptor: an ordinary tracer `D:P(A;;GA;;;SY)(A;;GA;;;<user SID>)`, an elevated one `D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NWNR;;;HI)`. On connect and reattach the daemon checks `GetNamedPipeServerProcessId` against the tracer pid its sidecar recorded. This is additive, not a tracer ABI bump.

## Steps

Measured on the builder: a Medium process of the account read an elevated daemon's `daemon.json` and got a High shell through `rustling-tulipd spawn`. The steps run in this order so the measured attack closes first.

- [x] P2 The door: a High no-read-up label on an elevated daemon's `daemon.json`; a fail-closed integrity check on each connection to an elevated daemon (`/ws`, `/shutdown`, `/hook`) with no opt-in yet (the opt-in needs P1's protected profile); and `ensure_running` treating an unreadable handshake as a typed refusal that names how to reach that daemon, never as "no daemon".
- [ ] P1 Protected profile: the integrity-to-profile function, the protected root and its checks, `Dirs` and Credential Manager naming, `ensure_running` choosing by caller integrity, and the `allow_unelevated_clients` opt-in stored there.
- [ ] P3 Tracer pipe descriptors and the server-pid check, extending the tracer's shutdown tests.
- [ ] Measured end to end on the builder after each step.

## Out of scope, booked

- `git status` in the git watcher does not disable `core.fsmonitor`.
- The binary cache trusts an existing file without rehashing it.
- A Low-integrity process can drive an ordinary daemon (the same flaw one level down).
