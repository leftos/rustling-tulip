//! The providers the summarizer calls over HTTP: one row per provider, holding
//! every fact about it (where to call, which model, how to send the key, what
//! a token costs). The HTTP call reads only the row, so it names no provider.

use protocol::SummarizerProvider;

/// One provider the summarizer can call over HTTP.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ProviderRow {
    /// The id [`protocol::SummarizerSettings::provider`] names.
    pub(crate) id: &'static str,
    /// The Settings picker's label.
    pub(crate) name: &'static str,
    /// The Messages endpoint the call posts to.
    pub(crate) endpoint: &'static str,
    /// The model the request names.
    pub(crate) model: &'static str,
    /// The header that carries the key.
    pub(crate) key_header: &'static str,
    /// The API version header and its value.
    pub(crate) version_header: (&'static str, &'static str),
    /// US dollars per million input tokens; cache tokens are priced the same.
    pub(crate) input_usd_per_million: f64,
    /// US dollars per million output tokens.
    pub(crate) output_usd_per_million: f64,
    /// The environment variable a key source of `env` names by default.
    pub(crate) default_key_var: &'static str,
    /// The line under the Settings choice that says where the text goes.
    pub(crate) privacy: &'static str,
}

/// Every HTTP provider. The endpoint's base URL is the one the Claude plugin's
/// provider table routes a DeepSeek-provider Claude session to
/// (`crates/agents/claude/src/providers.rs`), plus the Messages path; the
/// daemon cannot read the plugin's constant, so a change there is made here
/// too.
pub(crate) static TABLE: [ProviderRow; 1] = [ProviderRow {
    id: "deepseek",
    name: "DeepSeek Flash",
    endpoint: "https://api.deepseek.com/anthropic/v1/messages",
    model: "deepseek-flash",
    key_header: "x-api-key",
    version_header: ("anthropic-version", "2023-06-01"),
    input_usd_per_million: 0.30,
    output_usd_per_million: 1.20,
    default_key_var: "DEEPSEEK_API_KEY",
    privacy: "Summaries send session text to DeepSeek, a provider in China, under its terms.",
}];

/// The row for `id`, or `None` when no HTTP provider has that id.
pub(crate) fn row(id: &str) -> Option<&'static ProviderRow> {
    TABLE.iter().find(|row| row.id == id)
}

/// The HTTP providers as the Settings picker lists them.
pub(crate) fn offered() -> Vec<SummarizerProvider> {
    TABLE
        .iter()
        .map(|row| SummarizerProvider {
            id: row.id.to_owned(),
            name: row.name.to_owned(),
            needs_key: true,
            privacy: row.privacy.to_owned(),
        })
        .collect()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;

    #[test]
    fn the_table_offers_each_row_with_its_privacy_line() {
        let offered = offered();
        assert_eq!(offered.len(), TABLE.len());
        for (provider, row) in offered.iter().zip(TABLE.iter()) {
            assert_eq!(provider.id, row.id);
            assert_eq!(provider.name, row.name);
            assert!(provider.needs_key, "{} takes a key", row.id);
            assert!(
                !provider.privacy.is_empty(),
                "{} says where the text goes",
                row.id
            );
            assert_eq!(provider.privacy, row.privacy);
        }
    }

    #[test]
    fn a_known_provider_has_its_row() {
        let row = row("deepseek").expect("the table has the deepseek row");
        assert_eq!(row.model, "deepseek-flash");
        assert_eq!(row.default_key_var, "DEEPSEEK_API_KEY");
    }

    #[test]
    fn an_unknown_provider_has_no_row() {
        assert!(row("no-such-provider").is_none());
        assert!(row("").is_none());
    }
}
