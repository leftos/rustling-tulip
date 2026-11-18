//! The phone's Settings screen as plain data: open on its category list or
//! on one category's page, with the sub-pages opened over that page, and the
//! step Back takes through them, from a sub-page to its page, from a page to
//! the list and from the list out of Settings. `settings_view` draws it.

use super::quick_replies::ReplySet;

/// One category of the phone's Settings, a row on the list and a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    /// The per-host Notifications switches.
    Notifications,
    /// The saved hosts, the add form and the phone's SSH key.
    Hosts,
    /// The theme the phone draws in and the size of its text.
    Appearance,
    /// The two sets of one-tap replies, a group at a time.
    QuickReplies,
    /// The key grid the bar and the dock draw.
    Keys,
    /// The copies kept of fetched files: their size and Clear.
    Storage,
}

impl Page {
    /// Every category, in the order the list draws them.
    pub(crate) const ALL: [Self; 6] = [
        Self::Notifications,
        Self::Hosts,
        Self::Appearance,
        Self::QuickReplies,
        Self::Keys,
        Self::Storage,
    ];

    /// The category's name: its row's label and its page's title.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Notifications => "Notifications",
            Self::Hosts => "Hosts",
            Self::Appearance => "Appearance",
            Self::QuickReplies => "Quick replies",
            Self::Keys => "Keys",
            Self::Storage => "Storage",
        }
    }

    /// The tail of the category's driver ids.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Notifications => "notifications",
            Self::Hosts => "hosts",
            Self::Appearance => "appearance",
            Self::QuickReplies => "quick-replies",
            Self::Keys => "keys",
            Self::Storage => "storage",
        }
    }
}

/// A page opened over a category's page: one of its items, named by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubPage {
    /// One set's group and its replies; `group` is the group's index in the
    /// set.
    ReplyGroup { set: ReplySet, group: usize },
    /// A group's title in its editor; `None` is a group being added.
    GroupTitle { set: ReplySet, group: Option<usize> },
    /// One reply in its editor; `None` is a reply being added.
    Reply {
        set: ReplySet,
        group: usize,
        reply: Option<usize>,
    },
    /// One key in its editor; `None` is a key being added.
    KeyEditor { key: Option<usize> },
    /// The picker choosing the key of press `step` of the open key editor.
    KeyPicker { step: usize },
}

impl SubPage {
    /// The sub-page's name: its header's title.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::ReplyGroup { .. } => "Group",
            Self::GroupTitle { .. } => "Group title",
            Self::Reply { .. } => "Reply",
            Self::KeyEditor { .. } => "Key",
            Self::KeyPicker { .. } => "Choose a key",
        }
    }
}

/// Where an open Settings is: its list while `page` is `None`, else that
/// category's page, with its sub-pages over it innermost last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The category page shown over the list, if any.
    pub(crate) page: Option<Page>,
    /// The sub-pages open over the page, innermost last.
    pub(crate) stack: Vec<SubPage>,
}

/// What one Back step did to an open Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackStep {
    /// The innermost sub-page closed: its category's page shows.
    Pop,
    /// A category's page closed: the list shows.
    ToList,
    /// The list closed: Settings is gone.
    Close,
}

/// The title Settings' header draws over its list.
pub(crate) const TITLE: &str = "Settings";

impl Settings {
    /// Shows `page` over the list, with no sub-page over it.
    pub(crate) fn open(&mut self, page: Page) {
        self.page = Some(page);
        self.stack.clear();
    }

    /// Opens `sub` over the page; nothing while no page shows.
    pub(crate) fn push(&mut self, sub: SubPage) {
        if self.page.is_some() {
            self.stack.push(sub);
        }
    }

    /// The innermost sub-page, if any.
    pub(crate) fn top(&self) -> Option<SubPage> {
        self.stack.last().copied()
    }

    /// One Back step: the innermost sub-page pops, then a page goes back to
    /// the list, then the list closes.
    pub(crate) fn back(&mut self) -> BackStep {
        if self.stack.pop().is_some() {
            return BackStep::Pop;
        }
        if self.page.take().is_some() {
            return BackStep::ToList;
        }
        BackStep::Close
    }

    /// The header's title: the innermost sub-page's, else the page's
    /// category, else Settings'.
    pub(crate) fn title(&self) -> &'static str {
        if let Some(sub) = self.top() {
            return sub.title();
        }
        self.page.map_or(TITLE, Page::label)
    }

    /// Drops the innermost sub-pages `valid` refuses, and the ones under them
    /// with them: a page whose item is gone closes.
    pub(crate) fn retain_valid(&mut self, valid: impl Fn(SubPage) -> bool) {
        while let Some(top) = self.top() {
            if valid(top) {
                return;
            }
            self.stack.pop();
        }
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

    #[test]
    fn the_categories_are_listed_in_order() {
        let keys: Vec<&str> = Page::ALL.iter().map(|page| page.key()).collect();

        assert_eq!(
            keys,
            [
                "notifications",
                "hosts",
                "appearance",
                "quick-replies",
                "keys",
                "storage"
            ]
        );
    }

    #[test]
    fn a_sub_page_needs_a_page() {
        let mut settings = Settings::default();
        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Agent,
            group: 0,
        });

        assert_eq!(settings.stack, Vec::new());
        assert_eq!(settings.top(), None);
    }

    #[test]
    fn back_from_a_sub_page_pops_it() {
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Agent,
            group: 0,
        });

        assert_eq!(settings.back(), BackStep::Pop);
        assert_eq!(settings.page, Some(Page::QuickReplies), "the page stays");
        assert_eq!(settings.stack, Vec::new());
    }

    #[test]
    fn back_steps_out_one_sub_page_at_a_time() {
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        for group in [0, 1] {
            settings.push(SubPage::ReplyGroup {
                set: ReplySet::Agent,
                group,
            });
        }

        assert_eq!(settings.back(), BackStep::Pop);
        assert_eq!(settings.back(), BackStep::Pop);
        assert_eq!(settings.back(), BackStep::ToList);
        assert_eq!(settings.page, None);
    }

    #[test]
    fn opening_a_page_clears_its_sub_pages() {
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Agent,
            group: 0,
        });

        settings.open(Page::Hosts);

        assert_eq!(settings.page, Some(Page::Hosts));
        assert_eq!(settings.stack, Vec::new());
    }

    #[test]
    fn the_title_is_the_innermost_pages() {
        let mut settings = Settings::default();
        assert_eq!(settings.title(), TITLE);

        settings.open(Page::QuickReplies);
        assert_eq!(settings.title(), "Quick replies");

        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Shell,
            group: 0,
        });
        assert_eq!(settings.title(), "Group");

        settings.push(SubPage::GroupTitle {
            set: ReplySet::Shell,
            group: None,
        });
        assert_eq!(settings.title(), "Group title");

        settings.push(SubPage::Reply {
            set: ReplySet::Shell,
            group: 0,
            reply: Some(1),
        });
        assert_eq!(settings.title(), "Reply");

        settings.push(SubPage::KeyEditor { key: Some(0) });
        assert_eq!(settings.title(), "Key");

        settings.push(SubPage::KeyPicker { step: 0 });
        assert_eq!(settings.title(), "Choose a key");
    }

    #[test]
    fn a_sub_page_whose_item_is_gone_closes_with_those_over_it() {
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Agent,
            group: 1,
        });
        settings.push(SubPage::Reply {
            set: ReplySet::Agent,
            group: 1,
            reply: Some(0),
        });

        settings.retain_valid(|sub| match sub {
            SubPage::GroupTitle { group, .. } => group.is_none_or(|group| group == 0),
            SubPage::ReplyGroup { group, .. } | SubPage::Reply { group, .. } => group == 0,
            SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. } => true,
        });

        assert_eq!(
            settings.stack,
            Vec::new(),
            "the reply over the removed group closes with it"
        );
    }

    #[test]
    fn only_the_stale_sub_pages_close() {
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Agent,
            group: 0,
        });
        settings.push(SubPage::ReplyGroup {
            set: ReplySet::Agent,
            group: 3,
        });

        settings.retain_valid(|sub| match sub {
            SubPage::GroupTitle { group, .. } => group.is_none_or(|group| group == 0),
            SubPage::ReplyGroup { group, .. } | SubPage::Reply { group, .. } => group == 0,
            SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. } => true,
        });

        assert_eq!(
            settings.stack,
            [SubPage::ReplyGroup {
                set: ReplySet::Agent,
                group: 0
            }]
        );
    }
}
