//! The native client's UI specs, one module per area, linked as one test
//! binary: the root view on a test window over the scripted fake daemon in
//! `tests/support`.
#![expect(
    clippy::expect_used,
    reason = "the shared fake daemon fails a spec with the message of the precondition it lost"
)]

#[expect(
    dead_code,
    reason = "the UI specs use the fake daemon, not the live e2e and smoke helpers that share the module"
)]
#[path = "../support/mod.rs"]
mod support;

mod ui_appearance;
mod ui_appearance_editor;
mod ui_changes;
mod ui_commit;
mod ui_delete_worktree;
mod ui_diff_tab;
mod ui_diff_view;
mod ui_ellipsis;
mod ui_font_size;
mod ui_headless;
mod ui_history;
mod ui_layout_chooser;
mod ui_links;
mod ui_needs_you;
mod ui_notifications;
mod ui_pane_spawn;
mod ui_panes;
mod ui_quit;
mod ui_recover;
mod ui_repos;
mod ui_session_actions;
mod ui_settings;
mod ui_shell;
mod ui_shell_integration;
mod ui_shortcuts;
mod ui_sidebar;
mod ui_source_control;
mod ui_spawn;
mod ui_spawn_combobox;
mod ui_spawn_dialog;
mod ui_spawn_preview;
mod ui_stashes;
mod ui_tabs;
mod ui_terminal;
mod ui_worktree_cleanup;
mod ui_worktrees;
