# Non-git folders in the sidebar

Design for RT-932: a folder that is not a git repository can be added to the sidebar, and the spawn dialog shows no worktree or branch choice for it. Every decision below is the orchestrator's; each is open to the owner.

## What the owner asked

"Allow adding non-git folders as "repos" or "folders". Don't show the option to create a worktree or pick a git branch if the folder I'm creating a new session into is a non-git folder."

## What exists

- The daemon already accepts a non-git folder: `registry::add_repo` checks only that the path is a directory, and `spawn_single` skips worktree and branch work when `git::is_initialized` is false.
- The client does not know which repos are git: `RepoEntry` has only `default_branch`, which is also empty for a repo with no commits. The spawn dialog always shows the Worktree toggle and the branch and base fields, and the Add repo hints say the folder "holds a git repo".
- A workspace may list a non-git member, and `workspace.rs` runs branch and worktree steps on every member.

## Decisions

1. **One kind of entry, with a flag.** A folder stays a `RepoEntry` in `state.json` and on the wire, with an additive `#[serde(default = ...)] is_git: bool` field that reads as `true` when absent. `ContainerRef` keeps its two variants (it has no `#[serde(other)]`, so a third would break an older client). No protocol version bump.
2. **The daemon owns the flag.** It is true when the folder's root holds `.git` (a folder or a file, as `history.rs` judges `folder_is_git_repo`), so a git repo with no commits counts as git. The daemon sets it when a repo is added and refreshes it for every repo at start and before each `Repos` broadcast, so a folder that later gets `git init` turns into a repo without being added again. An older `state.json` without the field loads, and the first refresh corrects any entry that is not git; a test loads a `v0.5.1-alpha`-shaped `state.json` and checks both.
3. **Wording.** The sidebar and the spawn Target list tag a non-git entry `[DIR]`, the tag the sidebar already gives an unregistered folder; a git one keeps `[REPO]`. The Add repo action keeps its name, and its hints say "a git repo or any folder" instead of "holds a git repo".
4. **Spawn dialog.** For a single non-git target the Worktree toggle and the branch and base fields are not drawn, and the spawn is sent with `use_worktree: false` and no branch. A non-git entry's `default_use_worktree` is false when it is added. Launch-last and duplicate replay a non-git target's saved config the same way.
5. **Workspaces with folders.** A workspace may hold non-git members. Its spawn keeps the worktree and branch choice when at least one member is git; the daemon applies them to the git members and runs each non-git member in its own folder. A workspace with no git member shows neither choice.
6. **Other git surfaces.** For a non-git entry the sidebar shows no branch chip, the container menu leaves out its git items (source control, worktrees, stashes, fetch), and the git watcher does not watch it. Recover offers "register as repo" for a non-git folder too, registering it as a folder.

## Steps

- [ ] NG.1: protocol `RepoEntry.is_git` (doc comment, `docs/wire-protocol.md`); the daemon's flag, its refresh, `default_use_worktree` for a folder, the workspace spawn's per-member skip, and the git watcher's skip; the state test in the oldest released shape; daemon tests.
- [ ] NG.2: the spawn dialog's hide rules (`spawn_form.rs`, `spawn_view.rs`), the `[DIR]` tag in the Target list, launch-last and duplicate; spawn specs.
- [ ] NG.3: the sidebar tag and branch chip, the container menu's git items, the Add repo hints, recover's register offer; `docs/native-client.md`.
