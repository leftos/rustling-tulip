//! The runtime icon a session wears: its agent's own icon while the agent is
//! up, a shell family's brand icon, or the generic shell glyph.

use gpui::{
    AnyElement, ImageSource, InteractiveElement, IntoElement, ParentElement, Pixels, Resource,
    SharedString, Styled, div, img, svg,
};
use protocol::{AgentDescriptor, SessionSnapshot};

use crate::assets::{
    RUNTIME_CMD_ICON, RUNTIME_POSIX_ICON, RUNTIME_POWERSHELL_ICON, RUNTIME_PWSH_ICON,
    SIDEBAR_SHELL_ICON, agent_icon_path,
};
use crate::ui_theme::UiTheme;

/// Which icon a runtime tag is drawn as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeIcon {
    /// The agent's own icon: the SVG markup its descriptor carries.
    Agent(SharedString),
    /// PowerShell 7 and later.
    Pwsh,
    /// Windows PowerShell 5.
    WindowsPowerShell,
    Cmd,
    /// bash, zsh, sh and WSL.
    Posix,
    /// A program with no icon of its own.
    Generic,
}

impl RuntimeIcon {
    /// The icon's part of its selector, `<id>-icon-<name>`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Agent(_) => "agent",
            Self::Pwsh => "pwsh",
            Self::WindowsPowerShell => "powershell",
            Self::Cmd => "cmd",
            Self::Posix => "posix",
            Self::Generic => "generic",
        }
    }
}

/// The icon of a shell's program `label`, matched without case and without a
/// trailing `.exe`; a program of no known family is [`RuntimeIcon::Generic`].
#[must_use]
pub(crate) fn shell_icon(label: &str) -> RuntimeIcon {
    let label = label.trim().to_ascii_lowercase();
    match label.strip_suffix(".exe").unwrap_or(&label) {
        "pwsh" => RuntimeIcon::Pwsh,
        "powershell" => RuntimeIcon::WindowsPowerShell,
        "cmd" => RuntimeIcon::Cmd,
        "bash" | "zsh" | "sh" | "wsl" => RuntimeIcon::Posix,
        _ => RuntimeIcon::Generic,
    }
}

/// An agent's icon: the one its descriptor carries, else the generic glyph.
#[must_use]
pub(crate) fn agent_icon(descriptor: Option<&AgentDescriptor>) -> RuntimeIcon {
    descriptor
        .and_then(|descriptor| descriptor.icon.clone())
        .map_or(RuntimeIcon::Generic, |svg| RuntimeIcon::Agent(svg.into()))
}

/// The icon of session `s`'s runtime tag `runtime`: its agent's while the
/// agent is up, else its shell program's; `None` without a tag.
#[must_use]
pub(crate) fn runtime_icon(
    s: &SessionSnapshot,
    runtime: Option<&str>,
    agents: &[AgentDescriptor],
) -> Option<RuntimeIcon> {
    let label = runtime?;
    Some(match crate::sidebar::agent_up(s, runtime, agents) {
        Some(descriptor) => agent_icon(Some(descriptor)),
        None => shell_icon(label),
    })
}

/// `icon` drawn `size` square, tagged `<id>-icon-<name>`: the generic glyph
/// in the theme's `subtle` colour, every other icon in its own colours.
pub(crate) fn runtime_icon_view(
    icon: &RuntimeIcon,
    size: Pixels,
    id: &str,
    t: &UiTheme,
) -> AnyElement {
    let selector = format!("{id}-icon-{}", icon.name());
    let path: SharedString = match icon {
        RuntimeIcon::Generic => {
            return frame(size, selector)
                .child(
                    svg()
                        .path(SIDEBAR_SHELL_ICON)
                        .size(size)
                        .text_color(gpui::rgb(t.subtle)),
                )
                .into_any_element();
        }
        RuntimeIcon::Agent(markup) => agent_icon_path(markup),
        RuntimeIcon::Pwsh => RUNTIME_PWSH_ICON.into(),
        RuntimeIcon::WindowsPowerShell => RUNTIME_POWERSHELL_ICON.into(),
        RuntimeIcon::Cmd => RUNTIME_CMD_ICON.into(),
        RuntimeIcon::Posix => RUNTIME_POSIX_ICON.into(),
    };
    frame(size, selector)
        .child(img(ImageSource::Resource(Resource::Embedded(path))).size(size))
        .into_any_element()
}

/// The icon's `size` square box, tagged `selector`.
fn frame(size: Pixels, selector: String) -> gpui::Div {
    div().flex_none().size(size).debug_selector(|| selector)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use crate::sidebar::runtime_label;
    use crate::sidebar::tests::{session, shell};
    use protocol::{AgentId, SessionKind};

    /// The registry's Claude descriptor, as the daemon sends it.
    fn claude_descriptor() -> AgentDescriptor {
        agent_registry::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id.0 == "claude")
            .expect("the registry has Claude")
    }

    #[test]
    fn shell_icon_groups_programs_by_family() {
        for (label, icon) in [
            ("pwsh", RuntimeIcon::Pwsh),
            ("PWSH", RuntimeIcon::Pwsh),
            ("pwsh.exe", RuntimeIcon::Pwsh),
            (" pwsh ", RuntimeIcon::Pwsh),
            ("powershell", RuntimeIcon::WindowsPowerShell),
            ("PowerShell.EXE", RuntimeIcon::WindowsPowerShell),
            ("cmd", RuntimeIcon::Cmd),
            ("cmd.exe", RuntimeIcon::Cmd),
            ("bash", RuntimeIcon::Posix),
            ("zsh", RuntimeIcon::Posix),
            ("sh", RuntimeIcon::Posix),
            ("wsl", RuntimeIcon::Posix),
            ("bash.exe", RuntimeIcon::Posix),
            ("shell", RuntimeIcon::Generic),
            ("node", RuntimeIcon::Generic),
            ("", RuntimeIcon::Generic),
            (".exe", RuntimeIcon::Generic),
        ] {
            assert_eq!(shell_icon(label), icon, "{label:?}");
        }
    }

    #[test]
    fn an_agent_up_wears_its_descriptors_icon() {
        let claude = claude_descriptor();
        let svg = claude.icon.clone().expect("Claude carries an icon");
        let agents = [claude];
        let mut agent = session("a");
        agent.agent = Some(AgentId("claude".to_owned()));
        let runtime = runtime_label(&agent, &agents);
        assert_eq!(runtime.as_deref(), Some("claude"));
        assert_eq!(
            runtime_icon(&agent, runtime.as_deref(), &agents),
            Some(RuntimeIcon::Agent(svg.into()))
        );
    }

    #[test]
    fn the_shell_around_an_agent_wears_the_shells_icon() {
        let agents = [claude_descriptor()];
        let mut agent = session("a");
        agent.agent = Some(AgentId("claude".to_owned()));
        agent.program_name = Some("pwsh".to_owned());
        let runtime = runtime_label(&agent, &agents);
        assert_eq!(runtime.as_deref(), Some("pwsh"));
        assert_eq!(
            runtime_icon(&agent, runtime.as_deref(), &agents),
            Some(RuntimeIcon::Pwsh)
        );
    }

    #[test]
    fn an_agent_without_an_icon_wears_the_generic_glyph() {
        let agents = [agent_api::test_agent::DESCRIPTOR.clone()];
        let mut agent = session("a");
        agent.agent = Some(AgentId("test-agent".to_owned()));
        let runtime = runtime_label(&agent, &agents);
        assert_eq!(runtime.as_deref(), Some("test"));
        assert_eq!(
            runtime_icon(&agent, runtime.as_deref(), &agents),
            Some(RuntimeIcon::Generic)
        );
    }

    #[test]
    fn a_shell_with_no_program_has_no_icon() {
        let programless = shell("s", None, SessionKind::Standalone);
        let runtime = runtime_label(&programless, &[]);
        assert_eq!(runtime, None);
        assert_eq!(runtime_icon(&programless, runtime.as_deref(), &[]), None);
    }
}
