//! The Linear hover card's model: each host's copy of the Linear issues its
//! daemon sent, whether that host has a key configured and which
//! identifiers it was asked for, and the card a hovered identifier shows
//! from them. Plain Rust; the hover timer and `linear_card_view` only
//! render it.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use protocol::LinearIssue;

use crate::dashboard_rows::{DASH, format_age};
use crate::links::{IssueContext, LinkKind, tip_text, title_issue};
use crate::sidebar::ContainerKind;
use crate::target::HostKey;

/// The no-key notice.
pub(crate) const NO_KEY: &str =
    "Add a Linear API key in Settings › Linear to see this issue's details.";
/// The loading notice.
pub(crate) const LOADING: &str = "Fetching from Linear";
/// What the card's button says, and what it does.
pub(crate) const OPEN_IN_LINEAR: &str = "Open in Linear";
/// The foot of a session title chip's card: a plain click on the chip
/// opens the issue.
pub(crate) const CHIP_FOOT: &str = "Click to open";
/// The pane id of a phone card raised from the chat or a title, which no
/// pane owns.
pub(crate) const PHONE_CARD_PANE: &str = "phone-card";

/// Every host's Linear issues, key flag and requests in flight.
#[derive(Debug, Default)]
pub(crate) struct LinearCache {
    hosts: HashMap<HostKey, HostIssues>,
}

/// One host's part of the [`LinearCache`].
#[derive(Debug, Default)]
struct HostIssues {
    /// The issues the host's daemon sent, by identifier.
    issues: HashMap<String, LinearIssue>,
    /// The host's daemon has a Linear key configured.
    key_configured: bool,
    /// The identifiers asked for whose answer has not come yet.
    in_flight: HashSet<String>,
}

impl LinearCache {
    /// `host`'s `LinearContext` said whether it has a key configured.
    pub(crate) fn set_key_configured(&mut self, host: &HostKey, configured: bool) {
        self.hosts.entry(host.clone()).or_default().key_configured = configured;
    }

    /// `host`'s daemon sent `issues`: each replaces the copy of the same
    /// identifier and ends the request for it in flight. An empty reply,
    /// which only answers a request whose issues Linear lacks or could not
    /// give, ends every request of `host` in flight; a broadcast another
    /// client's fetch caused names only what it fetched.
    pub(crate) fn upsert(&mut self, host: &HostKey, issues: &[LinearIssue]) {
        let entry = self.hosts.entry(host.clone()).or_default();
        if issues.is_empty() {
            entry.in_flight.clear();
        }
        for issue in issues {
            entry.in_flight.remove(&issue.identifier);
            entry.issues.insert(issue.identifier.clone(), issue.clone());
        }
    }

    /// Marks a request for `identifier` on `host` in flight. Returns whether
    /// it should be sent: not while one for it already is.
    pub(crate) fn ask(&mut self, host: &HostKey, identifier: &str) -> bool {
        self.hosts
            .entry(host.clone())
            .or_default()
            .in_flight
            .insert(identifier.to_owned())
    }

    /// `host` connected again: nothing asked before is in flight any more.
    /// Its copies stay until its daemon sends fresh ones.
    pub(crate) fn reconnected(&mut self, host: &HostKey) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.in_flight.clear();
        }
    }

    /// Drops everything `host`'s daemon sent.
    pub(crate) fn forget(&mut self, host: &HostKey) {
        self.hosts.remove(host);
    }

    /// The card for `identifier` on `host`, opening `web_url` until a copy
    /// names its own, with ages counted to `now`.
    pub(crate) fn card(
        &self,
        host: &HostKey,
        identifier: &str,
        web_url: &str,
        now: DateTime<Utc>,
    ) -> Card {
        let entry = self.hosts.get(host);
        let lookup = Lookup {
            identifier,
            web_url,
            key_configured: entry.is_some_and(|entry| entry.key_configured),
            in_flight: entry.is_some_and(|entry| entry.in_flight.contains(identifier)),
        };
        card(
            entry.and_then(|entry| entry.issues.get(identifier)),
            &lookup,
            now,
        )
    }
}

/// A session title's Linear chip: the title's first identifier of one of
/// its host's teams, the title's rest, the dot's colour and the card a
/// hover shows, whose address a click opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleChip {
    pub host: HostKey,
    pub identifier: String,
    /// The title with the identifier cut out; empty when it was all of it.
    pub rest: String,
    /// Linear's colour for the issue's state, as `0xrrggbb`; `None`, with no
    /// copy or no colour, draws the theme's muted dot.
    pub dot: Option<u32>,
    /// The web address built from the host's Linear context, which the
    /// card opens until a copy names the issue's own.
    pub(crate) web_url: String,
    pub(crate) card: Card,
}

impl TitleChip {
    /// The address a click on the chip opens: the copy's own, else the web
    /// address built from the host's Linear context.
    pub(crate) fn open_url(&self) -> &str {
        &self.card.open_url
    }
}

/// What the title chips of one render read: each host's Linear context and
/// the issues its daemon sent, with ages counted to `now`.
pub(crate) struct TitleChips<'a> {
    pub contexts: &'a HashMap<HostKey, IssueContext>,
    pub issues: &'a LinearCache,
    pub now: DateTime<Utc>,
}

impl TitleChips<'_> {
    /// The chip of `host`'s session titled `title`; `None` when the title
    /// names no identifier of the host's teams, as on a host with no context
    /// or with Linear off.
    pub(crate) fn chip(&self, host: &HostKey, title: &str) -> Option<TitleChip> {
        let context = self
            .contexts
            .get(host)
            .filter(|context| context.is_enabled())?;
        let (identifier, rest) = title_issue(title, context)?;
        let web_url = context.bare_address(&identifier)?;
        let card = self.issues.card(host, &identifier, &web_url, self.now);
        Some(TitleChip {
            host: host.clone(),
            dot: card.state.as_ref().and_then(|state| state.color),
            identifier,
            rest,
            web_url,
            card,
        })
    }

    /// The chip of `host`'s sidebar heading of `kind` named `name`: only a
    /// lone shell's heading, whose name is its session's title, takes one;
    /// a repo, workspace or folder heading names a folder.
    pub(crate) fn container_chip(
        &self,
        kind: ContainerKind,
        host: &HostKey,
        name: &str,
    ) -> Option<TitleChip> {
        (kind == ContainerKind::Shell)
            .then(|| self.chip(host, name))
            .flatten()
    }
}

/// The card a pane shows: the pane, its host, the identifier and the
/// address the link was detected with. At most one exists at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardAnchor {
    pub host: HostKey,
    pub pane_id: String,
    pub identifier: String,
    pub web_url: String,
}

/// What the cache knows of a hovered identifier besides its copy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lookup<'a> {
    pub identifier: &'a str,
    /// The address the link was detected with, opened until a copy names
    /// the issue's own.
    pub web_url: &'a str,
    pub key_configured: bool,
    pub in_flight: bool,
}

/// What the card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Card {
    pub identifier: String,
    /// Linear's priority label, drawn beside a neutral dot.
    pub priority: Option<String>,
    pub state: Option<CardState>,
    pub title: Option<String>,
    /// Project, Release, Assignee and Updated, in that order.
    pub fields: Vec<(&'static str, String)>,
    pub notice: Option<Notice>,
    /// The address Open in Linear and a click on the title open.
    pub open_url: String,
}

/// The issue's workflow state: its name and Linear's colour for it, as
/// `0xrrggbb`; `None` draws a neutral dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardState {
    pub name: String,
    pub color: Option<u32>,
}

/// The one-line notice a card carries when it cannot show a fresh copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notice {
    /// The host has no Linear key.
    NoKey,
    /// The issue was asked for and its answer has not come.
    Loading,
    /// The answer came without the issue.
    NotFound,
    /// The last refresh failed; the copy is as old as the age.
    Unreachable(String),
}

impl Notice {
    /// The notice's line on the card of `identifier`.
    pub(crate) fn text(&self, identifier: &str) -> String {
        match self {
            Self::NoKey => NO_KEY.to_owned(),
            Self::Loading => LOADING.to_owned(),
            Self::NotFound => format!("Linear has no issue {identifier}."),
            Self::Unreachable(age) => format!("Couldn't reach Linear · as of {age} ago"),
        }
    }

    /// The notice carries the Open in Linear button.
    pub(crate) fn offers_open(&self) -> bool {
        matches!(self, Self::NoKey)
    }

    /// The notice is drawn in the asking colour.
    pub(crate) fn warns(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

impl Card {
    /// The line at the card's foot, the Ctrl tip's.
    pub(crate) fn foot() -> &'static str {
        tip_text(LinkKind::Issue)
    }

    /// The card's text, top to bottom: the identifier, priority, state,
    /// title, each field as `Label: value`, the notice and the foot.
    pub(crate) fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.identifier.clone()];
        lines.extend(self.priority.clone());
        lines.extend(self.state.as_ref().map(|state| state.name.clone()));
        lines.extend(self.title.clone());
        lines.extend(
            self.fields
                .iter()
                .map(|(label, value)| format!("{label}: {value}")),
        );
        lines.extend(
            self.notice
                .as_ref()
                .map(|notice| notice.text(&self.identifier)),
        );
        if self.notice.as_ref().is_some_and(Notice::offers_open) {
            lines.push(OPEN_IN_LINEAR.to_owned());
        }
        lines.push(Self::foot().to_owned());
        lines
    }
}

/// The card for `lookup`'s identifier from its copy `issue`, if the cache
/// holds one, with ages counted to `now`.
pub(crate) fn card(issue: Option<&LinearIssue>, lookup: &Lookup<'_>, now: DateTime<Utc>) -> Card {
    let bare = Card {
        identifier: lookup.identifier.to_owned(),
        priority: None,
        state: None,
        title: None,
        fields: Vec::new(),
        notice: None,
        open_url: lookup.web_url.to_owned(),
    };
    let notice = if !lookup.key_configured {
        Notice::NoKey
    } else if let Some(issue) = issue {
        return issue_card(bare, issue, now);
    } else if lookup.in_flight {
        Notice::Loading
    } else {
        Notice::NotFound
    };
    Card {
        notice: Some(notice),
        ..bare
    }
}

/// `bare` filled from copy `issue`, with the unreachable notice when the
/// copy is stale.
fn issue_card(bare: Card, issue: &LinearIssue, now: DateTime<Utc>) -> Card {
    let or_dash = |value: Option<String>| value.unwrap_or_else(|| DASH.to_owned());
    let releases = (!issue.releases.is_empty()).then(|| issue.releases.join(", "));
    let updated = issue.updated_at.map(|at| age_ago(at, now));
    let priority = if issue.priority_label.is_empty() {
        "No priority".to_owned()
    } else {
        issue.priority_label.clone()
    };
    Card {
        priority: Some(priority),
        state: issue.state.as_ref().map(|state| CardState {
            name: state.name.clone(),
            color: parse_color(&state.color),
        }),
        title: Some(issue.title.clone()),
        fields: vec![
            ("Project", or_dash(issue.project.clone())),
            ("Release", or_dash(releases)),
            ("Assignee", or_dash(issue.assignee.clone())),
            ("Updated", or_dash(updated)),
        ],
        notice: issue
            .stale
            .then(|| Notice::Unreachable(card_age(issue.fetched_at, now))),
        open_url: issue.url.clone(),
        ..bare
    }
}

/// How long ago `at` was at `now`, as the card reads it: "1d ago" from a
/// day on, else the daemon's wording ("<1m ago", "12m ago", "23h59m ago").
fn age_ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format!("{} ago", card_age(at, now))
}

/// The card's age of `at` at `now`, without "ago": whole days from a day
/// on, else [`format_age`]'s.
fn card_age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let days = (now - at).num_days();
    if days >= 1 {
        format!("{days}d")
    } else {
        format_age(at, now)
    }
}

/// Linear's `#rrggbb` as `0xrrggbb`; `None` for anything else.
fn parse_color(color: &str) -> Option<u32> {
    let hex = color.strip_prefix('#').filter(|hex| hex.len() == 6)?;
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use protocol::{LinearState, LinearStateKind};

    use super::*;

    const WEB: &str = "https://linear.app/acme/issue/RT-12";

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap_or_default()
    }

    /// A fresh copy of RT-12 with every field set, fetched a minute ago.
    fn issue() -> LinearIssue {
        LinearIssue {
            identifier: "RT-12".to_owned(),
            title: "Hover card".to_owned(),
            url: "https://linear.app/acme/issue/RT-12/hover-card".to_owned(),
            state: Some(LinearState {
                name: "In Progress".to_owned(),
                kind: LinearStateKind::Started,
                color: "#f2c94c".to_owned(),
            }),
            priority: 2,
            priority_label: "High".to_owned(),
            project: Some("Linear links".to_owned()),
            releases: vec!["vNext".to_owned(), "v1".to_owned()],
            assignee: Some("Ada".to_owned()),
            updated_at: Some(now() - TimeDelta::minutes(12)),
            fetched_at: now() - TimeDelta::minutes(1),
            stale: false,
        }
    }

    fn lookup(key_configured: bool, in_flight: bool) -> Lookup<'static> {
        Lookup {
            identifier: "RT-12",
            web_url: WEB,
            key_configured,
            in_flight,
        }
    }

    #[test]
    fn a_fresh_copy_shows_every_field_and_no_notice() {
        let card = card(Some(&issue()), &lookup(true, false), now());
        assert_eq!(
            card,
            Card {
                identifier: "RT-12".to_owned(),
                priority: Some("High".to_owned()),
                state: Some(CardState {
                    name: "In Progress".to_owned(),
                    color: Some(0x00f2_c94c),
                }),
                title: Some("Hover card".to_owned()),
                fields: vec![
                    ("Project", "Linear links".to_owned()),
                    ("Release", "vNext, v1".to_owned()),
                    ("Assignee", "Ada".to_owned()),
                    ("Updated", "12m ago".to_owned()),
                ],
                notice: None,
                open_url: "https://linear.app/acme/issue/RT-12/hover-card".to_owned(),
            }
        );
    }

    #[test]
    fn missing_fields_show_a_dash_and_a_bad_colour_no_colour() {
        let mut bare = issue();
        bare.project = None;
        bare.releases = Vec::new();
        bare.assignee = None;
        bare.updated_at = None;
        bare.priority = 0;
        bare.priority_label = String::new();
        if let Some(state) = bare.state.as_mut() {
            state.color = "orange".to_owned();
        }
        let card = card(Some(&bare), &lookup(true, false), now());
        assert_eq!(card.priority.as_deref(), Some("No priority"));
        assert_eq!(card.state.and_then(|state| state.color), None);
        assert!(
            card.fields.iter().all(|(_, value)| value == DASH),
            "{:?}",
            card.fields
        );
    }

    #[test]
    fn no_key_shows_the_identifier_and_the_no_key_notice() {
        let card = card(None, &lookup(false, false), now());
        assert_eq!(card.notice, Some(Notice::NoKey));
        assert_eq!(card.title, None);
        assert_eq!(card.open_url, WEB);
        assert_eq!(
            card.lines(),
            [
                "RT-12",
                "Add a Linear API key in Settings › Linear to see this issue's details.",
                "Open in Linear",
                "Ctrl+click to open"
            ]
        );
    }

    #[test]
    fn in_flight_with_no_copy_is_loading() {
        let card = card(None, &lookup(true, true), now());
        assert_eq!(card.notice, Some(Notice::Loading));
        assert_eq!(
            card.lines(),
            ["RT-12", "Fetching from Linear", "Ctrl+click to open"]
        );
    }

    #[test]
    fn an_answer_without_the_issue_says_linear_has_none() {
        let card = card(None, &lookup(true, false), now());
        assert_eq!(card.notice, Some(Notice::NotFound));
        assert_eq!(
            card.lines(),
            ["RT-12", "Linear has no issue RT-12.", "Ctrl+click to open"]
        );
    }

    #[test]
    fn a_stale_copy_shows_the_card_and_the_unreachable_line() {
        let mut stale = issue();
        stale.stale = true;
        stale.fetched_at = now() - TimeDelta::minutes(25);
        let card = card(Some(&stale), &lookup(true, false), now());
        assert_eq!(card.title.as_deref(), Some("Hover card"));
        assert_eq!(card.notice, Some(Notice::Unreachable("25m".to_owned())));
        assert!(card.notice.as_ref().is_some_and(Notice::warns));
        assert!(
            card.lines()
                .contains(&"Couldn't reach Linear · as of 25m ago".to_owned())
        );
    }

    #[test]
    fn ages_read_in_days_past_a_day() {
        let updated = |age: TimeDelta| {
            let mut copy = issue();
            copy.updated_at = Some(now() - age);
            let card = card(Some(&copy), &lookup(true, false), now());
            card.fields
                .iter()
                .find(|(label, _)| *label == "Updated")
                .map(|(_, value)| value.clone())
        };
        let hours_minutes = |h: i64, m: i64| TimeDelta::hours(h) + TimeDelta::minutes(m);
        assert_eq!(
            updated(hours_minutes(23, 59)).as_deref(),
            Some("23h59m ago")
        );
        assert_eq!(updated(TimeDelta::hours(24)).as_deref(), Some("1d ago"));
        assert_eq!(updated(hours_minutes(45, 37)).as_deref(), Some("1d ago"));
        assert_eq!(updated(TimeDelta::hours(49)).as_deref(), Some("2d ago"));

        let mut stale = issue();
        stale.stale = true;
        stale.fetched_at = now() - TimeDelta::days(3);
        let card = card(Some(&stale), &lookup(true, false), now());
        assert!(
            card.lines()
                .contains(&"Couldn't reach Linear · as of 3d ago".to_owned()),
            "{:?}",
            card.lines()
        );
    }

    #[test]
    fn a_cached_reply_fills_the_card_and_ends_the_request() {
        let mut cache = LinearCache::default();
        let host = HostKey::Local;
        cache.set_key_configured(&host, true);
        assert!(cache.ask(&host, "RT-12"));
        assert_eq!(
            cache.card(&host, "RT-12", WEB, now()).notice,
            Some(Notice::Loading)
        );
        cache.upsert(&host, &[]);
        assert_eq!(
            cache.card(&host, "RT-12", WEB, now()).notice,
            Some(Notice::NotFound),
            "any reply ends the request"
        );
        cache.upsert(&host, &[issue()]);
        assert_eq!(
            cache.card(&host, "RT-12", WEB, now()).title.as_deref(),
            Some("Hover card")
        );
    }

    #[test]
    fn an_upsert_replaces_the_copy() {
        let mut cache = LinearCache::default();
        let host = HostKey::Local;
        cache.set_key_configured(&host, true);
        cache.upsert(&host, &[issue()]);
        let mut renamed = issue();
        renamed.title = "Hover card, renamed".to_owned();
        cache.upsert(&host, &[renamed]);
        assert_eq!(
            cache.card(&host, "RT-12", WEB, now()).title.as_deref(),
            Some("Hover card, renamed")
        );
    }

    #[test]
    fn a_second_request_while_one_is_in_flight_is_not_sent() {
        let mut cache = LinearCache::default();
        let host = HostKey::Local;
        assert!(cache.ask(&host, "RT-12"), "the first is sent");
        assert!(!cache.ask(&host, "RT-12"), "the second waits for it");
        assert!(cache.ask(&host, "RT-13"), "another identifier is sent");
        cache.reconnected(&host);
        assert!(cache.ask(&host, "RT-12"), "a reconnect ends the request");
        cache.upsert(&host, &[]);
        assert!(cache.ask(&host, "RT-12"), "so does any reply");
    }

    #[test]
    fn one_hosts_issues_never_fill_another_hosts_card() {
        let mut cache = LinearCache::default();
        let other = HostKey::Remote("box".to_owned());
        cache.set_key_configured(&HostKey::Local, true);
        cache.set_key_configured(&other, true);
        cache.upsert(&HostKey::Local, &[issue()]);
        assert_eq!(cache.card(&other, "RT-12", WEB, now()).title, None);
        cache.forget(&HostKey::Local);
        assert_eq!(
            cache.card(&HostKey::Local, "RT-12", WEB, now()).notice,
            Some(Notice::NoKey),
            "a forgotten host keeps nothing"
        );
    }

    /// A cache whose host has a key and a request for RT-12 in flight.
    fn asked_for_rt_12() -> LinearCache {
        let mut cache = LinearCache::default();
        cache.set_key_configured(&HostKey::Local, true);
        cache.ask(&HostKey::Local, "RT-12");
        cache
    }

    #[test]
    fn a_broadcast_naming_other_ids_keeps_ours_in_flight() {
        let mut cache = asked_for_rt_12();
        let mut other = issue();
        other.identifier = "RT-99".to_owned();
        cache.upsert(&HostKey::Local, &[other]);
        assert_eq!(
            cache.card(&HostKey::Local, "RT-12", WEB, now()).notice,
            Some(Notice::Loading),
            "a reply for RT-99 says nothing of RT-12"
        );
        assert!(!cache.ask(&HostKey::Local, "RT-12"), "still in flight");
    }

    #[test]
    fn a_reply_naming_ours_ends_it() {
        let mut cache = asked_for_rt_12();
        cache.upsert(&HostKey::Local, &[issue()]);
        let card = cache.card(&HostKey::Local, "RT-12", WEB, now());
        assert_eq!(card.notice, None);
        assert_eq!(card.title.as_deref(), Some("Hover card"));
        assert!(cache.ask(&HostKey::Local, "RT-12"), "no longer in flight");
    }

    #[test]
    fn an_empty_reply_ends_every_in_flight_mark() {
        let mut cache = asked_for_rt_12();
        cache.ask(&HostKey::Local, "RT-13");
        cache.upsert(&HostKey::Local, &[]);
        for identifier in ["RT-12", "RT-13"] {
            assert_eq!(
                cache.card(&HostKey::Local, identifier, WEB, now()).notice,
                Some(Notice::NotFound),
                "{identifier}"
            );
        }
    }

    /// This PC has the team RT, the remote host `box` the team YAAT, both
    /// in workspace `acme` with a key; This PC holds a copy of RT-12.
    fn two_hosts() -> (HashMap<HostKey, IssueContext>, LinearCache) {
        let context = |key: &str| IssueContext::new(&[key.to_owned()], Some("acme"), true);
        let contexts = HashMap::from([
            (HostKey::Local, context("RT")),
            (HostKey::Remote("box".to_owned()), context("YAAT")),
        ]);
        let mut issues = LinearCache::default();
        issues.set_key_configured(&HostKey::Local, true);
        issues.upsert(&HostKey::Local, &[issue()]);
        (contexts, issues)
    }

    #[test]
    fn a_title_chip_takes_the_identifier_of_its_own_hosts_teams_only() {
        let (contexts, issues) = two_hosts();
        let chips = TitleChips {
            contexts: &contexts,
            issues: &issues,
            now: now(),
        };
        let remote = HostKey::Remote("box".to_owned());
        let named = |host: &HostKey, title: &str| {
            chips.chip(host, title).map(|chip| {
                let url = chip.open_url().to_owned();
                (chip.host, chip.identifier, chip.rest, url)
            })
        };
        assert_eq!(named(&remote, "RT-7 x"), None, "RT is not box's team");
        assert_eq!(
            named(&HostKey::Local, "RT-7 x"),
            Some((
                HostKey::Local,
                "RT-7".to_owned(),
                "x".to_owned(),
                "https://linear.app/acme/issue/RT-7".to_owned()
            )),
            "RT is This PC's team"
        );
        assert_eq!(
            named(&remote, "YAAT-3: radar"),
            Some((
                remote.clone(),
                "YAAT-3".to_owned(),
                "radar".to_owned(),
                "https://linear.app/acme/issue/YAAT-3".to_owned()
            )),
            "YAAT is box's team"
        );
        assert_eq!(named(&HostKey::Local, "YAAT-3: radar"), None);
    }

    #[test]
    fn a_title_chip_reads_its_hosts_copy_for_the_dot() {
        let (contexts, issues) = two_hosts();
        let chips = TitleChips {
            contexts: &contexts,
            issues: &issues,
            now: now(),
        };
        let drawn = |title: &str| {
            chips.chip(&HostKey::Local, title).map(|chip| {
                let url = chip.open_url().to_owned();
                (chip.dot, chip.rest, url)
            })
        };
        assert_eq!(
            drawn("RT-12 Hover card"),
            Some((Some(0x00f2_c94c), "Hover card".to_owned(), issue().url)),
            "the copy's state colour and its own address"
        );
        assert_eq!(
            drawn("RT-7 x").map(|(dot, _, _)| dot),
            Some(None),
            "no copy, a muted dot"
        );
    }

    #[test]
    fn a_host_with_no_linear_context_has_no_title_chip() {
        let (contexts, issues) = two_hosts();
        let chips = TitleChips {
            contexts: &contexts,
            issues: &issues,
            now: now(),
        };
        let unknown = HostKey::Remote("elsewhere".to_owned());
        assert_eq!(chips.chip(&unknown, "RT-7 x"), None);
        assert_eq!(chips.chip(&unknown, "YAAT-3 x"), None);
    }

    #[test]
    fn only_a_lone_shell_container_takes_a_title_chip() {
        let (contexts, issues) = two_hosts();
        let chips = TitleChips {
            contexts: &contexts,
            issues: &issues,
            now: now(),
        };
        let name = "RT-133 smoke chip";
        let shell = chips.container_chip(ContainerKind::Shell, &HostKey::Local, name);
        assert_eq!(
            shell.map(|chip| (chip.identifier, chip.rest)),
            Some(("RT-133".to_owned(), "smoke chip".to_owned())),
            "a lone shell's heading is its session's title"
        );
        for kind in [
            ContainerKind::Workspace,
            ContainerKind::Repo,
            ContainerKind::Dir,
            ContainerKind::Detached,
            ContainerKind::Tab,
            ContainerKind::Unbound,
        ] {
            assert_eq!(
                chips.container_chip(kind, &HostKey::Local, name),
                None,
                "{kind:?} names no session"
            );
        }
        let unknown = HostKey::Remote("elsewhere".to_owned());
        assert_eq!(
            chips.container_chip(ContainerKind::Shell, &unknown, name),
            None,
            "no Linear context, no chip"
        );
    }
}
