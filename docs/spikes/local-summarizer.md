# Spike: a local model as the summarizer, smoked against Haiku

2026-10-27. For RT-27. Question: can a local model served over an OpenAI-compatible endpoint replace the cloud provider for the [summarizer service](../plans/dashboard.md) (the spoken alerts' kind and one-line texts, and the Dashboard's `working` refreshes)? The owner set the reference to Claude Haiku at its lowest effort with no thinking, run as a smoke test. A DeepSeek Flash pass (the design's default provider) over a larger sample ran before that ruling and is kept as a second reference.

Files in [local-summarizer/](./local-summarizer/): `prompt.md`, `extract.py` (collects and redacts the samples), `bench.py` (runs a sample folder through Haiku, DeepSeek or a local llama-server, which it starts and stops itself), `score.py` (the tables below). Each script's docstring has its usage; all run as `uv run --no-project python -I <script>`. The samples are transcript text and stay outside the repo.

## Verdict

**Do not adopt a local model.** Granite 4.2 at 3B and 8B fails the kind-agreement bar against Haiku by a wide margin: 72.2% and 75.0% against a bar of 95% (Haiku's own run-to-run agreement, 100%, minus 5 points). On the 13 samples every cloud run labelled the same way, the two reach only 76.9% and 80.8%.

They pass every other part of the rule: 100% valid JSON with no grammar, warm GPU p95 of 0.51 s and 1.12 s against the 3 s bar, and the 3B on CPU at p95 5.15 s against the 8 s bar. The 8B misses the CPU bar at p95 14.07 s, so it would be GPU-only.

The DeepSeek pass reads differently, and the difference is the references, not the models. Against DeepSeek on 43 samples the 8B agrees 92.9% with DeepSeek's run-to-run 97.5%, just over that rule's 92.5% bar.

But Haiku and DeepSeek agree with each other only 81.8% on the smoke set, so a bar set by either reference's own consistency is stricter than the task's real ambiguity. Both local models also leak plan numbers and function names into speech about as often as Haiku does, where DeepSeek leaked none under the same prompt (below).

## Setup

- **Server**: llama.cpp `llama-server` build 11149 (commit d2e54583c), the winget package. That package ships the Vulkan backend (`ggml-vulkan.dll`), not CUDA, so the GPU runs are Vulkan on the RTX 4090 (24 GB). CPU: Ryzen 7 7800X3D, 8 cores / 16 threads, 63 GB RAM.
- **Flags**: `-m <gguf> -c 8192 -ngl 99` (GPU) or `-ngl 0` (CPU), `-np 1` (one slot, so the whole 8,192 context is one request's), `--jinja`, `--no-webui`, `--host 127.0.0.1 --port 18427`. No API key and no grammar.
- **Request**: `POST /v1/chat/completions`, one user message, `temperature: 0`, `max_tokens: 400`, `chat_template_kwargs: {"enable_thinking": false}`. Granite 4.2 is a reasoning model whose template thinks by default; thinking was off for every local run.
- **Models** (Hugging Face `ibm-granite`, Apache-2.0, Q4_K_M, SHA-256 checked against the Hub):

  | Model | File | Size |
  |---|---|---|
  | Granite 4.2 3B (the `granite4.2:3b` ops-tower runs in Ollama) | `granite-4.2-3b-GGUF/granite-4.2-3b-Q4_K_M.gguf` | 2,244,011,552 bytes (2.09 GiB) |
  | Granite 4.2 8B (the 7–8B second candidate) | `granite-4.2-8b-GGUF/granite-4.2-8b-Q4_K_M.gguf` | 5,347,917,952 bytes (4.98 GiB) |

- **Haiku reference**: the Claude Code CLI headless, `claude -p --model claude-haiku-4-5 --effort low --output-format json --no-session-persistence --setting-sources "" --strict-mcp-config --tools ""`, prompt on stdin, with `MAX_THINKING_TOKENS=0` in its environment.

  `--effort low` is the CLI's lowest level, but on its own Haiku still thinks: one call wrote 1,650 output tokens and took 13.1 s of API time, against 89 tokens and 1.4 s with `MAX_THINKING_TOKENS=0`. The thinking run is kept as an extra column ("Haiku, low effort, thinking").
- **DeepSeek reference**: `deepseek-flash` through `https://api.deepseek.com/anthropic/v1/messages` at its default effort with `max_tokens: 1500`, the design's request.
- **Prompt**: `prompt.md` is the earlier spike's `prompt-v2.md` with the design's `headline` field (at most 8 words) added and the `Session:` line removed (no session name leaves the machine). It does not yet carry v3's rule against plan numbers and function names, so every model ran the same v2 text.

## Method

- **Samples**: `extract.py` takes the last assistant text of each turn in this repo's top-level transcripts (`subagents` folders skipped) whose last block is text and which a user turn follows: 1,214 candidates. It redacts the design's secret shapes and caps each sample as the daemon would (first 1,000 and last 5,000 characters).

  Heuristic buckets (asks a question, mentions running work, reports work landed, other) gave 40 samples plus the earlier spike's three messages; 20 more from the landed bucket were added because the references labelled few samples `done_waiting`. This repo's stops are mostly an orchestrator reporting work that is still running.
- **Smoke set**: 18 samples, 5 per kind by DeepSeek's label plus the three spike messages, each run twice through Haiku (no thinking), Haiku (thinking), and both local models on GPU. Haiku's labels then came out 9 `done_waiting`, 5 `needs_answer`, 4 `working_update`.
- **Scoring** (`score.py`): JSON is strict when the reply parses as is, tolerant after stripping a code fence and reading the first object (the design's parser). Kind agreement is against the reference's first parsed round. Cap violations are a headline over 8 words, speech over 25, or a notification over 100 characters. A leak is a plan or issue number (`RT-389`, `P4.13b`, `AP.8c`), a code identifier (`reattach_orphans`), a path or a hash in the headline or speech.
- **Timing**: every local pass ran through the gate as `--slot critical --alone --local`, and each log shows "holds the machine alone (critical)". The gate's own load lines show 6.1 to 8.6 of 16 cores busy outside any gate during those passes (ungated `rustc` processes among them), so the CPU figures are pessimistic. The Haiku and DeepSeek passes ran on `--slot light` and `--slot none`. Each local server was started by `bench.py` and stopped at the end of its pass.

## Results: quality against Haiku (smoke set, 18 samples × 2 rounds)

| Run | strict JSON | tolerant JSON | kind agreement | agreement on the 13 consensus samples | cap violations | leaks |
|---|---|---|---|---|---|---|
| Haiku, no thinking (reference), second round | 0% (0/18) | 100% (18/18) | 100% (18/18) | – | 4 | 9 |
| Haiku, low effort, thinking | 0% (0/36) | 100% (36/36) | 83.3% (30/36) | – | 1 | 11 |
| DeepSeek Flash | 91.7% (33/36) | 91.7% (33/36) | 81.8% (27/33) | – | 0 | 0 |
| Granite 4.2 3B, GPU | 100% (36/36) | 100% (36/36) | 72.2% (26/36) | 76.9% (20/26) | 2 | 12 |
| Granite 4.2 8B, GPU | 100% (36/36) | 100% (36/36) | 75.0% (27/36) | 80.8% (21/26) | 10 | 20 |

The 13 consensus samples are those on which every round of all three cloud runs gave one kind (5 `done_waiting`, 4 `needs_answer`, 4 `working_update`). Haiku's JSON always came in a code fence. The three DeepSeek failures are replies cut off at `max_tokens` 1500 by its thinking.

The disagreements sit on two boundaries: work that will continue but is not described as running (`working_update` or `done_waiting`), and an offer at the end of a report (`needs_answer` or `done_waiting`). The references disagree there too.

Haiku without thinking labelled the spike's `message-update.md` ("Three implementers are still running") `done_waiting` while its own speech said "Three other tasks still running"; every other run said `working_update`. The local models' misses include clear cases the cloud runs all agree on, such as "Three explorers are mapping the slice now" (3B: `done_waiting`).

Leaks are not where the local models fall behind Haiku. Both Granite models copy issue and plan numbers into speech ("RT-389 landed, completing RT-340."), as Haiku does in 7 to 9 of 18 replies, and the 8B often runs past 25 words. DeepSeek leaked none under the same prompt.

### The earlier spike's three messages (first round)

| Message | Run | kind | headline | speech | notification |
|---|---|---|---|---|---|
| done | Haiku | `done_waiting` | Fixed flaky reattach_orphans test | Fixed a flaky test by adding a timeout and waiting on handshake. All daemon tests pass and the fix is committed. | Fixed flaky reattach_orphans test; daemon tests pass |
| done | DeepSeek | `done_waiting` | Flaky test fixed and committed | Flaky test fixed, full test suite and lint checks pass, and changes are committed. Nothing else is running. | Flaky test fixed; tests and lint pass; changes committed. |
| done | Granite 3B | `done_waiting` | Test fix completed | The flaky reattach_orphans test has been fixed. Cargo test passes and clippy is clean. The fix is committed. | Flaky test fixed. Cargo test passes. |
| done | Granite 8B | `done_waiting` | Test fix completed | Fixed the flaky reattach_orphans test with a 5-second timeout; cargo test passes and clippy is clean. | Test fix done. Ready for next instructions. |
| question | Haiku | `needs_answer` | Duplicate emails in migration | Found 37 duplicate email rows in staging. Should I keep the newest per email, merge into oldest, or skip the unique index? | 37 duplicate emails in staging—choose keep newest, merge oldest, or skip index |
| question | DeepSeek | `needs_answer` | Decide how to handle duplicate emails | Thirty-seven duplicate email rows are blocking the unique index. Should I keep the newest, merge into the oldest, or skip the index? | 37 duplicate email rows: keep newest, merge to oldest, or skip unique index? |
| question | Granite 3B | `needs_answer` | Migration Decision Needed | The agent needs your choice: keep newest rows and delete others, merge rows, or skip the unique index. | Choose option 1, 2, or 3 for migration. |
| question | Granite 8B | `needs_answer` | Decision needed for duplicate emails | The migration is ready except for 37 duplicate rows in users.email. Choose: keep newest row and delete others, merge into oldest row, or skip the unique index. | Pick an option: keep newest row, merge into oldest, or skip unique index |
| update | Haiku | `done_waiting` | Alerts plan added to main | Added the alerts architecture to the plan. Depends on hook-reported status. Three other tasks still running. | Alerts plan committed. Awaiting checkpoint from three implementers. |
| update | DeepSeek | `working_update` | Plan updated; three implementers still running | The plan is updated, and three implementers are still running: a rebase and two code changes. | Plan updated. Three implementers still running: a rebase and two changes. |
| update | Granite 3B | `working_update` | Daemon plan active | The daemon plan is running with multiple tasks still in progress. | Daemon plan active with ongoing tasks. |
| update | Granite 8B | `working_update` | Plan updated, work still running | I added the plan details about the daemon's one-shot call and hooks. Work is still in progress with background tasks running. | Plan updated. Background tasks (rebase, worktree change) still running. |

The 3B's "Choose option 1, 2, or 3" names options the listener never heard, and its update speech says nothing about what is running. The 8B's question speech is 27 words.

## Results: quality against DeepSeek (43 samples, before the Haiku ruling)

| Run | calls | strict JSON | kind agreement | cap violations | leaks |
|---|---|---|---|---|---|
| DeepSeek, second round against first | 43 | 93.0% (40/43) | 97.5% (39/40) | 0 | 0 |
| Granite 4.2 3B, GPU (2 rounds) | 86 | 100% | 71.4% (60/84) | 6 | 20 |
| Granite 4.2 3B, CPU | 43 | 100% | 76.2% (32/42) | 4 | 11 |
| Granite 4.2 8B, GPU (2 rounds) | 86 | 100% | 92.9% (78/84) | 16 | 38 |
| Granite 4.2 8B, CPU | 43 | 100% | 92.9% (39/42) | 6 | 19 |

The 3B mostly read questions as `working_update` when work was also running, and implementer fix rounds as `done_waiting`. Over all 126 DeepSeek calls, 9 (7.1%) hit `max_tokens` 1500 inside the thinking block and returned no usable text, and 10 took longer than the design's 8 s total timeout.

## Results: latency

Warm calls exclude the first call after load. Token rates are llama-server's medians; the prompt is about 540 tokens, of which the instruction prefix (about 370) is served from the prompt cache after the first call.

| Run | samples | load | cold call | warm p50 | warm p95 | warm max | prompt tok/s | gen tok/s |
|---|---|---|---|---|---|---|---|---|
| Granite 3B, GPU, smoke | 18 × 2 | 5.77 s | 0.56 s | 0.42 s | 0.51 s | 0.53 s | 6,011 | 187 |
| Granite 8B, GPU, smoke | 18 × 2 | 3.23 s | 0.85 s | 0.82 s | 1.12 s | 1.13 s | 3,900 | 103 |
| Granite 3B, GPU | 43 × 2 | 2.38 s | 2.55 s | 0.41 s | 0.55 s | 1.47 s | 3,594 | 186 |
| Granite 8B, GPU | 43 × 2 | 13.67 s | 2.79 s | 0.77 s | 1.00 s | 1.13 s | 2,292 | 104 |
| Granite 3B, CPU | 43 | 1.63 s | 5.91 s | 4.28 s | 5.71 s | 6.83 s | 363 | 18.5 |
| Granite 8B, CPU | 43 | 2.21 s | 11.21 s | 10.30 s | 15.61 s | 18.99 s | 232 | 7.8 |
| Haiku, no thinking, CLI | 18 × 2 | – | 5.08 s | 4.42–10.99 s | 20.64–51.60 s | 67.32 s | – | – |
| DeepSeek Flash | 63 × 2 | – | 3.33 s | 3.08–3.16 s | 8.67–8.79 s | 10.11 s | – | – |

Over the 14 smoke samples the CPU passes also covered, the 3B's p95 is 5.15 s and the 8B's 14.07 s. The very first call after llama-server was installed took 21.8 s (prompt evaluation at 28 tokens per second) while Vulkan built its pipelines; later server starts' first calls took 0.56 to 2.8 s. Load time depends on whether the model file is in the OS cache (2.4 to 13.7 s).

The Haiku wall times are CLI process times: its own figures put the API time at p50 1.52 s and p95 2.38 s, and the rest is CLI start-up. Without thinking a call cost a median $0.0029 ($0.111 for all 36 calls); with thinking at low effort, $0.0118 and 14.3 s of API time at the median.

## Results: memory

VRAM is nvidia-smi's `memory.used` after load minus the reading before start. Working set and private bytes are the server process's, after load and after the pass; the working set includes the memory-mapped model file.

| Run | VRAM | working set (loaded / after) | private bytes (loaded / after) |
|---|---|---|---|
| Granite 3B, GPU | 2,724 MiB | 2,124 / 2,816 MiB | 2,892 / 3,901 MiB |
| Granite 8B, GPU | 6,273 MiB | 5,008 / 6,174 MiB | 6,460 / 8,316 MiB |
| Granite 3B, CPU | 86 MiB | 2,756 / 3,565 MiB | 879 / 1,738 MiB |
| Granite 8B, CPU | 133 MiB | 6,280 / 7,733 MiB | 1,581 / 3,080 MiB |

## Against the rule

| Criterion | Bar | Granite 3B | Granite 8B |
|---|---|---|---|
| Kind agreement with Haiku | ≥ 95% (Haiku's 100% minus 5) | 72.2%: fail | 75.0%: fail |
| JSON valid | ≥ 95% (grammar if needed) | 100%, no grammar: pass | 100%, no grammar: pass |
| Warm GPU p95 | < 3 s | 0.51 s: pass | 1.12 s: pass |
| CPU p95 | < 8 s, else GPU-only | 5.15 s: pass | 14.07 s: GPU-only |

## What the daemon would need

If a later model passes, the change is one provider row, not a new client: an OpenAI-compatible entry with base URL `http://127.0.0.1:<port>` (llama-server or Ollama), path `/v1/chat/completions`, the model name, no key, $0 prices for the ledger, and `chat_template_kwargs: {"enable_thinking": false}` for a reasoning model's template.

No grammar was needed: every local reply parsed as plain JSON. The design's caps and fence-tolerant parser apply unchanged. A local provider would also free the alert text from the privacy line, since nothing leaves the PC.

The runs also bear on the providers already in the design. DeepSeek's `max_tokens` 1500 truncated 7.1% of replies here, and 10 of 126 calls ran past the 8 s timeout. Haiku through the Messages API would need thinking off, and the CLI's `--effort low` is not the same thing.

## Next

Untried here, each a separate run: Granite with low-effort thinking (more tokens, still well under 3 s on GPU at these rates); prompt v3's rule against identifiers, which would test the leaks; another 7–8B instruct family. Any of them should be judged against a reference set labelled by the owner, since the cloud references disagree with each other on about one sample in five.
