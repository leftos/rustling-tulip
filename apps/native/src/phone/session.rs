//! The phone's session page as plain Rust: what its header says, whether
//! its Terminal toggle shows, which page it is on and where a back step
//! goes. `session_view` draws it.

use gpui::BackGesturePhase;

use super::home::{HomeRow, Tone};
use crate::dashboard_rows::ContextLabel;
use crate::linear_card::TitleChip;

/// The toggle's label on the Chat page.
pub(crate) const TERMINAL_LABEL: &str = "Terminal";
/// The toggle's label on the Terminal page.
pub(crate) const CHAT_LABEL: &str = "Chat";

/// What the page's header shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    /// Home's label for the session, else its id.
    pub(crate) label: String,
    /// Home's status word and tone for the session, while Home lists it.
    pub(crate) status: Option<(&'static str, Tone)>,
    /// Whether the Terminal toggle shows.
    pub(crate) toggle: bool,
    /// Whether the session's chat is mirrored now.
    pub(crate) chat_mirrored: bool,
    /// The session's context label, while its host's facts give a figure.
    pub(crate) context: Option<ContextLabel>,
    /// The chip its title's Linear identifier draws, while the title names
    /// one of the host's teams.
    pub(crate) issue: Option<TitleChip>,
}

impl Header {
    /// The header of session `session_id`, from Home's `row` for it, the
    /// session having `ended` or not and its chat mirrored or not.
    pub(crate) fn new(
        session_id: &str,
        row: Option<&HomeRow>,
        ended: bool,
        chat_mirrored: bool,
    ) -> Self {
        Self {
            label: row.map_or_else(|| session_id.to_owned(), |row| row.label.clone()),
            status: row.map(|row| (row.status, row.tone)),
            toggle: toggle_shown(ended, chat_mirrored),
            chat_mirrored,
            context: None,
            issue: None,
        }
    }

    /// The header showing `context` as its context label.
    pub(crate) fn with_context(self, context: Option<ContextLabel>) -> Self {
        Self { context, ..self }
    }

    /// The header drawing `issue` as its title's chip.
    pub(crate) fn with_issue(self, issue: Option<TitleChip>) -> Self {
        Self { issue, ..self }
    }

    /// The title text the header draws after its chip: the label, or, with
    /// a chip, the label with its identifier cut out.
    pub(crate) fn title_text(&self) -> String {
        self.issue
            .as_ref()
            .map_or_else(|| self.label.clone(), |chip| chip.rest.clone())
    }
}

/// Whether the Terminal toggle shows: for a session still running whose
/// chat is mirrored.
pub(crate) fn toggle_shown(ended: bool, chat_mirrored: bool) -> bool {
    !ended && chat_mirrored
}

/// One of the session page's two pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    /// The conversation as chat.
    Chat,
    /// The live terminal.
    Terminal,
}

impl Page {
    /// The toggle's label on this page: the page it flips to.
    pub(crate) fn toggle_label(self) -> &'static str {
        match self {
            Self::Chat => TERMINAL_LABEL,
            Self::Terminal => CHAT_LABEL,
        }
    }
}

/// Where one back step goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackStep {
    /// The Linear card sheet goes down; the page stays as it is.
    DismissCard,
    /// The prompt sheet goes down; the page stays as it is.
    DismissSheet,
    /// The composer's open dock closes, and the page stays as it is.
    CloseDock,
    /// From the Terminal page to the Chat page.
    ToChat,
    /// Off the page, back to Home.
    Close,
}

/// The page an open session shows; never saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionPage {
    page: Page,
}

impl SessionPage {
    /// A session just opened: on its chat when the chat is mirrored, else
    /// on its terminal.
    pub(crate) fn opened(chat_mirrored: bool) -> Self {
        let page = if chat_mirrored {
            Page::Chat
        } else {
            Page::Terminal
        };
        Self { page }
    }

    /// The page chosen: the one opened on, or the toggle's last flip.
    #[cfg(test)]
    pub(crate) fn page(self) -> Page {
        self.page
    }

    /// The page to draw while the chat is mirrored or not: the terminal
    /// while it is not, else the page chosen, which is kept either way.
    pub(crate) fn shown(self, chat_mirrored: bool) -> Page {
        if chat_mirrored {
            self.page
        } else {
            Page::Terminal
        }
    }

    /// Puts the session on `page`, as a turn of the window does.
    pub(crate) fn set(&mut self, page: Page) {
        self.page = page;
    }

    /// The toggle's tap: the other page.
    pub(crate) fn toggle(&mut self) {
        self.page = match self.page {
            Page::Chat => Page::Terminal,
            Page::Terminal => Page::Chat,
        };
    }

    /// One back step while the chat is mirrored or not and the toggle shows
    /// or not: from the shown Terminal page with the toggle shown, to the
    /// Chat page; otherwise off the page.
    pub(crate) fn back(&mut self, chat_mirrored: bool, toggle_shown: bool) -> BackStep {
        if self.shown(chat_mirrored) == Page::Terminal && toggle_shown {
            self.page = Page::Chat;
            BackStep::ToChat
        } else {
            BackStep::Close
        }
    }
}

/// What lies over the session page when a back step comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Over {
    /// The Linear card sheet is up.
    pub(crate) card_up: bool,
    /// The prompt sheet is up.
    pub(crate) sheet_up: bool,
    /// The composer's dock is drawn.
    pub(crate) dock_drawn: bool,
}

/// One back step with `over` lying over the page: the Linear card sheet
/// goes down first, then the prompt sheet, the page unchanged; with both
/// down, [`back_step`] (the dock, then the page's own step).
pub(crate) fn back_over_sheet(
    over: Over,
    page: &mut SessionPage,
    chat_mirrored: bool,
    toggle_shown: bool,
) -> BackStep {
    if over.card_up {
        BackStep::DismissCard
    } else if over.sheet_up {
        BackStep::DismissSheet
    } else {
        back_step(over.dock_drawn, page, chat_mirrored, toggle_shown)
    }
}

/// One back step while the composer's dock is `drawn` or not: a drawn dock
/// closes first and the page stays as it is, else the page's own step comes
/// ([`SessionPage::back`]). A dock left set on a page that does not draw it
/// takes no step of its own.
pub(crate) fn back_step(
    dock_drawn: bool,
    page: &mut SessionPage,
    chat_mirrored: bool,
    toggle_shown: bool,
) -> BackStep {
    if dock_drawn {
        BackStep::CloseDock
    } else {
        page.back(chat_mirrored, toggle_shown)
    }
}

/// Whether a back gesture in `phase` takes a back step: only once it
/// commits.
pub(crate) fn back_on(phase: BackGesturePhase) -> bool {
    phase == BackGesturePhase::Committed
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::linear_card::{LinearCache, TitleChips};
    use crate::links::IssueContext;
    use crate::target::HostKey;

    #[test]
    fn the_toggle_is_hidden_for_an_ended_session() {
        assert!(!toggle_shown(true, true));
        assert!(!toggle_shown(true, false));
        assert!(!Header::new("s", None, true, true).toggle);
    }

    #[test]
    fn the_toggle_is_hidden_without_a_mirrored_chat() {
        assert!(!toggle_shown(false, false));
        assert!(!Header::new("s", None, false, false).toggle);
    }

    #[test]
    fn the_toggle_shows_for_a_live_mirrored_session() {
        assert!(toggle_shown(false, true));
        let row = HomeRow {
            host: HostKey::Local,
            session_id: "s".to_owned(),
            label: "Alpha".to_owned(),
            status: "Working",
            tone: Tone::Working,
            container: None,
            tab_group: None,
        };
        assert_eq!(
            Header::new("s", Some(&row), false, true),
            Header {
                label: "Alpha".to_owned(),
                status: Some(("Working", Tone::Working)),
                toggle: true,
                chat_mirrored: true,
                context: None,
                issue: None,
            }
        );
    }

    /// `host`'s Linear context in workspace `acme` with the team RT and a
    /// key, Linear on or off, as `TitleChips` reads it.
    fn rt_contexts(host: &HostKey, enabled: bool) -> HashMap<HostKey, IssueContext> {
        let context =
            IssueContext::new(&["RT".to_owned()], Some("acme"), true).with_enabled(enabled);
        HashMap::from([(host.clone(), context)])
    }

    #[test]
    fn a_header_carries_its_titles_chip() {
        let host = HostKey::Local;
        let row = |label: &str| HomeRow {
            host: host.clone(),
            session_id: "s".to_owned(),
            label: label.to_owned(),
            status: "Working",
            tone: Tone::Working,
            container: None,
            tab_group: None,
        };
        let issues = LinearCache::default();
        let contexts = rt_contexts(&host, true);
        let chips = TitleChips {
            contexts: &contexts,
            issues: &issues,
            now: chrono::Utc::now(),
        };
        let titled = Header::new("s", Some(&row("RT-12 Fix the login loop")), false, true);
        let chip = chips.chip(&host, &titled.label);
        assert_eq!(
            chip.as_ref()
                .map(|chip| (chip.identifier.as_str(), chip.rest.as_str())),
            Some(("RT-12", "Fix the login loop")),
            "the title's identifier, cut out of the rest"
        );
        assert_eq!(
            titled.with_issue(chip).issue.map(|chip| chip.identifier),
            Some("RT-12".to_owned()),
            "the header carries its title's chip"
        );
        let plain = Header::new("s", Some(&row("November")), false, true)
            .with_issue(chips.chip(&host, "November"));
        assert_eq!(plain.issue, None, "a title naming no identifier takes none");
        let off = rt_contexts(&host, false);
        let chips = TitleChips {
            contexts: &off,
            issues: &issues,
            now: chrono::Utc::now(),
        };
        let off = Header::new("s", Some(&row("RT-12 Fix the login loop")), false, true)
            .with_issue(chips.chip(&host, "RT-12 Fix the login loop"));
        assert_eq!(off.issue, None, "with Linear off the title takes none");
    }

    #[test]
    fn a_mirrored_session_opens_on_its_chat() {
        let page = SessionPage::opened(true);
        assert_eq!(page.page(), Page::Chat);
        assert_eq!(page.page().toggle_label(), TERMINAL_LABEL);
    }

    #[test]
    fn an_unmirrored_session_opens_on_its_terminal() {
        assert_eq!(SessionPage::opened(false).page(), Page::Terminal);
    }

    #[test]
    fn the_toggle_flips_the_page() {
        let mut page = SessionPage::opened(true);
        page.toggle();
        assert_eq!(page.page(), Page::Terminal);
        assert_eq!(page.page().toggle_label(), CHAT_LABEL);
        page.toggle();
        assert_eq!(page.page(), Page::Chat);
    }

    #[test]
    fn back_from_the_terminal_returns_to_the_chat() {
        let mut page = SessionPage::opened(true);
        page.toggle();
        assert_eq!(page.back(true, true), BackStep::ToChat);
        assert_eq!(page.page(), Page::Chat);
    }

    #[test]
    fn back_from_the_chat_closes_the_session() {
        let mut page = SessionPage::opened(true);
        assert_eq!(page.back(true, true), BackStep::Close);
        assert_eq!(page.page(), Page::Chat);
    }

    #[test]
    fn back_from_an_unmirrored_terminal_closes_the_session() {
        let mut page = SessionPage::opened(false);
        assert_eq!(
            page.back(false, toggle_shown(false, false)),
            BackStep::Close
        );
        let mut chosen_chat = SessionPage::opened(true);
        assert_eq!(
            chosen_chat.back(false, toggle_shown(false, false)),
            BackStep::Close,
            "a chat that stopped being mirrored shows the terminal, with no toggle"
        );
        let mut ended = SessionPage::opened(true);
        ended.toggle();
        assert_eq!(
            ended.back(true, toggle_shown(true, true)),
            BackStep::Close,
            "an ended session's Terminal page has no toggle"
        );
    }

    #[test]
    fn the_shown_page_is_the_terminal_while_the_chat_is_not_mirrored() {
        let chat = SessionPage::opened(true);
        assert_eq!(chat.shown(true), Page::Chat);
        assert_eq!(chat.shown(false), Page::Terminal);
        let mut terminal = SessionPage::opened(true);
        terminal.toggle();
        assert_eq!(terminal.shown(true), Page::Terminal);
        assert_eq!(terminal.shown(false), Page::Terminal);
    }

    #[test]
    fn the_chosen_page_returns_when_the_chat_is_mirrored_again() {
        let mut page = SessionPage::opened(true);
        assert_eq!(page.shown(false), Page::Terminal);
        assert_eq!(page.shown(true), Page::Chat, "the chat shows again");
        page.toggle();
        assert_eq!(page.shown(false), Page::Terminal);
        assert_eq!(
            page.shown(true),
            Page::Terminal,
            "the chosen terminal stays"
        );
    }

    #[test]
    fn back_with_a_dock_open_closes_it_first() {
        let mut page = SessionPage::opened(true);
        assert_eq!(back_step(true, &mut page, true, true), BackStep::CloseDock);
        assert_eq!(page.page(), Page::Chat, "the page stays where it was");
        assert_eq!(
            back_step(false, &mut page, true, true),
            BackStep::Close,
            "with no dock open the page's own step comes"
        );
    }

    #[test]
    fn back_with_a_dock_set_but_not_drawn_takes_the_next_step() {
        let mut page = SessionPage::opened(true);
        page.toggle();
        assert_eq!(
            page.shown(true),
            Page::Terminal,
            "the dock draws only on the Chat page"
        );
        assert_eq!(
            back_step(false, &mut page, true, true),
            BackStep::ToChat,
            "Back steps off the Terminal page, not onto a dock nobody sees"
        );
        assert_eq!(page.page(), Page::Chat);
    }

    /// The sheet up or not and the dock drawn or not.
    fn over(sheet_up: bool, dock_drawn: bool) -> Over {
        Over {
            card_up: false,
            sheet_up,
            dock_drawn,
        }
    }

    #[test]
    fn back_with_the_sheet_up_dismisses_it_first() {
        let mut page = SessionPage::opened(true);
        page.toggle();
        assert_eq!(
            back_over_sheet(over(true, false), &mut page, true, true),
            BackStep::DismissSheet
        );
        assert_eq!(page.page(), Page::Terminal, "the page stays");
        assert_eq!(
            back_over_sheet(over(false, false), &mut page, true, true),
            BackStep::ToChat
        );
        assert_eq!(
            back_over_sheet(over(false, false), &mut page, true, true),
            BackStep::Close
        );
    }

    #[test]
    fn back_with_the_sheet_up_and_a_dock_open_dismisses_the_sheet_first() {
        let mut page = SessionPage::opened(true);
        assert_eq!(
            back_over_sheet(over(true, true), &mut page, true, true),
            BackStep::DismissSheet
        );
        assert_eq!(page.page(), Page::Chat, "the page stays");
        assert_eq!(
            back_over_sheet(over(false, true), &mut page, true, true),
            BackStep::CloseDock,
            "then the dock closes"
        );
        assert_eq!(
            back_over_sheet(over(false, false), &mut page, true, true),
            BackStep::Close
        );
    }

    #[test]
    fn only_a_committed_back_gesture_goes_back() {
        assert!(back_on(BackGesturePhase::Committed));
        assert!(!back_on(BackGesturePhase::Started));
        assert!(!back_on(BackGesturePhase::Progressed));
        assert!(!back_on(BackGesturePhase::Cancelled));
    }
}
