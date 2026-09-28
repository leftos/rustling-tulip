# Spike: alerts for waiting agents, summarized by Haiku

2026-09-28. For the "Spoken and phone alerts when an agent waits" item in [plan.md](../plan.md). Question: what does a cheap model produce from an agent's last message, and what does each alert cost and take?

Files in [waiting-alerts/](./waiting-alerts/): the prompts (`prompt-v1.md`, `prompt-v2.md`), three test messages, and `run.py` (`python run.py <prompt> <message> <session name> [lean]`), which fills the template and pipes it to `claude -p --model haiku`.

## Prompt v2

`prompt-v2.md` asks for JSON with three fields:

- `kind`: `needs_answer` (the agent asks something and can't go on), `working_update` (it reported progress but work it started still runs), or `done_waiting` (it finished and waits for new work). This lets each kind carry its own notification settings.
- `speech`: at most 25 words for a TTS voice, with no paths, hashes, code or markdown.
- `notification`: at most 100 characters.

Neither text names the session: the app prefixes the repo's or workspace's name itself, or its spoken name where the user set one.

## Results (v2, `lean` flags)

| Message | kind | speech | wall |
|---|---|---|---|
| `message-update.md` (an orchestrator's status, three implementers still running) | `working_update` | "Added plan items. Three implementations still running: P4.9 rebase, P4.13b, worktree-path change." | 17.1 s |
| `message-done.md` (a fix committed, nothing running) | `done_waiting` | "Fixed the flaky reattach_orphans test. All tests pass, code is clean, changes are committed." | 32.8 s |
| `message-question.md` (three options on duplicate rows) | `needs_answer` | "Migration needs your decision on 37 duplicate emails: keep newest, merge oldest, or skip the index?" | 9.1 s |

All three kinds were right. The speech still leaks identifiers a voice reads badly ("P4.9", "P4.13b", "reattach_orphans"). v1 (no `kind`, name first) gave similar texts in 20–30 s.

## Cost and latency

- **Through the CLI** (`claude -p --model haiku`, with `--setting-sources "" --strict-mcp-config --tools ""`): one alert reported 9 input tokens, 7,161 cache-read tokens (the CLI's own system prompt), 2,452 output tokens and `total_cost_usd` 0.013, with 17.2 s of API time. At 100 alerts a day that is about $1.30.
- **Direct Messages API** (allowed, user 2026-09-28; `waiting-alerts/api.py`, key from `ANTHROPIC_API_KEY_TOAST`), measured on the same three messages with `claude-haiku-4-5` at $1 / $5 per million tokens:

  | Message | kind | wall | tokens in / out | cost |
  |---|---|---|---|---|
  | `message-update.md` | `working_update` | 1.29 s | 595 / 67 | $0.00093 |
  | `message-done.md` | `done_waiting` | 1.05 s | 447 / 55 | $0.00072 |
  | `message-question.md` | `needs_answer` | 1.44 s | 437 / 67 | $0.00077 |

  About $0.08 at 100 alerts a day, and 15–30 times faster than the CLI. The kinds matched the CLI runs; the speech dropped one identifier ("reattach orphans") but kept "P4.13b".
- `--bare` would trim the CLI's startup, but it reads only `ANTHROPIC_API_KEY`, so it needs a key too.

## Next

- Haiku wraps its JSON in a code fence though the prompt says not to: use structured outputs (`output_config.format`) in v3 instead of parsing around the fence.
- Iterate the prompt against identifiers in speech: spell out or drop plan numbers and function names.
