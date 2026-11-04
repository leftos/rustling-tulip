# RT-878 folder-transfer bench: every candidate over the full fixtures, builder to PC

Date: 2026-11-04 · commit ce84b14c · RT-878 (part B of RT-748)

Method: `tools/folder-bench` (its README is the harness's manual) fetched each fixture shape from the builder to this PC with each transfer candidate, three reps each, 90 runs in all. The nine per-file fetch runs on `small-files` failed in the first matrix on a repo-name clash in the harness and were rerun after the fix (see `small-files` below).

Every number below is computed by `.tmp/rt-878/summarise.py` from the 90 result lines in `.tmp/rt-878/results.jsonl`; the tables are its output pasted unchanged.

## Setup

- This PC (the receiving side): AMD Ryzen 7 7800X3D, 8 cores, 16 logical processors.
- The builder `builder@leftos-buildbox` (the sending side): AMD Ryzen 9 9950X, 16 cores, 32 logical processors.
- OpenSSH on this PC: `OpenSSH_for_Windows_9.5p2, LibreSSL 3.8.2`.
- The link is the tailnet, on a direct path both before and after the timed runs, never DERP. Before: `via 192.168.50.30:41702`, pings of 18 ms (`tailscale ping --c 5`, which stops at the first direct pong) then 3, 3, 5, 12 and 3 ms (`--until-direct=false`). After: `via 172.17.80.1:41702`, a different direct endpoint, pings of 2 ms then 75, 4, 3, 3 and 20 ms.
- Fixtures (`make-fixtures --scale full`, in `C:\Users\builder\.tmp\folder-bench\fx-full`): `small-files` 2000 files, 21226756 bytes; `large-binaries` 3 files, 201326592 bytes; `html-media` 21 files, 5244366 bytes.
- Reps: three, as the outer loop (rep 1 runs every shape and candidate, then rep 2, then rep 3). Each run was its own gate call holding this PC alone (`--slot critical --alone --local`), so no other gate on this PC overlapped a timed run; other sessions' gates could run between two runs.

Settled decisions (orchestrator decisions in RT-748 and RT-877, not owner rulings):

- Every run goes over the tailnet to `builder@leftos-buildbox`. The daemon's transport (`fetch-seq`, `fetch-n`) is measured through `rustling-tulipd bridge` over SSH, the SSH host path a client uses. The direct tailnet WebSocket listener is not measured, since an isolated daemon would need a tailnet listener and its owner rule.
- The packed stream (candidate 2) is measured without product changes: `serve-tar` packs with the system `tar.exe` and compresses through the zstd CLI, because bsdtar 3.8.9 truncates `--zstd` written to stdout. It frames the stream as 256 KiB base64 JSON lines like `FetchFileChunk`; `tar-zstd1-raw` sends the same stream as raw bytes, so the base64 cost shows by difference.
- `wall_ms` includes the SSH connect for every candidate (and `Hello` for the fetch candidates). Remote CPU includes the bridge relay for the fetch candidates. Remote CPU for `scp`, `sftp` and `ssh-tar` is unmeasured.

## Results

Medians, minimums and maximums are over the reps that were `ok`. MB/s is the shape's bytes over the median wall time, with MB = 10^6 bytes.

### `small-files`

| Candidate | Median wall ms (min-max) | MB/s | Median local CPU ms | Median remote CPU ms | ok |
|---|---|---|---|---|---|
| `fetch-seq` | 11264 (10463-34358) | 1.9 | 2155 | 1702 | 3/3 |
| `fetch-n` (4 in flight) | 3452 (2934-4922) | 6.1 | 1718 | 1406 | 3/3 |
| `fetch-n` (16 in flight) | 1769 (1426-1811) | 12.0 | 1531 | 1249 | 3/3 |
| `tar-none` | 1194 (1009-1548) | 17.8 | 1155 | 93 | 3/3 |
| `tar-zstd1` | 1040 (940-1634) | 20.4 | 936 | 217 | 3/3 |
| `tar-zstd3` | 1071 (967-1242) | 19.8 | 843 | 186 | 3/3 |
| `tar-zstd1-raw` | 1133 (1067-2048) | 18.7 | 919 | 170 | 3/3 |
| `scp` | 38140 (37772-40370) | 0.6 | 1187 | unmeasured | 3/3 |
| `sftp` | 31755 (29151-41526) | 0.7 | 968 | unmeasured | 3/3 |
| `ssh-tar` | 1773 (1215-3338) | 12.0 | 1249 | unmeasured | 3/3 |

In the first matrix the nine fetch runs on this shape (`fetch-seq`, `fetch-n` with 4 and 16 in flight, reps 1-3) failed in the untimed setup session, before the window opened, with: "registering C:\Users\builder\.tmp\folder-bench\fx-full\small-files: Can't save "folder-bench-small-files": its worktree folder name "folder-bench-small-files" is already used by repo "folder-bench-small-files". Rename one of them."

The isolated daemon already held a repo of that name, and the harness then named a repo after its shape alone. The harness now registers `folder-bench-<fixtures folder>-<shape>` (`folder-bench-fx-full-small-files`). The nine runs were rerun after the 90-run matrix, under the same gate form and rep order, and their rows above are those reruns; all nine were `ok`.

### `large-binaries`

| Candidate | Median wall ms (min-max) | MB/s | Median local CPU ms | Median remote CPU ms | ok |
|---|---|---|---|---|---|
| `fetch-seq` | 3789 (3759-6678) | 53.1 | 2624 | 531 | 3/3 |
| `fetch-n` (4 in flight) | 4016 (3723-4534) | 50.1 | 2640 | 562 | 3/3 |
| `fetch-n` (16 in flight) | 4194 (3789-5312) | 48.0 | 2671 | 827 | 3/3 |
| `tar-none` | 3883 (3355-6343) | 51.8 | 3451 | 437 | 3/3 |
| `tar-zstd1` | 4555 (4353-8010) | 44.2 | 4421 | 1029 | 3/3 |
| `tar-zstd3` | 5393 (4005-9145) | 37.3 | 3936 | 1249 | 3/3 |
| `tar-zstd1-raw` | 3479 (2806-8863) | 57.9 | 4031 | 1217 | 3/3 |
| `scp` | 3047 (2801-21026) | 66.1 | 562 | unmeasured | 3/3 |
| `sftp` | 3031 (2590-3806) | 66.4 | 390 | unmeasured | 3/3 |
| `ssh-tar` | 3177 (2747-10080) | 63.4 | 3468 | unmeasured | 3/3 |

### `html-media`

| Candidate | Median wall ms (min-max) | MB/s | Median local CPU ms | Median remote CPU ms | ok |
|---|---|---|---|---|---|
| `fetch-seq` | 694 (538-2604) | 7.6 | 155 | 125 | 3/3 |
| `fetch-n` (4 in flight) | 618 (430-1821) | 8.5 | 172 | 93 | 3/3 |
| `fetch-n` (16 in flight) | 599 (483-809) | 8.8 | 218 | 62 | 3/3 |
| `tar-none` | 466 (373-775) | 11.3 | 108 | 46 | 3/3 |
| `tar-zstd1` | 383 (361-964) | 13.7 | 217 | 30 | 3/3 |
| `tar-zstd3` | 568 (381-1816) | 9.2 | 139 | 45 | 3/3 |
| `tar-zstd1-raw` | 362 (349-560) | 14.5 | 154 | 61 | 3/3 |
| `scp` | 1080 (977-1328) | 4.9 | 46 | unmeasured | 3/3 |
| `sftp` | 1523 (810-1561) | 3.4 | 93 | unmeasured | 3/3 |
| `ssh-tar` | 878 (359-3959) | 6.0 | 170 | unmeasured | 3/3 |

## Base64 cost

`tar-zstd1` (base64 frames) against `tar-zstd1-raw` (the same stream as raw bytes), medians; a difference is base64 minus raw, the percentage relative to raw.

| Shape | Wall ms base64 / raw | Wall diff | Local CPU ms base64 / raw | Local CPU diff | Remote CPU ms base64 / raw | Remote CPU diff |
|---|---|---|---|---|---|---|
| `small-files` | 1040 / 1133 | -93 ms (-8.2%) | 936 / 919 | +17 ms (+1.8%) | 217 / 170 | +47 ms (+27.6%) |
| `large-binaries` | 4555 / 3479 | +1076 ms (+30.9%) | 4421 / 4031 | +390 ms (+9.7%) | 1029 / 1217 | -188 ms (-15.4%) |
| `html-media` | 383 / 362 | +21 ms (+5.8%) | 217 / 154 | +63 ms (+40.9%) | 30 / 61 | -31 ms (-50.8%) |

## Findings

- Fastest median per shape: `small-files` `tar-zstd1` (1040 ms, 20.4 MB/s); `large-binaries` `sftp` (3031 ms, 66.4 MB/s), with `scp` (3047 ms) and `ssh-tar` (3177 ms) next; `html-media` `tar-zstd1-raw` (362 ms, 14.5 MB/s), with `tar-zstd1` (383 ms) next.
- On `small-files` the four packed-stream candidates fall within 1040-1194 ms, `ssh-tar` takes 1773 ms, and `scp` and `sftp` take 38140 and 31755 ms, more than 25 times the packed stream.
- On `large-binaries` the candidates without compression or framing (`scp`, `sftp`, `ssh-tar`, `tar-zstd1-raw`) lead at 3031-3479 ms; `scp` and `sftp` do it with the least local CPU (562 and 390 ms against 3451 ms and more for the tar candidates).
- Per-file fetch with more transfers in flight: on `small-files` it scales strongly (11264, 3452, 1769 ms for 1, 4, 16 in flight) with the remote CPU falling (1702, 1406, 1249 ms).
- On `small-files` at 16 in flight the per-file fetch matches `ssh-tar` (1773 ms) and is 1.7 times the `tar-zstd1` median (1040 ms), with more CPU on both sides (1531 ms local and 1249 ms remote against 936 and 217).
- Per-file fetch on the other shapes: on `large-binaries` it does not get faster (3789, 4016, 4194 ms for 1, 4, 16 in flight) while the remote CPU rises (531, 562, 827 ms). On `html-media` it gets slightly faster (694, 618, 599 ms) and the remote CPU falls (125, 93, 62 ms). It stays slower than `tar-none` on all three shapes.
- Compression against `tar-none`: on `small-files` zstd level 1 and 3 save 154 and 123 ms of wall; on `html-media` level 1 saves 83 ms and level 3 costs 102 ms; on the low-compressibility `large-binaries` level 1 and 3 cost 672 and 1510 ms.
- The base64 frames cost 1076 ms of wall (+30.9%) on `large-binaries`; on the two smaller shapes the wall difference (-93 and +21 ms) is smaller than the spread between reps.
- The spread between reps is wide on several rows (for example `fetch-seq` on `small-files`, 10463-34358 ms, `scp` on `large-binaries`, 2801-21026 ms, and `ssh-tar` on `html-media`, 359-3959 ms), so differences of a few hundred milliseconds between medians of three reps are within it.

## Ruling for RT-747

Orchestrator decision (not an owner ruling; open to the owner): a folder is fetched as **one packed stream**. The host packs the folder as a tar stream compressed at zstd level 1 and sends it over the daemon connection in the `FetchFileChunk` framing, and the client unpacks it as it arrives. Per-file `FetchFile` stays for a single file.

- **Why the packed stream:** it is the fastest candidate on many small files, the shape a source folder has: 1040 ms against 11264 ms for sequential per-file fetch and 1769 ms with 16 in flight, with a fraction of the remote CPU (217 ms against 1249 ms). On a page with its media it is the fastest too.
- **Why not `scp`, `sftp` or `ssh-tar`:** they exist only for an SSH host, while a tailnet host reaches the client through the daemon alone; on small files `scp` and `sftp` are more than 25 times slower than the packed stream.
- **Why zstd level 1:** it saves wall time on text and media, while level 3 buys nothing more on text and costs more on media. On incompressible binaries it costs about 0.7 s on 201 MB, which is acceptable for a folder fetch.
- **What the product cannot copy from the bench:** the bench shells out to `tar.exe` and the zstd CLI. A host cannot be assumed to have the zstd CLI, and bsdtar 3.8.9 truncates `--zstd` written to stdout. So the daemon packs in-process and the client unpacks in-process; RT-747's brief names and justifies the crates it adds.
- **Base64:** the base64 framing costs about 1 s (+31%) on 201 MB of binaries and is within the spread on the smaller shapes. Binary frames are left to the remote-traffic item (RT-120) rather than this one.
