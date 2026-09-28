# Spike: how GUI mode is billed

Measured 2026-09-28 with Claude Code 2.1.284, on a Max plan with claude.ai login (`claude auth status`: `authMethod: claude.ai`, `subscriptionType: max`), with no `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or Bedrock/Vertex variable set.

**Question.** Is GUI mode, the conversation view in [borrowed-ideas.md](../plans/borrowed-ideas.md) that drives Claude through `claude --print --verbose --input-format stream-json --output-format stream-json`, billed against the subscription or as API credits?

**Answer.** Today it runs on the subscription. The terms, though, only cover personal use of the tool. Run GUI mode through the same logged-in `claude` binary the user runs themselves; never ship a build that signs other people in or routes their requests through a plan.

## Run

One user message (`Reply with just the word ok.`) on `--model haiku`, in an empty folder. It exited 0 and produced 15 events: `system/init`, 5 hook start/response pairs (the user's global hooks), 2 `assistant`, 1 `rate_limit_event` and 1 `result/success`.

| Event and field | Value | What it shows |
|---|---|---|
| `system/init` `.apiKeySource` | `"none"` | No API key was used; the request authenticated with the claude.ai OAuth login |
| `rate_limit_event` `.rate_limit_info` | `unifiedWindows.five_hour.utilization: 0.02`, `seven_day.utilization: 0.64`, `isUsingOverage: false`, `status: allowed_warning` | The request was checked against the plan's own 5-hour and 7-day usage windows, not a credit balance |
| `result` `.modelUsage.*.costUSD` / `.costBasis` | `0.0873` / `"list"` | An estimate at list price; the headless docs call `total_cost_usd` a client-side estimate that "can differ from your actual bill". It is not a charge |
| `result` `.fast_mode_disabled_reason` | `"sdk_opt_in_required"` | The CLI counts this run as SDK-style use. This is the category an announced billing change would target |

## Policy

- Anthropic's Legal and compliance page (`code.claude.com/docs/en/legal-and-compliance`, "Authentication and credential use") says OAuth "is designed to support ordinary use of Claude Code". It also says "Developers building products or services that interact with Claude's capabilities, including those using the Agent SDK, should use API key authentication", and that "Anthropic does not permit third-party developers to offer Claude.ai login or to route requests through Free, Pro, or Max plan credentials on behalf of their users." The same page says the Pro and Max limits "assume ordinary, individual usage of Claude Code and the Agent SDK".
- News reports (DevOps.com and VentureBeat; not re-read against an Anthropic primary source) say a change that would have given Agent SDK use its own monthly credit, separate from the plan's limits, was paused on its launch day, 2026-06-15, with no new date.

## What would change the answer

- The paused change is revived, and `--print` counts as Agent SDK use. The `sdk_opt_in_required` reason above suggests it would.
- rustling-tulip is distributed to other people who sign in with their own plans.

Re-run the spike after a Claude Code update, or on news of either: compare `apiKeySource`, the `rate_limit_event` windows and `isUsingOverage`. The input is one line of JSON on stdin, `{"type":"user","message":{"role":"user","content":"Reply with just the word ok."}}`. The terminal UI stays the fallback, since it is plainly "ordinary use of Claude Code".
