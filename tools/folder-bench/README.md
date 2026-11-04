# folder-bench

A bench harness that times fetching a folder from the builder (`builder@leftos-buildbox`, over the tailnet) by each transfer candidate. It is a development tool: `publish = false`, never in the installer. It adds no crate: it shells out to the system `C:\Windows\System32\tar.exe` (bsdtar, on both machines), the zstd CLI (see "serve-tar") and the Windows OpenSSH client in `C:\Windows\System32\OpenSSH\` (`ssh`, `scp`, `sftp`).

Everything it puts on the builder lives under `C:\Users\builder\.tmp\folder-bench\`: the binaries in `bin\`, the fixtures wherever `make-fixtures` is pointed (`fx\` below), and the isolated daemon's folders (`config\`, `binaries\`, `worktrees\`, `tailnet\`).

## Build and push

```powershell
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/build-bench.log --timeout 2400 --slot heavy -- cargo build --release -p folder-bench -p daemon -p tracer
target\release\folder-bench.exe push target\release\folder-bench.exe
```

The routed build brings `target\release\*.exe` back to the PC, where `push` and `run` find them.

`push <local folder-bench.exe>` creates `C:\Users\builder\.tmp\folder-bench\bin\` and copies there, with `scp`, the given `folder-bench.exe` and the `rustling-tulipd.exe` and `rt-tracer.exe` beside it (a missing one is named on stderr and skipped; only the fetch candidates need them).

It also writes `rtd.cmd` there: a batch file that sets `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR`, `RUSTLING_TULIP_WORKTREES_DIR` and `RUSTLING_TULIP_TAILNET_DIR` to folders under `C:\Users\builder\.tmp\folder-bench\`, then runs `rustling-tulipd.exe` with its arguments.

It also writes `isolated.cmd`, which sets the same variables and then runs its arguments as a command line. Every daemon command the bench runs on the builder goes through one of them, so the builder's own daemon and sessions are never touched. Neither creates the config folder: over an elevated SSH logon the daemon refuses one that inherits its parent's permissions.

## Fixtures

```powershell
ssh builder@leftos-buildbox C:\Users\builder\.tmp\folder-bench\bin\folder-bench.exe make-fixtures C:\Users\builder\.tmp\folder-bench\fx --scale small
```

`make-fixtures <dir> [--scale small|full]` (default `full`) writes three shapes under `<dir>`, the same bytes and names on every run (a fixed-seed xorshift):

| Shape | `full` | `small` |
|---|---|---|
| `small-files` | 2000 text files of 1-20 KiB in nested folders (`dNN\sN\file-NNNN.txt`) | 50 |
| `large-binaries` | 3 files of 64 MiB of low-compressibility bytes | 3 of 1 MiB |
| `html-media` | `index.html` referencing 20 image-like binaries of 256 KiB in `media\` | 3 |

It prints one JSON line per shape: `{"shape":…,"files":…,"bytes":…}`.

## Runs

```powershell
target\release\folder-bench.exe run <candidate> --shape <shape> --remote-dir C:\Users\builder\.tmp\folder-bench\fx --dest .tmp\fb\run-01 [--scale small|full] [--in-flight N] [--timeout-s S] [--zstd <local zstd.exe>]
```

`--remote-dir` is the fixtures folder on the builder (what `make-fixtures` was given); the run fetches `<remote-dir>\<shape>` into `<dest>\<shape>`, which must not exist yet, so each run takes a fresh `--dest`. `--scale` must match the one the fixtures were made at: the run checks the files against that scale's manifest. Remote paths must have no spaces or shell characters. Only `fetch-n` takes `--in-flight` (default 4); any other candidate refuses it.

Each run prints one JSON line and exits 0 when `ok`, 1 otherwise:

`{"candidate":…,"shape":…,"files":…,"bytes":…,"in_flight":…|null,"wall_ms":…,"local_cpu_ms":…|null,"remote_cpu_ms":…|null,"cpu_note":…|null,"ok":…,"error":…|null}`

`ok` means every file of the manifest is on disk under `<dest>\<shape>` with the fixture's exact bytes: each file is streamed and compared with the bytes the generator makes for that path. Otherwise `error` names the first missing, wrong-sized or different file (`<path>: contents differ from the fixture at byte <n>`), or the failure.

A child that runs past the hang guard (`--timeout-s`, default 900) is killed with every other child of the run, and `error` is `hung`, or `hung after: <first error>` when the run had also failed.

For the candidates that start processes on the builder (the tar and fetch candidates, `ssh-tar`), the guard also stops the builder's `folder-bench.exe`, `tar.exe` and `zstd.exe` whose command line names `<remote-dir>\<shape>`, with their children of those names; a failed cleanup adds `; remote cleanup failed: <why>`. `ok` reflects only the transfer and its check, never a CPU reading.

`in_flight` is the transfers outstanding at once for the fetch candidates (1 for `fetch-seq`) and `null` for the rest.

Every candidate measures in one window that opens, with the local CPU baseline, just before its first SSH connect, so `wall_ms` includes the connect (and `Hello` for the fetch candidates) everywhere. It closes when the last byte is written: when the local unpacker or copier exits, or at the fetch session's last `FileFetchDone`. The content check runs after it.

`local_cpu_ms` is the harness's own CPU over the window plus every child it started there (`ssh`, `scp`, `sftp`, the local `zstd` and `tar.exe`). `remote_cpu_ms` is the builder's side where it is measured. Either is `null` when any part of it is unknown (a CPU read that failed, a child that was never waited, a probe that failed), and `cpu_note` then says why; it also says `daemon not idle` (below). `cpu_note` is `null` when nothing needs saying.

| Candidate | What runs | `remote_cpu_ms` |
|---|---|---|
| `fetch-seq` | per-file `FetchFile`, one at a time, through `ssh … isolated.cmd folder-bench.exe relay -- rustling-tulipd.exe bridge` | the isolated daemon's CPU delta plus the `bridge`'s CPU as `relay` reports it |
| `fetch-n` | the same with `--in-flight N` outstanding | as `fetch-seq` |
| `tar-none`, `tar-zstd1`, `tar-zstd3` | `ssh … folder-bench.exe serve-tar` in base64 frames, decoded here into `tar.exe -x` (through `zstd -d` for zstd) as they arrive | `serve-tar`'s own, its `tar`'s and its `zstd`'s CPU, from the end frame |
| `tar-zstd1-raw` | the same at zstd level 1 with raw bytes, so the base64 cost shows by difference | from the end frame `serve-tar` writes to stderr in raw mode |
| `scp` | `scp -r -q builder@leftos-buildbox:<dir> <dest>` | `null` |
| `sftp` | `sftp -b <batch>` with `get -r <dir> <dest>\<shape>` | `null` |
| `ssh-tar` | `ssh … tar.exe -c -f - -C <dir> .` piped (by the harness, no shell) into `tar.exe -x -f - -C <dest>\<shape>` | `null` |

The fetch candidates first run an untimed setup session (`ssh … rtd.cmd bridge`): it starts the isolated daemon if none runs, registers `<remote-dir>\<shape>` as a repo named `folder-bench-<leaf of remote-dir>-<shape>` (`AddRepo`; a non-git folder is accepted; the leaf keeps fixtures at two scales apart) and reads its id (`ListRepos`). They then wait until the daemon is idle, meaning its CPU time is the same in two readings 1 s apart, for at most 30 s; past that `cpu_note` says `daemon not idle`.

Only then does the timed session open the window. Its own SSH connect, the `bridge` start and `Hello` fall inside the window, and the daemon start and the registration do not.

The daemon's CPU is read over a separate `ssh` with `Get-Process`, before the window and after the timed session ends. The pid comes from `C:\Users\builder\.tmp\folder-bench\config\daemon.json`, the one handshake a daemon under a config override writes, elevated or not. A missing file, one with no pid, or a pid that changed between the readings gives `null` and a `cpu_note`.

`relay -- <program> [args...]` runs the program with its own stdin, stdout and stderr, then writes `{"type":"end","cpu_ms":<n>}`, the program's CPU, to stderr.

The fetch candidates leave the isolated daemon running for the next run. Stop it afterwards, then check that no daemon from the bench folder still runs:

```powershell
ssh builder@leftos-buildbox C:\Users\builder\.tmp\folder-bench\bin\rtd.cmd shutdown
ssh builder@leftos-buildbox 'powershell -NoProfile -Command "Get-Process rustling-tulipd* -ErrorAction SilentlyContinue | Select-Object Id,Path | Format-List"'
```

## serve-tar

`serve-tar <dir> --codec none|zstd1|zstd3 --frame base64|raw [--zstd <path>]` runs on the builder: it spawns `C:\Windows\System32\tar.exe -c -f - -C <dir> .` and, for a zstd codec, pipes that into the zstd CLI (`<path> -1|-3 -c -q`, default `C:\Tools\bin\zstd.exe`), then writes the result to stdout.

With `--frame base64` it writes one line `{"type":"chunk","data":"<base64>"}` per 256 KiB (the daemon's `FetchFile` chunk size and encoding), then `{"type":"end","cpu_ms":<n>}` with its own, tar's and zstd's CPU.

With `--frame raw` it writes the bytes as they are and the end line to stderr.

Compression goes through the zstd CLI rather than bsdtar's own `--zstd` because bsdtar 3.8.9 (`C:\Windows\System32\tar.exe`) truncates `--zstd` output written to `-f -`: it drops the last partial block, so `tar -tf` on the result fails with "Truncated input file", while the same command writing a named file is whole. `--gzip` and `--xz` to stdout are not affected.

On the PC, `run` unpacks a zstd candidate with `zstd -d -c -q` piped into `tar.exe -x -f - -C <dest>\<shape>`, the zstd CLI taken from `run --zstd <path>` (default `zstd.exe` on PATH); a base64 candidate decodes its frames into that pipe as they arrive, and the raw one feeds `ssh`'s output to it directly.

## Tests

```powershell
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/nt-bench.log --timeout 1800 --slot heavy -- cargo nextest run -p folder-bench --cargo-profile test-release
```

The unit tests cover the pure parts: the fixture generator's determinism and counts, the frame round trip and end lines, the result line's fields, the content check's report, the CPU sum, the probe's script and output, the idle wait, the hang error and cleanup script, `--in-flight`'s rules, `relay`'s arguments and the stderr reads. None touches the network or the builder.
