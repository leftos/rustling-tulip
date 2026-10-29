//! The summarizer's rules: which providers it offers (each registered agent
//! whose plugin has a CLI summarizer, then the HTTP table), the default
//! settings, whether a request makes a call, and the template line used when
//! it does not.

use std::fmt;

use agent_api::{AgentPlugin, SummarizerRequest, SummarizerSpec};
use protocol::{
    AlertKind, SummarizerKeySource, SummarizerProvider, SummarizerSettings, SummarySource,
};

use crate::summarizer::ledger;
use crate::summarizer::prompt::Trigger;
use crate::summarizer::providers::{self, ProviderRow};
use crate::summarizer::reply::{self, NOTIFICATION_MAX_CHARS, SUMMARY_SCHEMA, Summary};

/// The daily cap a fresh install starts with, in US dollars.
pub(crate) const DEFAULT_DAILY_CAP_USD: f64 = 1.0;

/// The template line's headline.
pub(crate) const TEMPLATE_HEADLINE: &str = "Turn finished";

/// A provider the summarizer can call.
#[derive(Clone, Copy)]
pub(crate) enum Provider {
    /// A registered agent's own CLI.
    Cli(&'static dyn AgentPlugin),
    /// A row of the HTTP table.
    Http(&'static ProviderRow),
}

impl Provider {
    /// The id [`SummarizerSettings::provider`] names.
    pub(crate) fn id(&self) -> String {
        match self {
            Self::Cli(plugin) => plugin.descriptor().id.0.clone(),
            Self::Http(row) => row.id.to_owned(),
        }
    }
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(_) => write!(f, "Cli({})", self.id()),
            Self::Http(_) => write!(f, "Http({})", self.id()),
        }
    }
}

/// `plugin`'s CLI summarizer, if it has one.
fn cli_spec(plugin: &dyn AgentPlugin) -> Option<SummarizerSpec> {
    let read_env = |name: &str| std::env::var(name).ok();
    plugin.summarizer(&SummarizerRequest {
        schema: SUMMARY_SCHEMA,
        read_env: &read_env,
    })
}

/// The registered agents with a CLI summarizer, in registration order.
fn cli_providers() -> impl Iterator<Item = (&'static dyn AgentPlugin, SummarizerSpec)> {
    agent_registry::all()
        .iter()
        .copied()
        .filter_map(|plugin| cli_spec(plugin).map(|spec| (plugin, spec)))
}

/// Every provider as the Settings picker lists it: the CLI summarizers, which
/// need no key, then the HTTP table.
pub(crate) fn offered() -> Vec<SummarizerProvider> {
    cli_providers()
        .map(|(plugin, spec)| SummarizerProvider {
            id: plugin.descriptor().id.0.clone(),
            name: spec.label,
            needs_key: false,
            privacy: spec.privacy,
        })
        .chain(providers::offered())
        .collect()
}

/// The offered provider with `id`, or `None` when none is.
pub(crate) fn resolve(id: &str) -> Option<Provider> {
    cli_providers()
        .find(|(plugin, _)| plugin.descriptor().id.0 == id)
        .map(|(plugin, _)| Provider::Cli(plugin))
        .or_else(|| providers::row(id).map(Provider::Http))
}

/// The settings before any are saved: the first CLI summarizer, on, since it
/// needs no key; else the first HTTP provider, off until a key is saved. The
/// cap is [`DEFAULT_DAILY_CAP_USD`] and the key source the stored key.
pub(crate) fn default_settings() -> SummarizerSettings {
    let (enabled, provider) = match cli_providers().next() {
        Some((plugin, _)) => (true, plugin.descriptor().id.0.clone()),
        None => (
            false,
            providers::TABLE
                .first()
                .map_or_else(String::new, |row| row.id.to_owned()),
        ),
    };
    SummarizerSettings {
        enabled,
        provider,
        key_source: SummarizerKeySource::Stored,
        daily_cap_usd: DEFAULT_DAILY_CAP_USD,
    }
}

/// What a request does.
#[derive(Debug)]
pub(crate) enum Plan {
    /// No client is connected: no call and no line.
    Idle,
    /// Summaries are off, no offered provider is set, or the cap stops the
    /// trigger: the template line, with no call.
    Template,
    /// Call this provider.
    Call(Provider),
}

/// The plan for a `trigger` request with `clients` connected, under
/// `settings`, with `spent_usd` spent today.
pub(crate) fn plan(
    clients: usize,
    settings: &SummarizerSettings,
    trigger: Trigger,
    spent_usd: f64,
) -> Plan {
    if clients == 0 {
        return Plan::Idle;
    }
    if !settings.enabled {
        return Plan::Template;
    }
    let Some(provider) = resolve(&settings.provider) else {
        return Plan::Template;
    };
    if !ledger::allows(trigger, spent_usd, settings.daily_cap_usd) {
        return Plan::Template;
    }
    Plan::Call(provider)
}

/// One summary line and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// The kind a model gave; `None` for the template.
    pub(crate) kind: Option<AlertKind>,
    pub(crate) headline: String,
    pub(crate) notification: String,
    pub(crate) source: SummarySource,
}

impl Line {
    /// A model's summary.
    pub(crate) fn from_summary(summary: Summary) -> Self {
        Self {
            kind: Some(summary.kind),
            headline: summary.headline,
            notification: summary.notification,
            source: SummarySource::Model,
        }
    }
}

/// The line made with no model call: no kind, [`TEMPLATE_HEADLINE`], and the
/// report's first non-blank line cut at a word boundary to
/// [`NOTIFICATION_MAX_CHARS`].
pub(crate) fn template(report: &str) -> Line {
    let first = report
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    Line {
        kind: None,
        headline: TEMPLATE_HEADLINE.to_owned(),
        notification: reply::cut_chars(first, NOTIFICATION_MAX_CHARS),
        source: SummarySource::Template,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> SummarizerSettings {
        let settings = default_settings();
        assert!(settings.enabled, "a CLI summarizer is registered");
        settings
    }

    #[test]
    fn no_call_is_made_with_no_client_connected() {
        assert!(matches!(plan(0, &on(), Trigger::Stop, 0.0), Plan::Idle));
        assert!(matches!(plan(1, &on(), Trigger::Stop, 0.0), Plan::Call(_)));
    }

    #[test]
    fn spend_past_half_the_cap_stops_refreshes() {
        let settings = on();
        let half = settings.daily_cap_usd / 2.0;
        assert!(matches!(
            plan(1, &settings, Trigger::Working, half - 0.01),
            Plan::Call(_)
        ));
        assert!(matches!(
            plan(1, &settings, Trigger::Working, half),
            Plan::Template
        ));
        assert!(
            matches!(plan(1, &settings, Trigger::Stop, half), Plan::Call(_)),
            "a stop summary still calls at half the cap"
        );
    }

    #[test]
    fn spend_at_the_cap_switches_to_templates() {
        let settings = on();
        let cap = settings.daily_cap_usd;
        assert!(matches!(
            plan(1, &settings, Trigger::Stop, cap - 0.01),
            Plan::Call(_)
        ));
        assert!(matches!(
            plan(1, &settings, Trigger::Stop, cap),
            Plan::Template
        ));
        assert!(matches!(
            plan(1, &settings, Trigger::Working, cap),
            Plan::Template
        ));
        assert!(ledger::cap_reached(cap, cap));
        assert!(!ledger::cap_reached(cap - 0.01, cap));
    }

    #[test]
    fn a_stored_provider_no_longer_offered_falls_to_templates() {
        let settings = SummarizerSettings {
            provider: "a-provider-no-longer-offered".to_owned(),
            ..on()
        };
        assert!(resolve(&settings.provider).is_none());
        assert!(matches!(
            plan(1, &settings, Trigger::Stop, 0.0),
            Plan::Template
        ));
        let off = SummarizerSettings {
            enabled: false,
            ..on()
        };
        assert!(matches!(plan(1, &off, Trigger::Stop, 0.0), Plan::Template));
    }

    #[test]
    fn the_template_is_turn_finished_with_the_first_non_blank_line_cut_at_100() {
        let line = template("\n   \n  Fixed the parser.  \nAnd ran the tests.");
        assert_eq!(line.kind, None);
        assert_eq!(line.headline, "Turn finished");
        assert_eq!(line.notification, "Fixed the parser.");
        assert_eq!(line.source, SummarySource::Template);

        let long = "abcde ".repeat(19);
        let cut = template(&long).notification;
        assert!(cut.chars().count() <= 100, "{cut}");
        assert_eq!(cut, "abcde ".repeat(16).trim_end());
        assert_eq!(template("").notification, "");
    }
}
