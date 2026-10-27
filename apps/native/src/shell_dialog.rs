//! The standalone-shell spawn and the Shell… dialog's state — how the folder
//! is judged, whether to remember it, the keyboard focus ring, the folder
//! picker's answers and the folder waiting on a spawn. Plain Rust, so every
//! rule is unit-tested; `shell_view` renders it and forwards input, and the
//! spawn goes through [`crate::RootView::spawn`].

use std::collections::HashMap;
use std::path::Path;

use protocol::{SessionMode, SpawnRequest, SpawnTarget};

use crate::repo_add::check_host_path_rules;

/// A focusable control of the Shell… dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellControl {
    Close,
    Folder,
    Browse,
    SaveDefault,
    /// Forget the remembered quick-shell folder.
    ClearDefault,
    Cancel,
    Submit,
}

impl ShellControl {
    /// The debug selector the control's element carries.
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Close => "shell-close",
            Self::Folder => "shell-folder",
            Self::Browse => "shell-browse",
            Self::SaveDefault => "shell-save-default",
            Self::ClearDefault => "shell-clear-default",
            Self::Cancel => "shell-cancel",
            Self::Submit => "shell-submit",
        }
    }
}

/// The folder the dialog shows as an example while its field is empty.
pub(crate) const FOLDER_PLACEHOLDER: &str = "C:\\Users\\you";

/// The folder field's example on remote host `host` (its short name), whose
/// OS this PC does not know.
pub(crate) fn host_placeholder(host: &str) -> String {
    format!("A full path on {host}")
}

/// What the dialog says while the folder is something but not a full path.
pub(crate) const PATH_HINT: &str = "Enter a full path, like C:\\Users\\you";

/// The Shell… dialog's state. The folder itself lives in the dialog's text
/// field, and every rule here reads the text it is given.
#[derive(Debug)]
pub(crate) struct ShellForm {
    /// This dialog's number, so a folder picker's answer reaches only the
    /// dialog that pressed Browse.
    generation: u64,
    /// The field's text when Browse was pressed, while its picker is open.
    browsing: Option<String>,
    quick_shell: QuickShellDefault,
    save_default: bool,
    focus: ShellControl,
    submitted: bool,
    /// The remote host's short name the shell opens on, whose path rules
    /// judge the folder; `None` for this PC.
    host: Option<String>,
}

/// What the dialog offers about the folder "+ Shell" opens in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuickShellDefault {
    /// Neither to remember the folder nor to forget one: a folder of this PC
    /// means nothing to a remote host's daemon.
    NotOffered,
    /// To remember the folder; none was remembered when the dialog opened.
    Unset,
    /// To remember the folder, or to forget the one remembered when the
    /// dialog opened.
    Remembered,
}

/// What a submit asks for: the folder to open in, and whether to remember it
/// for the quick shell.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ShellSubmission {
    pub folder: String,
    pub save_default: bool,
}

impl ShellForm {
    /// The dialog numbered `generation`, over `remembered`, the quick shell's
    /// folder; the checkbox starts ticked and the folder field starts with
    /// the keyboard.
    pub(crate) fn new(generation: u64, remembered: Option<&str>) -> Self {
        Self {
            generation,
            browsing: None,
            quick_shell: if remembered.is_some() {
                QuickShellDefault::Remembered
            } else {
                QuickShellDefault::Unset
            },
            save_default: true,
            focus: ShellControl::Folder,
            submitted: false,
            host: None,
        }
    }

    /// The dialog numbered `generation` on remote host `host` (the
    /// connection's short name): it neither remembers its folder for
    /// "+ Shell" nor offers to forget one, and judges the folder by the host
    /// path rules, since the host may run another OS than this PC.
    pub(crate) fn without_default(generation: u64, host: &str) -> Self {
        Self {
            quick_shell: QuickShellDefault::NotOffered,
            host: Some(host.to_owned()),
            ..Self::new(generation, None)
        }
    }

    /// This dialog's number.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a folder is remembered for "+ Shell" and the dialog offers
    /// to forget it.
    pub(crate) fn remembered(&self) -> bool {
        self.quick_shell == QuickShellDefault::Remembered
    }

    /// Whether the "Use as quick shell default" checkbox is offered.
    pub(crate) fn offers_default(&self) -> bool {
        self.quick_shell != QuickShellDefault::NotOffered
    }

    /// Whether the folder picker is open on this dialog, which disables
    /// Browse.
    pub(crate) fn browsing(&self) -> bool {
        self.browsing.is_some()
    }

    /// Browse was pressed with the field holding `folder`.
    pub(crate) fn begin_browse(&mut self, folder: &str) {
        self.browsing = Some(folder.to_owned());
    }

    /// What the folder picker answered: the folder to put in the field, or
    /// `None` when there is nothing to apply. Only this dialog's own answer,
    /// with the field unchanged since Browse was pressed, is applied; a
    /// cancel, a stale dialog and an edit meanwhile are dropped.
    pub(crate) fn finish_browse(
        &mut self,
        generation: u64,
        current: &str,
        picked: Option<&str>,
    ) -> Option<String> {
        if generation != self.generation {
            tracing::debug!(
                generation,
                dialog = self.generation,
                "a folder picker answered a dialog that did not ask"
            );
            return None;
        }
        let Some(started_with) = self.browsing.take() else {
            tracing::debug!("a folder picker answered a dialog that was not browsing");
            return None;
        };
        if started_with != current {
            tracing::debug!("a folder picker's answer is dropped: the folder was edited meanwhile");
            return None;
        }
        picked.map(str::to_owned)
    }

    /// Whether Open shell would remember the folder for the quick shell.
    pub(crate) fn save_default(&self) -> bool {
        self.offers_default() && self.save_default
    }

    pub(crate) fn toggle_save_default(&mut self) {
        self.save_default = !self.save_default;
    }

    /// Why `trimmed`, a folder that is not empty, cannot be opened: on this
    /// PC [`PATH_HINT`] unless it is a full path here (a drive or UNC path on
    /// Windows); on a remote host the Add repo prompt's refusal naming it,
    /// unless it passes the host path rules.
    fn refusal(&self, trimmed: &str) -> Option<String> {
        match &self.host {
            None => (!Path::new(trimmed).is_absolute()).then(|| PATH_HINT.to_owned()),
            Some(host) => check_host_path_rules(trimmed)
                .err()
                .map(|problem| problem.message(host)),
        }
    }

    /// The folder to open in: `folder` without the spaces around it, when it
    /// is a full path by the rules of the machine the shell runs on.
    fn openable<'a>(&self, folder: &'a str) -> Option<&'a str> {
        let trimmed = folder.trim();
        (!trimmed.is_empty() && self.refusal(trimmed).is_none()).then_some(trimmed)
    }

    /// Whether Open shell can go with the field holding `folder`.
    pub(crate) fn can_submit(&self, folder: &str) -> bool {
        self.openable(folder).is_some()
    }

    /// What the field says while its text is something it cannot open: not
    /// a full path, or on a remote host a path with a `..` segment.
    pub(crate) fn hint(&self, folder: &str) -> Option<String> {
        let trimmed = folder.trim();
        if trimmed.is_empty() {
            return None;
        }
        self.refusal(trimmed)
    }

    /// The focusable controls, in the order they are drawn; Open shell joins
    /// them only while it can go, the checkbox only while the dialog offers
    /// a quick-shell default, the clear link only while there is a folder
    /// to clear, and Browse only while `browse` (a picker on this PC can
    /// answer for the daemon).
    pub(crate) fn controls(&self, folder: &str, browse: bool) -> Vec<ShellControl> {
        let mut ring = vec![ShellControl::Close, ShellControl::Folder];
        if browse {
            ring.push(ShellControl::Browse);
        }
        if self.offers_default() {
            ring.push(ShellControl::SaveDefault);
        }
        if self.remembered() {
            ring.push(ShellControl::ClearDefault);
        }
        ring.push(ShellControl::Cancel);
        if self.can_submit(folder) {
            ring.push(ShellControl::Submit);
        }
        ring
    }

    /// The focused control; one that is gone gives way to the first.
    pub(crate) fn focused(&self, folder: &str, browse: bool) -> ShellControl {
        let ring = self.controls(folder, browse);
        if ring.contains(&self.focus) {
            self.focus
        } else {
            ShellControl::Close
        }
    }

    pub(crate) fn set_focus(&mut self, control: ShellControl) {
        self.focus = control;
    }

    /// Tab (`forward`) or Shift+Tab: the next or previous control, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool, folder: &str, browse: bool) {
        let ring = self.controls(folder, browse);
        let at = ring
            .iter()
            .position(|c| *c == self.focused(folder, browse))
            .unwrap_or(0);
        let len = ring.len();
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        if let Some(control) = ring.into_iter().nth(next) {
            self.focus = control;
        }
    }

    /// Open one: the folder to open in and whether to remember it, once;
    /// `None` while the field holds no full path, and after it went.
    pub(crate) fn submit(&mut self, folder: &str) -> Option<ShellSubmission> {
        if self.submitted {
            return None;
        }
        let folder = self.openable(folder)?.to_owned();
        self.submitted = true;
        Some(ShellSubmission {
            folder,
            save_default: self.save_default(),
        })
    }
}

/// The spawn a standalone shell is: a plain shell in `cwd`, or in the
/// daemon's own default (the user's home) with `None`.
pub(crate) fn standalone_shell_request(cwd: Option<String>) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target: SpawnTarget::Standalone {
            cwd,
            add_dirs: Vec::new(),
        },
        mode: SessionMode::PlainShell,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent: None,
        extra_env: Vec::new(),
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: None,
        resume_conversation: None,
    }
}

/// Quick-shell folders waiting for the spawn they came with to land, by
/// request id: the folder becomes the remembered one only then.
#[derive(Debug, Default)]
pub(crate) struct PendingQuickShell {
    folders: HashMap<String, String>,
}

impl PendingQuickShell {
    /// Remembers `folder` for "+ Shell" once spawn `request_id` lands.
    pub(crate) fn arm(&mut self, request_id: &str, folder: &str) {
        self.folders
            .insert(request_id.to_owned(), folder.to_owned());
    }

    /// Spawn `request_id` landed: the folder to remember, when one waited on
    /// it.
    pub(crate) fn succeed(&mut self, request_id: &str) -> Option<String> {
        self.folders.remove(request_id)
    }

    /// Spawn `request_id` failed: nothing is remembered for it.
    pub(crate) fn fail(&mut self, request_id: &str) {
        self.folders.remove(request_id);
    }

    /// Forgets every waiting folder, as a lost connection must.
    pub(crate) fn clear(&mut self) {
        self.folders.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_add::HostPathProblem;

    #[test]
    fn only_a_full_path_can_be_opened() {
        let form = ShellForm::new(1, None);
        for folder in ["", "   ", "work", "sub\\dir", ".\\x", "C:work"] {
            assert!(!form.can_submit(folder), "{folder:?} opens nothing");
        }
        for folder in ["C:\\work", "C:/work", "\\\\server\\share", "  C:\\work  "] {
            assert!(form.can_submit(folder), "{folder:?} is a full path");
            assert!(form.hint(folder).is_none(), "{folder:?} shows no hint");
        }
        for folder in ["", "  "] {
            assert!(
                form.hint(folder).is_none(),
                "an empty field asks for nothing"
            );
        }
        assert!(
            form.hint(" work ").is_some(),
            "a relative folder asks for a full path"
        );
        assert!(form.hint("C:work").is_some(), "a drive-relative one too");
    }

    #[test]
    fn local_shell_folder_check_is_unchanged() {
        let mut form = ShellForm::new(1, Some("C:\\work"));
        assert!(
            !form.can_submit("/home/me"),
            "a POSIX path is no full path on this PC"
        );
        assert_eq!(form.hint("/home/me").as_deref(), Some(PATH_HINT));
        assert_eq!(form.hint("work").as_deref(), Some(PATH_HINT));
        assert_eq!(form.hint("C:\\work"), None);
        assert_eq!(
            form.submit(" C:\\work "),
            Some(ShellSubmission {
                folder: "C:\\work".to_owned(),
                save_default: true,
            })
        );
    }

    #[test]
    fn remote_shell_folder_accepts_a_posix_host_path() {
        let mut form = ShellForm::without_default(1, "box");
        for folder in ["/home/me", "/", "C:\\work", "\\\\server\\share"] {
            assert!(form.can_submit(folder), "{folder:?} is a full host path");
            assert_eq!(form.hint(folder), None, "{folder:?} shows no hint");
        }
        assert_eq!(
            form.submit("  /home/me/src  "),
            Some(ShellSubmission {
                folder: "/home/me/src".to_owned(),
                save_default: false,
            })
        );
    }

    #[test]
    fn remote_shell_folder_refuses_a_relative_host_path_with_the_host_wording() {
        let mut form = ShellForm::without_default(1, "box");
        let not_absolute = Some(HostPathProblem::NotAbsolute.message("box"));
        for folder in ["work", "home/me", "C:work", "\\work"] {
            assert!(!form.can_submit(folder), "{folder:?} opens nothing");
            assert_eq!(form.hint(folder), not_absolute, "{folder:?}");
        }
        assert_eq!(
            form.hint("/home/../etc"),
            Some(HostPathProblem::ParentSegment.message("box"))
        );
        assert_eq!(form.hint("  "), None, "an empty field asks for nothing");
        assert_eq!(form.submit("work"), None);
    }

    #[test]
    fn remote_shell_folder_placeholder_names_the_host() {
        assert_eq!(host_placeholder("box"), "A full path on box");
        assert_eq!(FOLDER_PLACEHOLDER, "C:\\Users\\you", "this PC's stays");
    }

    #[test]
    fn submit_gives_the_trimmed_folder_and_the_save_choice_once() {
        let mut form = ShellForm::new(1, None);
        assert!(form.save_default(), "the box starts ticked");
        assert!(
            form.submit("work").is_none(),
            "a relative folder goes nowhere"
        );
        assert_eq!(
            form.submit("  C:\\work\\deep  "),
            Some(ShellSubmission {
                folder: "C:\\work\\deep".to_owned(),
                save_default: true,
            })
        );
        assert!(
            form.submit("C:\\work\\deep").is_none(),
            "a submit goes once"
        );

        let mut unticked = ShellForm::new(2, Some("C:\\work"));
        unticked.toggle_save_default();
        assert!(!unticked.save_default());
        assert_eq!(
            unticked.submit("C:\\work"),
            Some(ShellSubmission {
                folder: "C:\\work".to_owned(),
                save_default: false,
            })
        );
    }

    #[test]
    fn the_dialog_opens_on_the_field_with_the_box_ticked() {
        let form = ShellForm::new(7, Some("C:\\work"));
        assert_eq!(form.generation(), 7);
        assert!(form.remembered(), "there is a folder to clear");
        assert!(form.save_default());
        assert_eq!(form.focused("C:\\work", true), ShellControl::Folder);
        assert!(!form.browsing());

        let fresh = ShellForm::new(8, None);
        assert_ne!(fresh.generation(), form.generation());
        assert!(!fresh.remembered());
    }

    #[test]
    fn every_control_has_its_own_selector() {
        let mut seen = Vec::new();
        for control in [
            ShellControl::Close,
            ShellControl::Folder,
            ShellControl::Browse,
            ShellControl::SaveDefault,
            ShellControl::ClearDefault,
            ShellControl::Cancel,
            ShellControl::Submit,
        ] {
            let selector = control.selector();
            assert!(selector.starts_with("shell-"), "{selector}");
            assert!(!seen.contains(&selector), "{selector} twice");
            seen.push(selector);
        }
    }

    #[test]
    fn the_focus_ring_wraps_and_follows_the_folder() {
        let mut form = ShellForm::new(1, Some("C:\\old"));
        assert_eq!(
            form.controls("C:\\work", true),
            [
                ShellControl::Close,
                ShellControl::Folder,
                ShellControl::Browse,
                ShellControl::SaveDefault,
                ShellControl::ClearDefault,
                ShellControl::Cancel,
                ShellControl::Submit,
            ]
        );
        form.set_focus(ShellControl::ClearDefault);
        form.move_focus(true, "C:\\work", true);
        assert_eq!(form.focused("C:\\work", true), ShellControl::Cancel);
        form.move_focus(true, "C:\\work", true);
        assert_eq!(form.focused("C:\\work", true), ShellControl::Submit);
        form.move_focus(true, "C:\\work", true);
        assert_eq!(form.focused("C:\\work", true), ShellControl::Close, "wraps");
        form.move_focus(false, "C:\\work", true);
        assert_eq!(
            form.focused("C:\\work", true),
            ShellControl::Submit,
            "and back"
        );

        assert_eq!(
            form.focused("relative", true),
            ShellControl::Close,
            "a folder that cannot open drops Submit and the focus gives way"
        );
        assert!(
            !form
                .controls("relative", true)
                .contains(&ShellControl::Submit)
        );

        let plain = ShellForm::new(2, None);
        assert!(
            !plain
                .controls("C:\\work", true)
                .contains(&ShellControl::ClearDefault),
            "no remembered folder, no clear link"
        );
    }

    #[test]
    fn browse_leaves_the_ring_where_no_picker_answers() {
        let mut form = ShellForm::new(1, None);
        assert_eq!(
            form.controls("C:\\work", false),
            [
                ShellControl::Close,
                ShellControl::Folder,
                ShellControl::SaveDefault,
                ShellControl::Cancel,
                ShellControl::Submit,
            ]
        );
        form.set_focus(ShellControl::Folder);
        form.move_focus(true, "C:\\work", false);
        assert_eq!(
            form.focused("C:\\work", false),
            ShellControl::SaveDefault,
            "Tab steps over Browse"
        );
        form.move_focus(false, "C:\\work", false);
        assert_eq!(form.focused("C:\\work", false), ShellControl::Folder);

        form.set_focus(ShellControl::Browse);
        assert_eq!(
            form.focused("C:\\work", false),
            ShellControl::Close,
            "a focused Browse gives way once it leaves"
        );
        assert_eq!(form.focused("C:\\work", true), ShellControl::Browse);
    }

    #[test]
    fn a_picker_answer_needs_this_dialog_and_an_unchanged_field() {
        let mut form = ShellForm::new(7, None);
        assert!(!form.browsing());
        form.begin_browse("C:\\old");
        assert!(
            form.browsing(),
            "Browse is disabled while its picker is open"
        );
        assert_eq!(
            form.finish_browse(7, "C:\\old", Some("C:\\picked")),
            Some("C:\\picked".to_owned())
        );
        assert!(!form.browsing(), "the picker is over");
        assert_eq!(
            form.finish_browse(7, "C:\\old", Some("C:\\late")),
            None,
            "nothing is browsing here"
        );

        form.begin_browse("C:\\old");
        assert_eq!(
            form.finish_browse(7, "C:\\edited", Some("C:\\picked")),
            None,
            "the field was edited while the picker was open"
        );
        assert!(!form.browsing());

        form.begin_browse("C:\\old");
        assert_eq!(
            form.finish_browse(9, "C:\\old", Some("C:\\picked")),
            None,
            "another dialog pressed Browse"
        );
        assert!(form.browsing(), "this dialog's own picker is untouched");
        assert_eq!(form.finish_browse(7, "C:\\old", None), None, "a cancel");
        assert!(!form.browsing(), "which re-enables Browse");
    }

    #[test]
    fn a_folder_is_remembered_only_when_its_spawn_lands() {
        let mut pending = PendingQuickShell::default();
        pending.arm("q1", "C:\\work");
        pending.arm("q2", "C:\\other");
        assert_eq!(
            pending.succeed("q2").as_deref(),
            Some("C:\\other"),
            "the spawn that landed"
        );
        assert_eq!(pending.succeed("q2"), None, "once");

        pending.fail("q1");
        assert_eq!(
            pending.succeed("q1"),
            None,
            "a failed spawn remembers nothing"
        );

        pending.arm("q3", "C:\\third");
        pending.clear();
        assert_eq!(pending.succeed("q3"), None, "a lost connection forgets it");
        assert_eq!(pending.succeed("nope"), None);
    }

    #[test]
    fn the_request_is_a_plain_shell_in_the_folder() {
        let request = standalone_shell_request(Some("C:\\work".to_owned()));
        assert_eq!(request.label, None);
        assert_eq!(
            request.target,
            SpawnTarget::Standalone {
                cwd: Some("C:\\work".to_owned()),
                add_dirs: Vec::new(),
            }
        );
        assert_eq!(request.mode, SessionMode::PlainShell);
        assert_eq!(request.initial_prompt, None);
        assert!(!request.dangerously_skip_permissions);
        assert_eq!(request.agent, None);
        assert_eq!(
            request.extra_env,
            [] as [(std::string::String, std::string::String); 0]
        );
        assert!(request.prompt_injector.is_none());
        assert!(request.request_id.is_none(), "the view stamps its own");
        assert_eq!(
            standalone_shell_request(None).target,
            SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            "no folder: the daemon's own default"
        );
    }
}
