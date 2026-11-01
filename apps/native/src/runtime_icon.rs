//! The runtime icon a session wears: its agent's own icon while the agent is
//! up, a shell family's brand icon, or the generic shell glyph.

use std::collections::HashSet;
use std::fmt;
use std::sync::{LazyLock, Mutex, PoisonError};

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

/// An agent's icon: the one its descriptor carries when [`icon_is_safe`]
/// passes it, else the generic glyph. A refused icon is logged at warn once
/// per agent.
#[must_use]
pub(crate) fn agent_icon(descriptor: Option<&AgentDescriptor>) -> RuntimeIcon {
    let Some(descriptor) = descriptor else {
        return RuntimeIcon::Generic;
    };
    let Some(svg) = descriptor.icon.as_deref() else {
        return RuntimeIcon::Generic;
    };
    match icon_is_safe(svg) {
        Ok(()) => RuntimeIcon::Agent(svg.to_owned().into()),
        Err(refusal) => {
            let agent = descriptor.id.0.as_str();
            let first = REFUSALS_LOGGED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .first_for(agent);
            if first {
                tracing::warn!(
                    agent,
                    "the agent's icon is refused, so it wears the generic glyph: {refusal}"
                );
            }
            RuntimeIcon::Generic
        }
    }
}

/// The largest agent icon the client draws, in bytes; the plugin
/// conformance check holds plugins to the same limit.
const MAX_ICON_BYTES: usize = 8192;

/// The elements an agent icon may not hold, by lowercase local name.
const FORBIDDEN_ELEMENTS: [&str; 4] = ["image", "feimage", "foreignobject", "script"];

/// Why an agent's icon markup is not drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IconRefusal {
    /// The markup is this many bytes, over [`MAX_ICON_BYTES`].
    TooLarge(usize),
    /// The markup is not one root `<svg …>…</svg>` element.
    NotOneSvg,
    /// The markup holds a forbidden element, by lowercase local name.
    Element(&'static str),
    /// An `href` points anywhere but a `#id` in the icon itself.
    ExternalHref,
    /// A `url(…)` points anywhere but a `#id` in the icon itself.
    ExternalUrl,
    /// A style sheet `@import`s another.
    Import,
}

impl fmt::Display for IconRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(bytes) => {
                write!(
                    f,
                    "it is {bytes} bytes, over the {MAX_ICON_BYTES}-byte limit"
                )
            }
            Self::NotOneSvg => f.write_str("it is not one <svg> element"),
            Self::Element(name) => write!(f, "it has a <{name}> element"),
            Self::ExternalHref => f.write_str("an href points outside the icon"),
            Self::ExternalUrl => f.write_str("a url() points outside the icon"),
            Self::Import => f.write_str("it has an @import"),
        }
    }
}

/// Whether agent icon markup `svg` is safe to draw: at most
/// [`MAX_ICON_BYTES`], one root `<svg …>…</svg>`, no `<image>`, `<feImage>`,
/// `<foreignObject>` or `<script>`, every `href` and `url(…)` a same-document
/// `#id`, and no `@import`. Names are matched without ASCII case.
pub(crate) fn icon_is_safe(svg: &str) -> Result<(), IconRefusal> {
    if svg.len() > MAX_ICON_BYTES {
        return Err(IconRefusal::TooLarge(svg.len()));
    }
    let lower = svg.trim().to_ascii_lowercase();
    if !is_one_svg(&lower) {
        return Err(IconRefusal::NotOneSvg);
    }
    if let Some(name) = forbidden_element(&lower) {
        return Err(IconRefusal::Element(name));
    }
    if !hrefs_stay_inside(&lower) {
        return Err(IconRefusal::ExternalHref);
    }
    if !urls_stay_inside(&lower) {
        return Err(IconRefusal::ExternalUrl);
    }
    if lower.contains("@import") {
        return Err(IconRefusal::Import);
    }
    Ok(())
}

/// Every tag in lowercase markup `lower`: whether it closes, and its local
/// name (the part after any `prefix:`). Comments, declarations and
/// processing instructions are skipped.
fn tags(lower: &str) -> impl Iterator<Item = (bool, &str)> {
    lower.match_indices('<').filter_map(|(at, _)| {
        let rest = &lower[at + 1..];
        let (closing, rest) = match rest.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        if rest.starts_with(['!', '?']) {
            return None;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(rest.len());
        let name = &rest[..end];
        Some((closing, name.rsplit(':').next().unwrap_or(name)))
    })
}

/// Whether trimmed lowercase markup `lower` is one `<svg …>…</svg>` with no
/// other `svg` element, before or after it or inside it, and nothing (no
/// prolog, no doctype) before it.
fn is_one_svg(lower: &str) -> bool {
    let mut svgs = tags(lower).filter(|(_, name)| *name == "svg");
    lower.starts_with("<svg")
        && lower.ends_with("</svg>")
        && tags(lower).next() == Some((false, "svg"))
        && svgs.next() == Some((false, "svg"))
        && svgs.next() == Some((true, "svg"))
        && svgs.next().is_none()
}

/// The first forbidden element in lowercase markup `lower`.
fn forbidden_element(lower: &str) -> Option<&'static str> {
    tags(lower).find_map(|(_, name)| FORBIDDEN_ELEMENTS.into_iter().find(|&bad| bad == name))
}

/// Whether every `href` attribute (`xlink:href` and other prefixes too) in
/// lowercase markup `lower` holds a quoted same-document `#id` and nothing
/// else.
fn hrefs_stay_inside(lower: &str) -> bool {
    lower.match_indices("href").all(|(at, _)| {
        let Some(value) = lower[at + "href".len()..].trim_start().strip_prefix('=') else {
            return true;
        };
        quoted(value.trim_start()).is_some_and(is_id_reference)
    })
}

/// Whether `text` is exactly `#` and one or more id characters: ASCII
/// letters and digits, `_`, `-`, `.` and `:`. A path, a character reference
/// or a percent escape after the `#` is not an id.
fn is_id_reference(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(|id| {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    })
}

/// The value of attribute text `text` that opens with a `"` or `'` quote, up
/// to the matching quote; `None` unquoted or unclosed.
fn quoted(text: &str) -> Option<&str> {
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let body = &text[1..];
    body.find(quote).map(|end| &body[..end])
}

/// Whether every `url(…)` in lowercase markup `lower` names a same-document
/// `#id` and nothing else, quoted or not, with whitespace allowed only
/// inside the parentheses around it.
fn urls_stay_inside(lower: &str) -> bool {
    lower.match_indices("url(").all(|(at, _)| {
        let rest = &lower[at + "url(".len()..];
        rest.find(')').is_some_and(|end| {
            let inner = rest[..end].trim();
            let unquoted = match inner.chars().next() {
                Some(quote @ ('"' | '\'')) => inner[1..].strip_suffix(quote),
                _ => Some(inner),
            };
            unquoted.is_some_and(is_id_reference)
        })
    })
}

/// The agents whose refused icon has been logged.
static REFUSALS_LOGGED: LazyLock<Mutex<RefusalLog>> =
    LazyLock::new(|| Mutex::new(RefusalLog::default()));

/// Which agents' icon refusals have been logged, so each agent warns once
/// however often its icon is drawn.
#[derive(Debug, Default)]
struct RefusalLog {
    agents: HashSet<String>,
}

impl RefusalLog {
    /// Whether `agent`'s refusal is logged for the first time, noting it.
    fn first_for(&mut self, agent: &str) -> bool {
        if self.agents.contains(agent) {
            return false;
        }
        self.agents.insert(agent.to_owned())
    }
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

    /// The icon a test agent whose descriptor carries `svg` wears.
    fn wearing(svg: &str) -> RuntimeIcon {
        let mut descriptor = agent_api::test_agent::DESCRIPTOR.clone();
        descriptor.icon = Some(svg.to_owned());
        agent_icon(Some(&descriptor))
    }

    #[test]
    fn an_oversized_icon_wears_the_generic_glyph() {
        let wrap = |body: &str| format!("<svg>{body}</svg>");
        let at_limit = wrap(&" ".repeat(MAX_ICON_BYTES - wrap("").len()));
        assert_eq!(at_limit.len(), MAX_ICON_BYTES);
        assert_eq!(icon_is_safe(&at_limit), Ok(()));
        let over = wrap(&" ".repeat(MAX_ICON_BYTES + 1 - wrap("").len()));
        assert_eq!(
            icon_is_safe(&over),
            Err(IconRefusal::TooLarge(MAX_ICON_BYTES + 1))
        );
        assert_eq!(wearing(&over), RuntimeIcon::Generic);
    }

    #[test]
    fn an_icon_with_an_image_element_is_refused() {
        for svg in [
            r##"<svg><image href="#a"/></svg>"##,
            "<svg><IMAGE/></svg>",
            "<svg><svg:image/></svg>",
            "<svg>\n  <image\n/></svg>",
        ] {
            assert_eq!(
                icon_is_safe(svg),
                Err(IconRefusal::Element("image")),
                "{svg}"
            );
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg}");
        }
    }

    #[test]
    fn an_icon_with_an_external_href_is_refused() {
        for svg in [
            r#"<svg><use href="https://example.com/a.svg#a"/></svg>"#,
            "<svg><use xlink:href = 'file:///c:/secret'/></svg>",
            r#"<svg><a HREF="javascript:alert(1)"/></svg>"#,
            "<svg><use href=#a/></svg>",
            r##"<svg><use href="#a/></svg>"##,
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::ExternalHref), "{svg}");
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg}");
        }
    }

    #[test]
    fn an_icon_with_an_external_url_or_import_is_refused() {
        for svg in [
            r#"<svg><rect fill="url(https://example.com/p)"/></svg>"#,
            r#"<svg><rect style="fill: URL( 'http://example.com/p' )"/></svg>"#,
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::ExternalUrl), "{svg}");
        }
        let import = "<svg><style>@IMPORT 'a.css';</style></svg>";
        assert_eq!(icon_is_safe(import), Err(IconRefusal::Import));
        assert_eq!(wearing(import), RuntimeIcon::Generic);
    }

    #[test]
    fn a_same_document_href_and_url_are_allowed() {
        let svg = concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">"#,
            r##"<defs><linearGradient id="g"/></defs>"##,
            r##"<use href="#g"/><use xlink:href = '#g'/>"##,
            r##"<rect fill="url(#g)" style="fill: url( '#g' )"/></svg>"##,
        );
        assert_eq!(icon_is_safe(svg), Ok(()));
        assert_eq!(wearing(svg), RuntimeIcon::Agent(svg.into()));
    }

    #[test]
    fn a_script_or_foreign_object_is_refused() {
        for (svg, name) in [
            ("<svg><script>alert(1)</script></svg>", "script"),
            ("<svg><SCRIPT/></svg>", "script"),
            ("<svg><![CDATA[ <script> ]]></svg>", "script"),
            (
                "<svg><foreignObject><p/></foreignObject></svg>",
                "foreignobject",
            ),
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::Element(name)), "{svg}");
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg}");
        }
    }

    #[test]
    fn two_root_svgs_are_refused() {
        for svg in [
            "<svg></svg><svg></svg>",
            "<svg><svg></svg></svg>",
            "<g/><svg></svg>",
            r#"<?xml version="1.0"?><svg></svg>"#,
            r#"<!DOCTYPE svg [<!ENTITY x SYSTEM "file:///c:/secret">]><svg>&x;</svg>"#,
            "<svgx></svgx>",
            "<svg>",
            "",
            "   ",
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::NotOneSvg), "{svg:?}");
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg:?}");
        }
    }

    #[test]
    fn an_feimage_is_refused() {
        for svg in [
            concat!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">"#,
                r##"<filter id="f"><feImage href="#/../../../../../Users/victim/Pictures/a.png"/></filter>"##,
                r#"<rect width="16" height="16" filter="url(#f)"/></svg>"#,
            ),
            "<svg><FEIMAGE/></svg>",
            "<svg><svg:feImage/></svg>",
        ] {
            assert_eq!(
                icon_is_safe(svg),
                Err(IconRefusal::Element("feimage")),
                "{svg}"
            );
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg}");
        }
    }

    #[test]
    fn an_href_with_a_path_after_the_hash_is_refused() {
        for svg in [
            r##"<svg><use href="#/../../a.png"/></svg>"##,
            r##"<svg><use href="#a\b"/></svg>"##,
            r##"<svg><use href="#a%2fb"/></svg>"##,
            r##"<svg><use href="#a b"/></svg>"##,
            r#"<svg><use href=" #a"/></svg>"#,
            r##"<svg><use href="#"/></svg>"##,
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::ExternalHref), "{svg}");
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg}");
        }
        for svg in [
            r#"<svg><rect fill="url(#/../a.png)"/></svg>"#,
            r#"<svg><rect fill="url('#a/b')"/></svg>"#,
            r#"<svg><rect fill="url(#)"/></svg>"#,
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::ExternalUrl), "{svg}");
        }
    }

    #[test]
    fn an_href_with_a_character_reference_is_refused() {
        for svg in [
            r##"<svg><use href="#&#47;..&#47;a.png"/></svg>"##,
            r#"<svg><use href="&#35;a"/></svg>"#,
        ] {
            assert_eq!(icon_is_safe(svg), Err(IconRefusal::ExternalHref), "{svg}");
            assert_eq!(wearing(svg), RuntimeIcon::Generic, "{svg}");
        }
        let url = r#"<svg><rect fill="url(#&#47;a)"/></svg>"#;
        assert_eq!(icon_is_safe(url), Err(IconRefusal::ExternalUrl));
    }

    #[test]
    fn the_claude_icon_passes() {
        let svg = claude_descriptor().icon.expect("Claude carries an icon");
        assert_eq!(icon_is_safe(&svg), Ok(()));
        assert_eq!(wearing(&svg), RuntimeIcon::Agent(svg.into()));
    }

    #[test]
    fn a_refused_icon_is_logged_once_per_agent() {
        let mut log = RefusalLog::default();
        assert!(log.first_for("a"), "the first refusal of a is logged");
        assert!(!log.first_for("a"), "a's second refusal is not");
        assert!(log.first_for("b"), "b's first refusal is logged");
        assert!(!log.first_for("a"), "nor a's third");
    }

    #[test]
    fn a_shell_with_no_program_has_no_icon() {
        let programless = shell("s", None, SessionKind::Standalone);
        let runtime = runtime_label(&programless, &[]);
        assert_eq!(runtime, None);
        assert_eq!(runtime_icon(&programless, runtime.as_deref(), &[]), None);
    }
}
