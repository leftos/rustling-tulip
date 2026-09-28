I've added this to the plan (049fc4a, on local main, not pushed under the new rule):
- Summary: the daemon makes a one-shot claude -p --model haiku call, so the no-direct-API rule holds. Each alert uses a little subscription usage.
- Voice: Windows' built-in voices, offline, with the voice picked in Settings.
- Phone: waits for the mobile app's push notifications (MA7). No stopgap service in the meantime.
- Triggers: a finished turn waiting for input, permission prompts, and AskUserQuestion interviews. How an AskUserQuestion reaches the daemon (a hook or the transcript) still has to be checked against the Claude Code docs; the plan line says so.

It depends on the hook-reported status item, which is first in that list.

main is one commit ahead of origin. I'll offer /ship when the slice checkpoints. Three implementers are still running: P4.9's rebase, P4.13b and the worktree-path change.
