# Linear issue links

Linear issue identifiers (`RT-116`) and Linear issue URLs are detected in terminal text and in session titles. Hovering one shows a card with the issue's title and status; clicking opens it in the Linear app, or on the web when the app is missing. Linear issue: RT-133.

Research: [linear-links.md](../research/linear-links.md). Code map at the time of design: links in `apps/native/src/links.rs` (`LinkKind { Url, Path }`), Ctrl-hover detection and Ctrl+click in `term_view.rs`, opening in `open_view.rs` / `open.rs`, labels through `display_label` (`sidebar.rs`), tooltips through `lib.rs` (`Tip`), popovers through `session_menu.rs` (`popover_frame`). Mock: [canvases/linear-card/linear-card.html](./canvases/linear-card/linear-card.html).

## Rulings

- **Card (owner):** the Standard card, mock section 1 lane B. It shows the identifier, title, state (Linear's state colour), priority, project, release, assignee and when it was last updated.
- **Session titles (owner):** an identifier in a session title is drawn as a state chip, mock section 2 lane B: a small chip holding the identifier and a dot in Linear's state colour. Hover shows the card, click opens the issue. This applies in the pane header and the sidebar leaf.
- **Credential (owner):** the daemon reads `LINEAR_API_KEY` through `user_env::resolve` (process environment, then `HKCU\Environment`), and through the env-secrets store once Wave 1b merges. A read-only personal API key scoped to the owner's teams is enough (`Authorization: {key}`, no `Bearer`).
- **Detection (owner):** an identifier counts only when its team key is one of the workspace's teams. The daemon fetches the keys (`teams { nodes { key } }`) and refreshes them daily. Any `linear.app/{workspace}/issue/{ID}` URL also counts. With no key configured, only URLs are detected.

## Open questions

Asked in the next interview rounds: chip and card refresh cadence, hover timing in the terminal, what the card shows with no key or offline, the phone's behaviour, the branch, and where the work sits in the plan.

## Steps

To be split once the open questions are answered.
