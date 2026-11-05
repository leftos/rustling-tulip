//! Every Settings row the search field can find, and the matcher over them.
//!
//! A setting is searchable by being declared here: [`fixed_entries`] lists the
//! fixed tabs' rows in drawing order, [`agent_entries`] builds the agent
//! pages' rows from the daemon's descriptors, and [`results`] keeps the tabs
//! that hold a match, each with its count and its first match.

use std::borrow::Cow;

use protocol::AgentDescriptor;

use crate::agent_options::{AgentOptions, FieldView};
use crate::settings_view::{SettingsControl, SettingsTab, field_controls, intern};

/// One row of a Settings tab: a setting a query can find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The tab the row is drawn on.
    pub tab: SettingsTab,
    /// The title the tab list draws for the row's tab.
    pub tab_title: Cow<'static, str>,
    /// The row's control selector ([`SettingsControl::selector`]), or the
    /// driver id of a row with no control of its own.
    ///
    /// [`SettingsControl::selector`]: crate::settings_view::SettingsControl::selector
    pub selector: Cow<'static, str>,
    /// The row's label, as drawn.
    pub label: Cow<'static, str>,
    /// The section heading the row is drawn under, as drawn; empty for a row
    /// under none.
    pub within: Cow<'static, str>,
    /// Other words the row answers to.
    pub aliases: &'static [&'static str],
    /// When the row is drawn.
    pub gate: Gate,
}

/// When a row is drawn: a closed gate hides the row from a query, as the tab
/// does not show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Drawn wherever its tab is.
    Always,
    /// Drawn only while the Remote access tab exists, which a remote target
    /// leaves out.
    RemoteTab,
    /// This PC only: the Diagnostics rows.
    ThisPcOnly,
    /// Drawn on Windows builds only.
    Windows,
}

/// Which of the gated rows are drawn now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Visibility {
    /// Whether the Remote access tab exists.
    pub remote_tab: bool,
    /// Whether the This PC-only rows are drawn: the shown host is this PC.
    pub this_pc: bool,
}

impl Visibility {
    /// Whether a row behind `gate` is drawn now.
    fn shows(self, gate: Gate) -> bool {
        match gate {
            Gate::Always => true,
            Gate::RemoteTab => self.remote_tab,
            Gate::ThisPcOnly => self.this_pc,
            Gate::Windows => cfg!(windows),
        }
    }
}

/// The tabs a query finds, one per tab that holds a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TabHits {
    /// The tab that holds the match.
    pub tab: SettingsTab,
    /// How many of its rows match.
    pub count: usize,
    /// The index in the catalogue of its first matching row, in drawing
    /// order.
    pub first: usize,
}

/// Every row a search covers, the fixed tabs' in drawing order and then each
/// agent page's, in the order `agents` lists them.
#[must_use]
pub(crate) fn entries(agents: &[AgentDescriptor]) -> Vec<Entry> {
    let mut entries = fixed_entries();
    entries.extend(agent_entries(agents));
    entries
}

/// Whether `entry` matches every word of `query_words`, each already
/// lowercased: each one a case-insensitive substring of its label, its
/// section heading, its tab's title or one of its aliases. An empty word list
/// matches every entry; [`results`] never asks one.
#[must_use]
pub(crate) fn matches(query_words: &[String], entry: &Entry, tab_title: &str) -> bool {
    query_words.iter().all(|word| {
        holds(&entry.label, word)
            || holds(&entry.within, word)
            || holds(tab_title, word)
            || entry.aliases.iter().any(|alias| holds(alias, word))
    })
}

/// The tabs among `entries` that hold a match for `query`, in the tab list's
/// order, each with its match count and its first match; a query of nothing
/// but whitespace matches nothing and filters nothing.
#[must_use]
pub(crate) fn results(query: &str, entries: &[Entry], visible: Visibility) -> Vec<TabHits> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<TabHits> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if !visible.shows(entry.gate) || !matches(&words, entry, &entry.tab_title) {
            continue;
        }
        match hits.iter_mut().find(|hit| hit.tab == entry.tab) {
            Some(hit) => hit.count += 1,
            None => hits.push(TabHits {
                tab: entry.tab,
                count: 1,
                first: index,
            }),
        }
    }
    hits.sort_by_key(|hit| tab_place(hit.tab));
    hits
}

/// What one tab draws now: the selectors of its stops in the order Tab
/// walks them, and the driver ids of its rows that have no control of their
/// own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrawnTab {
    /// The tab.
    pub tab: SettingsTab,
    /// The selectors of the controls its page draws now.
    pub stops: Vec<&'static str>,
    /// The ids of the rows it draws now that no control of their own names.
    pub rows: Vec<String>,
}

/// The entries among `entries` whose row is drawn now: its tab is among
/// `tabs` and names the entry's selector as a stop or a row.
#[must_use]
pub(crate) fn drawn(entries: Vec<Entry>, tabs: &[DrawnTab]) -> Vec<Entry> {
    entries
        .into_iter()
        .filter(|entry| {
            tabs.iter().any(|drawn| {
                drawn.tab == entry.tab
                    && (drawn.stops.contains(&entry.selector.as_ref())
                        || drawn.rows.iter().any(|row| *row == entry.selector))
            })
        })
        .collect()
}

/// Ring stops on the tabs whose stop orders need live state, so the unit
/// guards cannot see them, that are no settings row of their own, with why:
/// the UI spec that walks every tab's ring applies them beside [`EXEMPT`].
pub(crate) const RING_EXEMPT: &[(&str, &str)] = &[
    (
        "settings-needs-you-sound-play",
        "the Needs you sound row's Play button",
    ),
    (
        "settings-finished-sound-play",
        "the Finished sound row's Play button",
    ),
    (
        "settings-finished-grace-2",
        "another choice of the Hold a Finished toast for row",
    ),
    (
        "settings-finished-grace-5",
        "another choice of the Hold a Finished toast for row",
    ),
    (
        "settings-finished-grace-10",
        "another choice of the Hold a Finished toast for row",
    ),
    (
        "settings-finished-grace-20",
        "another choice of the Hold a Finished toast for row",
    ),
    (
        "settings-notify-skip-focused",
        "another choice of the Skip notifications when row",
    ),
    (
        "settings-notify-skip-never",
        "another choice of the Skip notifications when row",
    ),
    (
        "settings-toast-style-app",
        "another choice of the Show toasts as row",
    ),
    (
        "settings-dashboard-key-source-env",
        "another choice of the Key source row",
    ),
    ("settings-dashboard-key-clear", "the Key row's Clear button"),
    (
        "settings-worktrees-root-browse",
        "the Path row's Browse button",
    ),
    (
        "settings-count-busy",
        "another choice of the Tab and badge count row",
    ),
    (
        "settings-count-idle",
        "another choice of the Tab and badge count row",
    ),
];

/// Whether a search covers the control `selector`: a catalogue entry
/// names it, an exemption says why none needs to, it is a provider of the
/// Dashboard's Provider row, a control of the Accounts page's table or
/// another choice of one of its rows, or another choice of an agent page's
/// choice row whose first choice an entry names.
#[must_use]
pub(crate) fn names(selector: &str, agents: &[AgentDescriptor]) -> bool {
    let catalogue = entries(agents);
    let catalogued = |selector: &str| catalogue.iter().any(|entry| entry.selector == selector);
    if catalogued(selector)
        || EXEMPT
            .iter()
            .chain(RING_EXEMPT)
            .any(|(exempt, _)| *exempt == selector)
    {
        return true;
    }
    if selector.starts_with("settings-dashboard-provider-") {
        return catalogued("settings-dashboard-provider");
    }
    if let Some(covered) = accounts_covered(selector, catalogued) {
        return covered;
    }
    agents.iter().any(|descriptor| {
        let options = AgentOptions::new(descriptor.clone(), None);
        options
            .settings_fields(false)
            .iter()
            .enumerate()
            .any(|(index, view)| {
                let controls = field_controls(&descriptor.tag, index, view);
                controls
                    .iter()
                    .any(|control| control.selector() == selector)
                    && controls
                        .first()
                        .is_some_and(|first| catalogued(first.selector()))
            })
    })
}

/// Whether `text` holds `word`, ignoring case: `word` is already lowercased
/// once by the caller.
fn holds(text: &str, word: &str) -> bool {
    text.to_lowercase().contains(word)
}

/// Where `tab` sits in the tab list: the fixed tabs in their order, then the
/// agent pages, which keep the order the catalogue lists them in.
fn tab_place(tab: SettingsTab) -> usize {
    SettingsTab::FIXED
        .iter()
        .position(|fixed| *fixed == tab)
        .unwrap_or(SettingsTab::FIXED.len())
}

/// A row drawn on `tab`, under `within`, a setting with no aliases and no
/// gate; a row that has either builds on this with a struct update.
fn entry(
    tab: SettingsTab,
    selector: &'static str,
    label: &'static str,
    within: &'static str,
    gate: Gate,
) -> Entry {
    Entry {
        tab,
        tab_title: Cow::Borrowed(tab.label(&[])),
        selector: Cow::Borrowed(selector),
        label: Cow::Borrowed(label),
        within: Cow::Borrowed(within),
        aliases: &[],
        gate,
    }
}

/// The fixed tabs' rows, in tab order and drawing order within a tab.
fn fixed_entries() -> Vec<Entry> {
    let mut entries = general_entries();
    entries.extend(notifications_entries());
    entries.extend(dashboard_entries());
    entries.extend(spawn_defaults_entries());
    entries.extend(worktrees_entries());
    entries.extend(remote_access_entries());
    entries.extend(accounts_entries());
    entries.extend(appearance_entries());
    entries.extend(app_title_entries());
    entries
}

/// The General tab's rows: Power, Sidebar, Terminal behavior, Chat, Updates
/// and Diagnostics, in the order the page draws them.
#[expect(
    clippy::too_many_lines,
    reason = "the tab's rows are one table, read as a whole"
)]
fn general_entries() -> Vec<Entry> {
    vec![
        Entry {
            aliases: &["sleep", "power"],
            ..entry(
                SettingsTab::General,
                "settings-keep-awake-off",
                "Keep this PC awake",
                "Power",
                Gate::Always,
            )
        },
        entry(
            SettingsTab::General,
            "settings-sidebar-view-repos",
            "Default view",
            "Sidebar",
            Gate::Always,
        ),
        entry(
            SettingsTab::General,
            "settings-leaf-density-comfortable",
            "Leaf density",
            "Sidebar",
            Gate::Always,
        ),
        entry(
            SettingsTab::General,
            "settings-sessions-under-repos-every",
            "Sessions under repos",
            "Sidebar",
            Gate::Always,
        ),
        entry(
            SettingsTab::General,
            "settings-repo-sort-manual",
            "Sort repos",
            "Sidebar",
            Gate::Always,
        ),
        entry(
            SettingsTab::General,
            "settings-terminal-copy-on-selection",
            "Copy selection to clipboard automatically",
            "Terminal behavior",
            Gate::Always,
        ),
        Entry {
            aliases: &["shell"],
            ..entry(
                SettingsTab::General,
                "settings-default-shell-field",
                "Default shell",
                "Terminal behavior",
                Gate::Always,
            )
        },
        Entry {
            aliases: &["shell", "command"],
            ..entry(
                SettingsTab::General,
                "settings-default-shell-command",
                "Custom shell command",
                "Terminal behavior",
                Gate::Always,
            )
        },
        Entry {
            aliases: &["tool calls", "compact", "comfortable"],
            ..entry(
                SettingsTab::General,
                "settings-chat-mode-compact",
                "Chat view",
                "Chat",
                Gate::Always,
            )
        },
        Entry {
            aliases: &["width", "column", "px"],
            ..entry(
                SettingsTab::General,
                "settings-chat-width-input",
                "Maximum chat width",
                "Chat",
                Gate::Always,
            )
        },
        Entry {
            aliases: &["update", "version"],
            ..entry(
                SettingsTab::General,
                "settings-general-check-updates",
                "Check for updates",
                "Updates",
                Gate::Always,
            )
        },
        entry(
            SettingsTab::General,
            "settings-general-check-now",
            "Check now",
            "Updates",
            Gate::Always,
        ),
        Entry {
            aliases: &["log", "logs", "debug", "trace"],
            ..entry(
                SettingsTab::General,
                "settings-detailed-logging",
                "Detailed logging",
                "Diagnostics",
                Gate::ThisPcOnly,
            )
        },
        Entry {
            aliases: &["log", "logs", "native.log", "daemon.log"],
            ..entry(
                SettingsTab::General,
                "settings-open-logs-folder",
                "Open logs folder",
                "Diagnostics",
                Gate::ThisPcOnly,
            )
        },
    ]
}

/// The Notifications tab's rows, in the order the page draws them.
fn notifications_entries() -> Vec<Entry> {
    vec![
        entry(
            SettingsTab::Notifications,
            "settings-open-windows-notifications",
            "Open Windows notification settings",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-notify-needs-you",
            "Needs you",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-needs-you-sound-field",
            "Needs you sound",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-notify-finished",
            "Finished",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-notify-account-switches",
            "Account switches",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-finished-sound-field",
            "Finished sound",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-finished-grace-0",
            "Hold a Finished toast for",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-notify-skip-on-screen",
            "Skip notifications when",
            "Notifications",
            Gate::Always,
        ),
        entry(
            SettingsTab::Notifications,
            "settings-toast-style-os",
            "Show toasts as",
            "Notifications",
            Gate::Windows,
        ),
    ]
}

/// The Dashboard tab's rows: the summarizer's settings, the daily cap and the
/// test button.
fn dashboard_entries() -> Vec<Entry> {
    vec![
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-enabled",
            "Summarize sessions",
            "Dashboard summaries",
            Gate::Always,
        ),
        // Each provider's control is named after the daemon's provider list,
        // so the row is catalogued by the control every provider's is built
        // from rather than by a selector that only one daemon names.
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-provider",
            "Provider",
            "Dashboard summaries",
            Gate::Always,
        ),
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-key-source-stored",
            "Key source",
            "Dashboard summaries",
            Gate::Always,
        ),
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-key-field",
            "Key",
            "Dashboard summaries",
            Gate::Always,
        ),
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-env-variable",
            "Variable",
            "Dashboard summaries",
            Gate::Always,
        ),
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-cap",
            "Cap (USD)",
            "Daily cap",
            Gate::Always,
        ),
        entry(
            SettingsTab::Dashboard,
            "settings-dashboard-test",
            "Test",
            "Test",
            Gate::Always,
        ),
    ]
}

/// The Spawn defaults tab's row.
fn spawn_defaults_entries() -> Vec<Entry> {
    vec![entry(
        SettingsTab::SpawnDefaults,
        "settings-spawn-trusted",
        "Trusted launch by default",
        "Spawn defaults",
        Gate::Always,
    )]
}

/// The Worktrees tab's rows: the path and the buttons that act on it.
fn worktrees_entries() -> Vec<Entry> {
    vec![
        entry(
            SettingsTab::Worktrees,
            "settings-worktrees-root-input",
            "Path",
            "Worktrees root",
            Gate::Always,
        ),
        entry(
            SettingsTab::Worktrees,
            "settings-worktrees-root-reset",
            "Reset to default",
            "Worktrees root",
            Gate::Always,
        ),
        entry(
            SettingsTab::Worktrees,
            "settings-worktrees-open-manager",
            "Manage worktrees…",
            "Worktrees root",
            Gate::Always,
        ),
    ]
}

/// The Remote access page's rows, which exist only while the tab does.
fn remote_access_entries() -> Vec<Entry> {
    vec![
        Entry {
            aliases: &["tailnet", "remote"],
            ..entry(
                SettingsTab::RemoteAccess,
                "settings-remote-toggle",
                "Allow remote access over Tailscale",
                "Remote access (Tailscale)",
                Gate::RemoteTab,
            )
        },
        entry(
            SettingsTab::RemoteAccess,
            "settings-remote-port-input",
            "Port",
            "Remote access (Tailscale)",
            Gate::RemoteTab,
        ),
        entry(
            SettingsTab::RemoteAccess,
            "settings-remote-autostart-toggle",
            "Start the daemon when I log in",
            "Remote access (Tailscale)",
            Gate::RemoteTab,
        ),
    ]
}

/// The Accounts page's rows: Add current login under the account table,
/// then the auto-switch block's, which the page draws only while the host
/// reports an auto-switch state. The account table's rows are the saved
/// accounts, not settings, and no entry names them.
fn accounts_entries() -> Vec<Entry> {
    vec![
        Entry {
            aliases: &["account", "login"],
            ..entry(
                SettingsTab::Accounts,
                "settings-accounts-add-login",
                "Add current login",
                "Accounts",
                Gate::Always,
            )
        },
        Entry {
            aliases: &["auto-switch", "rotation"],
            ..entry(
                SettingsTab::Accounts,
                "settings-accounts-auto",
                "Switch accounts automatically",
                "Auto-switch",
                Gate::Always,
            )
        },
        Entry {
            aliases: &["threshold"],
            ..entry(
                SettingsTab::Accounts,
                "settings-accounts-threshold-down",
                "Switch at",
                "Auto-switch",
                Gate::Always,
            )
        },
        entry(
            SettingsTab::Accounts,
            "settings-accounts-hysteresis-down",
            "Hysteresis",
            "Auto-switch",
            Gate::Always,
        ),
        entry(
            SettingsTab::Accounts,
            "settings-accounts-cooldown-0",
            "Cooldown",
            "Auto-switch",
            Gate::Always,
        ),
        entry(
            SettingsTab::Accounts,
            "settings-accounts-strategy-best",
            "Strategy",
            "Auto-switch",
            Gate::Always,
        ),
        Entry {
            aliases: &["forecast", "rate"],
            ..entry(
                SettingsTab::Accounts,
                "settings-accounts-window-30",
                "Forecast window",
                "Auto-switch",
                Gate::Always,
            )
        },
    ]
}

/// Whether a search covers the Accounts page's control `selector`, `None`
/// for a selector of no Accounts control: a control of a saved account's own
/// row always ([`accounts_entries`]: no query needs to land on one), another
/// choice or stepper end of a row when `catalogued` names the row's first.
fn accounts_covered(selector: &str, catalogued: impl Fn(&str) -> bool) -> Option<bool> {
    let part = selector.strip_prefix("settings-accounts-")?;
    let row = ["switch-", "alias-", "up-", "down-", "remove-"];
    if row.iter().any(|prefix| part.starts_with(prefix)) {
        return Some(true);
    }
    let companions = [
        ("threshold-", "settings-accounts-threshold-down"),
        ("hysteresis-", "settings-accounts-hysteresis-down"),
        ("cooldown-", "settings-accounts-cooldown-0"),
        ("strategy-", "settings-accounts-strategy-best"),
        ("window-", "settings-accounts-window-30"),
    ];
    companions
        .into_iter()
        .find(|(prefix, _)| part.starts_with(prefix))
        .map(|(_, first)| catalogued(first))
}

/// The Appearance tab's rows, which the app level draws under no section
/// heading: the colours' editor has its own stops, and these are the font's.
fn appearance_entries() -> Vec<Entry> {
    vec![
        entry(
            SettingsTab::Appearance,
            "appearance-theme",
            "Theme",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-frame-width-down",
            "Frame width",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-ui-family-filter",
            "UI font",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-ui-text-size-down",
            "UI text size",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-ui-weight-regular",
            "UI weight",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-family-filter",
            "Font family",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-size-down",
            "Font size",
            "",
            Gate::Always,
        ),
        entry(
            SettingsTab::Appearance,
            "appearance-term-weight-regular",
            "Terminal weight",
            "",
            Gate::Always,
        ),
    ]
}

/// The App title tab's rows.
fn app_title_entries() -> Vec<Entry> {
    vec![
        entry(
            SettingsTab::AppTitle,
            "settings-count-needs-you",
            "Tab and badge count",
            "App title",
            Gate::Always,
        ),
        entry(
            SettingsTab::AppTitle,
            "settings-title-count",
            "Show the count on the taskbar button",
            "App title",
            Gate::Always,
        ),
        entry(
            SettingsTab::AppTitle,
            "settings-title-product-suffix",
            "Append “ — rustling-tulip” suffix",
            "App title",
            Gate::Always,
        ),
    ]
}

/// The agent pages' rows: each descriptor's Settings fields, then its Launch
/// command rows, under the page's own tab.
fn agent_entries(agents: &[AgentDescriptor]) -> Vec<Entry> {
    agents
        .iter()
        .flat_map(|descriptor| {
            let tab = SettingsTab::Agent(intern(descriptor.id.0.clone()));
            let options = AgentOptions::new(descriptor.clone(), None);
            let fields = options.settings_fields(false);
            let rows: Vec<Entry> = fields
                .iter()
                .enumerate()
                .map(|(index, view)| field_entry(tab, descriptor, index, view))
                .collect();
            let title = || Cow::Owned(descriptor.label.clone());
            rows.into_iter().chain([
                Entry {
                    tab_title: title(),
                    ..entry(
                        tab,
                        SettingsControl::AgentProgram.selector(),
                        "Program",
                        "Launch command",
                        Gate::Always,
                    )
                },
                Entry {
                    tab_title: title(),
                    ..entry(
                        tab,
                        SettingsControl::AgentArgs.selector(),
                        "Extra arguments",
                        "Launch command",
                        Gate::Always,
                    )
                },
            ])
        })
        .collect()
}

/// The row of `view`, field `index` of `descriptor`'s page: its label under
/// the descriptor's label, naming its first control, or the row's own driver
/// id when the field has no control this client can draw.
fn field_entry(
    tab: SettingsTab,
    descriptor: &AgentDescriptor,
    index: usize,
    view: &FieldView,
) -> Entry {
    let selector = field_controls(&descriptor.tag, index, view)
        .first()
        .map_or_else(
            || {
                Cow::Owned(format!(
                    "settings-{}-{}",
                    descriptor.tag,
                    view.id.replace('_', "-")
                ))
            },
            |control| Cow::Borrowed(control.selector()),
        );
    Entry {
        tab,
        tab_title: Cow::Owned(descriptor.label.clone()),
        selector,
        label: Cow::Owned(view.label.clone()),
        within: Cow::Owned(descriptor.label.clone()),
        aliases: &[],
        gate: Gate::Always,
    }
}

/// Drawn controls that are no settings row of their own, with why: the
/// other choice of a choice row, the far end of a stepper, and a row's
/// reset link. No query needs to land on one, so no entry names it.
pub(crate) const EXEMPT: &[(&str, &str)] = &[
    (
        "settings-keep-awake-when-busy",
        "another choice of the Keep this PC awake row",
    ),
    (
        "settings-keep-awake-always",
        "another choice of the Keep this PC awake row",
    ),
    (
        "settings-sidebar-view-tabs",
        "another choice of the Default view row",
    ),
    (
        "settings-leaf-density-compact",
        "another choice of the Leaf density row",
    ),
    (
        "settings-sessions-under-repos-busy",
        "another choice of the Sessions under repos row",
    ),
    (
        "settings-sessions-under-repos-none",
        "another choice of the Sessions under repos row",
    ),
    (
        "settings-repo-sort-active",
        "another choice of the Sort repos row",
    ),
    (
        "settings-repo-sort-needs-you",
        "another choice of the Sort repos row",
    ),
    (
        "settings-chat-mode-comfortable",
        "another choice of the Chat view row",
    ),
    (
        "appearance-frame-width-up",
        "the other end of the Frame width stepper",
    ),
    (
        "appearance-frame-width-reset",
        "the Frame width row's reset link",
    ),
    (
        "appearance-ui-family-list",
        "the UI font row's list, under its filter",
    ),
    (
        "appearance-ui-text-size-up",
        "the other end of the UI text size stepper",
    ),
    (
        "appearance-ui-weight-medium",
        "another choice of the UI weight row",
    ),
    (
        "appearance-ui-weight-semibold",
        "another choice of the UI weight row",
    ),
    (
        "appearance-family-list",
        "the Font family row's list, under its filter",
    ),
    (
        "appearance-size-up",
        "the other end of the Font size stepper",
    ),
    ("appearance-size-reset", "the Font size row's reset link"),
    (
        "appearance-term-weight-medium",
        "another choice of the Terminal weight row",
    ),
    (
        "appearance-term-weight-semibold",
        "another choice of the Terminal weight row",
    ),
    (
        "appearance-term-weight-bold",
        "another choice of the Terminal weight row",
    ),
    (
        "appearance-term-weight-reset",
        "the Terminal weight row's reset link",
    ),
];

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use crate::appearance_stops;
    use crate::settings_view::general_controls;

    fn all_visible() -> Visibility {
        Visibility {
            remote_tab: true,
            this_pc: true,
        }
    }

    fn catalogue() -> Vec<Entry> {
        entries(&agent_registry::descriptors())
    }

    fn found(query: &str) -> Vec<TabHits> {
        results(query, &catalogue(), all_visible())
    }

    fn tabs(hits: &[TabHits]) -> Vec<SettingsTab> {
        hits.iter().map(|hit| hit.tab).collect()
    }

    fn row<'a>(entries: &'a [Entry], label: &str) -> &'a Entry {
        entries
            .iter()
            .find(|entry| entry.label == label)
            .expect("the catalogue holds the row")
    }

    #[test]
    fn a_two_word_query_needs_both_words() {
        let hits = found("chat width");
        assert_eq!(tabs(&hits), [SettingsTab::General]);
        assert_eq!(hits[0].count, 1, "only Maximum chat width holds both words");
        assert!(
            found("width compact").is_empty(),
            "no one entry holds both words"
        );
    }

    #[test]
    fn an_alias_query_finds_the_entry() {
        let entries = catalogue();
        let hits = results("px", &entries, all_visible());
        assert_eq!(tabs(&hits), [SettingsTab::General]);
        assert_eq!(entries[hits[0].first].label, "Maximum chat width");
        let hits = results("comfortable", &entries, all_visible());
        assert_eq!(entries[hits[0].first].label, "Chat view");
    }

    #[test]
    fn a_query_matches_the_section_heading_and_the_tab_title() {
        let entries = catalogue();
        let hits = results("diagnostics", &entries, all_visible());
        assert_eq!(tabs(&hits), [SettingsTab::General]);
        assert_eq!(hits[0].count, 2, "both Diagnostics rows");
        let hits = results("appearance", &entries, all_visible());
        assert_eq!(tabs(&hits), [SettingsTab::Appearance]);
        assert_eq!(hits[0].count, 8, "the tab's title finds its every row");
    }

    #[test]
    fn a_hint_is_not_searched() {
        // Detailed logging's hint reads "Records why each session changes
        // status. Fills the 5 MiB log files faster."
        assert!(
            found("status").is_empty(),
            "a word only the hint holds finds nothing"
        );
        assert!(
            found("records").is_empty(),
            "the hint's other words find nothing either"
        );
        assert!(
            !found("detailed").is_empty(),
            "the row's own label still finds it"
        );
    }

    #[test]
    fn matching_ignores_case() {
        assert!(
            !found("CHAT").is_empty(),
            "the label matches however it is cased"
        );
        assert_eq!(found("CHAT"), found("chat"));
    }

    #[test]
    fn a_blank_query_filters_nothing() {
        assert!(found("").is_empty(), "an empty query finds no tab");
        assert!(found("   ").is_empty(), "a query of spaces finds no tab");
    }

    #[test]
    fn match_counts_are_per_tab() {
        let hits = found("width");
        assert_eq!(tabs(&hits), [SettingsTab::General, SettingsTab::Appearance]);
        assert_eq!(hits[0].count, 1);
        assert_eq!(hits[1].count, 1);
        let hits = found("font");
        assert_eq!(tabs(&hits), [SettingsTab::Appearance]);
        assert_eq!(hits[0].count, 3, "UI font, Font family and Font size");
    }

    #[test]
    fn first_match_is_the_first_entry_in_drawing_order() {
        let entries = catalogue();
        let hits = results("logs", &entries, all_visible());
        assert_eq!(tabs(&hits), [SettingsTab::General]);
        assert_eq!(hits[0].count, 2);
        assert_eq!(
            entries[hits[0].first].label, "Detailed logging",
            "the first row in drawing order, not the first alphabetically"
        );
    }

    #[test]
    fn remote_access_entries_are_hidden_with_the_tab() {
        let entries = catalogue();
        let hidden = Visibility {
            remote_tab: false,
            ..all_visible()
        };
        assert!(
            results("tailnet", &entries, hidden).is_empty(),
            "no Remote access row is drawn without the tab"
        );
        let shown = results("tailnet", &entries, all_visible());
        assert_eq!(tabs(&shown), [SettingsTab::RemoteAccess]);
        assert_eq!(results("remote", &entries, all_visible())[0].count, 3);
        assert!(
            results("remote", &entries, hidden).is_empty(),
            "the alias and the heading go with the tab"
        );
    }

    #[test]
    fn this_pc_rows_are_hidden_on_a_remote_host() {
        let entries = catalogue();
        let remote = Visibility {
            this_pc: false,
            ..all_visible()
        };
        assert_eq!(results("logs", &entries, all_visible())[0].count, 2);
        assert!(
            results("logs", &entries, remote).is_empty(),
            "the Diagnostics rows are This PC only"
        );
    }

    #[test]
    fn keep_awake_rows_are_searchable_before_the_daemon_reports() {
        let entries = catalogue();
        // The row is drawn locked before the daemon reports, not left out.
        let hits = results("sleep", &entries, all_visible());
        assert_eq!(
            tabs(&hits),
            [SettingsTab::General],
            "the row is drawn locked before the daemon reports"
        );
        assert_eq!(entries[hits[0].first].label, "Keep this PC awake");
    }

    #[test]
    fn only_drawn_entries_are_searched() {
        let general = |logging: bool| DrawnTab {
            tab: SettingsTab::General,
            stops: general_controls(true, logging)
                .iter()
                .map(|control| control.selector())
                .collect(),
            rows: Vec::new(),
        };
        let drawn_now = drawn(catalogue(), &[general(false)]);
        assert!(
            results("logs", &drawn_now, all_visible()).is_empty(),
            "rows the page does not draw are not found"
        );
        assert!(
            results("font", &drawn_now, all_visible()).is_empty(),
            "a tab not drawn is not searched"
        );
        let drawn_now = drawn(catalogue(), &[general(true)]);
        assert_eq!(results("logs", &drawn_now, all_visible())[0].count, 2);

        let dashboard = |rows: Vec<String>| DrawnTab {
            tab: SettingsTab::Dashboard,
            stops: vec!["settings-dashboard-enabled"],
            rows,
        };
        let drawn_now = drawn(catalogue(), &[dashboard(Vec::new())]);
        assert!(
            results("provider", &drawn_now, all_visible()).is_empty(),
            "the Provider row is out while no provider is drawn"
        );
        let drawn_now = drawn(
            catalogue(),
            &[dashboard(vec!["settings-dashboard-provider".to_owned()])],
        );
        let hits = results("provider", &drawn_now, all_visible());
        assert_eq!(tabs(&hits), [SettingsTab::Dashboard]);
        assert_eq!(drawn_now[hits[0].first].label, "Provider");
    }

    #[test]
    fn an_agent_tab_title_matches_its_launch_command_rows() {
        let descriptor = agent_api::test_agent::DESCRIPTOR.clone();
        let entries = entries(std::slice::from_ref(&descriptor));
        let tab = SettingsTab::Agent(intern(descriptor.id.0.clone()));
        let rows = entries.iter().filter(|entry| entry.tab == tab).count();
        let title = descriptor.label.to_lowercase();
        let hits = results(&format!("{title} program"), &entries, all_visible());
        assert_eq!(tabs(&hits), [tab]);
        assert_eq!(entries[hits[0].first].label, "Program");
        let hits = results(&title, &entries, all_visible());
        let hit = hits
            .iter()
            .find(|hit| hit.tab == tab)
            .expect("the agent's tab holds the match");
        assert_eq!(hit.count, rows, "the tab's title finds every row on it");
    }

    #[test]
    fn the_chat_and_logging_rows_are_catalogued() {
        let entries = catalogue();
        let chat = row(&entries, "Chat view");
        assert_eq!(chat.tab, SettingsTab::General);
        assert_eq!(chat.within, "Chat");
        assert_eq!(chat.selector, "settings-chat-mode-compact");
        assert_eq!(chat.aliases, &["tool calls", "compact", "comfortable"][..]);
        assert_eq!(chat.gate, Gate::Always);
        let width = row(&entries, "Maximum chat width");
        assert_eq!(width.selector, "settings-chat-width-input");
        assert_eq!(width.aliases, &["width", "column", "px"][..]);
        let logging = row(&entries, "Detailed logging");
        assert_eq!(logging.within, "Diagnostics");
        assert_eq!(logging.gate, Gate::ThisPcOnly);
        assert_eq!(logging.aliases, &["log", "logs", "debug", "trace"][..]);
        let open = row(&entries, "Open logs folder");
        assert_eq!(open.gate, Gate::ThisPcOnly);
        assert_eq!(
            open.aliases,
            &["log", "logs", "native.log", "daemon.log"][..]
        );
    }

    #[test]
    fn agent_page_fields_are_searchable() {
        let rows = catalogue();
        for descriptor in agent_registry::descriptors() {
            let tab = SettingsTab::Agent(intern(descriptor.id.0.clone()));
            let options = AgentOptions::new(descriptor.clone(), None);
            let fields = options.settings_fields(false);
            assert!(
                !fields.is_empty(),
                "{} marks no Settings field",
                descriptor.id.0
            );
            for view in &fields {
                let found = rows
                    .iter()
                    .find(|row| row.tab == tab && row.label == view.label)
                    .expect("a field of the page is searchable");
                assert_eq!(found.within, descriptor.label);
                assert!(
                    !found.selector.is_empty(),
                    "{} names no selector",
                    view.label
                );
            }
            for label in ["Program", "Extra arguments"] {
                let found = rows
                    .iter()
                    .find(|row| row.tab == tab && row.label == label)
                    .expect("the launch command rows are searchable");
                assert_eq!(found.within, "Launch command");
            }
        }
    }

    /// The controls a unit test can see being drawn: the General tab's with
    /// both gates open, the Appearance tab's app-level stops in the order Tab
    /// walks them, and each agent page's first control of every field.
    fn drawn_selectors() -> Vec<String> {
        let mut drawn: Vec<String> = general_controls(true, true)
            .iter()
            .map(|control| control.selector().to_owned())
            .collect();
        drawn.extend(
            appearance_stops::stops(false, true, false, |_| true)
                .into_iter()
                .map(|stop| SettingsControl::Appearance(stop).selector().to_owned()),
        );
        for descriptor in agent_registry::descriptors() {
            let options = AgentOptions::new(descriptor.clone(), None);
            for (index, view) in options.settings_fields(false).iter().enumerate() {
                // A choice field draws one row, and the catalogue names its
                // first choice.
                if let Some(control) = field_controls(&descriptor.tag, index, view).first() {
                    drawn.push(control.selector().to_owned());
                }
            }
        }
        drawn.extend(
            [SettingsControl::AgentProgram, SettingsControl::AgentArgs]
                .map(|control| control.selector().to_owned()),
        );
        drawn
    }

    /// These two guards check the controls a unit test can enumerate: the
    /// General tab's with both gates open, the Appearance tab's app-level
    /// stops, and each agent page's. The other tabs' stop orders are
    /// `RootView` methods that need live state.
    #[test]
    fn every_drawn_control_has_a_catalogue_entry() {
        let drawn = drawn_selectors();
        let rows = catalogue();
        for selector in &drawn {
            if EXEMPT
                .iter()
                .any(|(exempt, _)| *exempt == selector.as_str())
            {
                continue;
            }
            assert!(
                rows.iter()
                    .any(|row| row.selector.as_ref() == selector.as_str()),
                "the drawn control {selector} has no catalogue entry"
            );
        }
        for (selector, why) in EXEMPT {
            assert!(
                drawn.iter().any(|drawn| drawn.as_str() == *selector),
                "the exemption {selector} ({why}) names no drawn control"
            );
        }
    }

    /// The reverse: a catalogue entry of a guarded tab names a control the
    /// page really draws, so a renamed or removed row is caught.
    #[test]
    fn every_catalogue_entry_names_a_drawn_selector() {
        let drawn = drawn_selectors();
        for row in catalogue() {
            let guarded = row.tab == SettingsTab::General
                || row.tab == SettingsTab::Appearance
                || matches!(row.tab, SettingsTab::Agent(_));
            if !guarded {
                continue;
            }
            assert!(
                drawn
                    .iter()
                    .any(|selector| selector.as_str() == row.selector.as_ref()),
                "the catalogue entry {} names no drawn control",
                row.label
            );
        }
    }

    #[test]
    fn every_entry_tab_is_listed() {
        let agents = agent_registry::descriptors();
        let ids: Vec<&str> = agents
            .iter()
            .map(|descriptor| descriptor.id.0.as_str())
            .collect();
        for row in entries(&agents) {
            match row.tab {
                SettingsTab::Agent(id) => {
                    assert!(ids.contains(&id), "the agent page {id} is no listed agent");
                }
                tab => assert!(SettingsTab::FIXED.contains(&tab), "{tab:?} is no tab"),
            }
        }
    }
}
