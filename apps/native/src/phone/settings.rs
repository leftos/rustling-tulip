//! The phone's Settings screen as plain data: open on its category list or
//! on one category's page, and the step Back takes through it, from a page
//! to the list and from the list out of Settings. `settings_view` draws it.

/// One category of the phone's Settings, a row on the list and a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    /// The per-host Notifications switches.
    Notifications,
    /// The saved hosts, the add form and the phone's SSH key.
    Hosts,
    /// The theme the phone draws in and the size of its text.
    Appearance,
}

impl Page {
    /// Every category, in the order the list draws them.
    pub(crate) const ALL: [Self; 3] = [Self::Notifications, Self::Hosts, Self::Appearance];

    /// The category's name: its row's label and its page's title.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Notifications => "Notifications",
            Self::Hosts => "Hosts",
            Self::Appearance => "Appearance",
        }
    }

    /// The tail of the category's driver ids.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Notifications => "notifications",
            Self::Hosts => "hosts",
            Self::Appearance => "appearance",
        }
    }
}

/// Where an open Settings is: its list while `page` is `None`, else that
/// category's page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The category page shown over the list, if any.
    pub(crate) page: Option<Page>,
}

/// What one Back step did to an open Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackStep {
    /// A category's page closed: the list shows.
    ToList,
    /// The list closed: Settings is gone.
    Close,
}

/// The title Settings' header draws over its list.
pub(crate) const TITLE: &str = "Settings";

impl Settings {
    /// Shows `page` over the list.
    pub(crate) fn open(&mut self, page: Page) {
        self.page = Some(page);
    }

    /// One Back step: a page goes back to the list, the list closes.
    pub(crate) fn back(&mut self) -> BackStep {
        if self.page.take().is_some() {
            BackStep::ToList
        } else {
            BackStep::Close
        }
    }

    /// The header's title: the page's category, else Settings'.
    pub(crate) fn title(self) -> &'static str {
        self.page.map_or(TITLE, Page::label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_from_a_page_goes_to_the_list() {
        let mut settings = Settings::default();
        settings.open(Page::Notifications);
        assert_eq!(settings.title(), "Notifications");

        assert_eq!(settings.back(), BackStep::ToList);
        assert_eq!(settings, Settings::default(), "the list shows");
        assert_eq!(settings.title(), TITLE);
    }

    #[test]
    fn back_from_the_list_closes() {
        let mut settings = Settings::default();

        assert_eq!(settings.back(), BackStep::Close);
        assert_eq!(settings.page, None);
    }
}
