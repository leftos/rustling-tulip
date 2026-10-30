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

mod exe_subsystem;
mod ui_agent_exit;
mod ui_agent_options;
mod ui_appearance;
mod ui_appearance_editor;
mod ui_changes;
mod ui_chat;
mod ui_commit;
mod ui_connections;
mod ui_container_menu;
mod ui_dashboard;
mod ui_delete_worktree;
mod ui_device_pairing;
mod ui_diff_tab;
mod ui_diff_view;
mod ui_ellipsis;
mod ui_fetch_dialog;
mod ui_file_fetch;
mod ui_font_size;
mod ui_history;
mod ui_keys;
mod ui_launch_last;
mod ui_layout_chooser;
mod ui_links;
mod ui_multi_host;
mod ui_needs_you;
mod ui_notifications;
mod ui_paint;
mod ui_pairing;
mod ui_pane_drag;
mod ui_pane_spawn;
mod ui_panes;
mod ui_popout;
mod ui_preset_wizard;
mod ui_prompt_jump;
mod ui_quit;
mod ui_recover;
mod ui_remote;
mod ui_remote_access;
mod ui_repo_remove;
mod ui_repos;
mod ui_resume_report;
mod ui_session_actions;
mod ui_settings;
mod ui_shell;
mod ui_shell_integration;
mod ui_shortcuts;
mod ui_sidebar;
mod ui_sidebar_drag;
mod ui_source_control;
mod ui_spawn;
mod ui_spawn_combobox;
mod ui_spawn_dialog;
mod ui_spawn_preview;
mod ui_stashes;
mod ui_summary;
mod ui_tab_drag;
mod ui_tabs;
mod ui_target_switch;
mod ui_terminal;
mod ui_theme;
mod ui_themes;
mod ui_themes_edit;
mod ui_update;
mod ui_viewer;
mod ui_viewer_md;
mod ui_viewer_tabs;
mod ui_vscode_prompt;
mod ui_workspace_creator;
mod ui_worktree_cleanup;
mod ui_worktrees;
