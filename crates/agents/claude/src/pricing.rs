//! The Claude models' API list prices and context windows, for the
//! Dashboard's cost and context figures.
//!
//! Both tables are copied from ops-tower: the prices and model matching from
//! `src/costs/transcripts.rs`, which took them from Anthropic's model table in
//! the bundled claude-api skill (cached 2026-10-06), with the cache-write
//! multipliers of that skill's `shared/prompt-caching.md`; the context window
//! from `src/agents/models.rs`, the one Claude Code's statusline payload
//! reports for the model.

/// The model families a `claude-` model whose id is not in [`MODEL_RATES`]
/// is priced by: the family's first row.
const FAMILIES: [&str; 5] = ["fable", "mythos", "opus", "sonnet", "haiku"];

/// What a 5-minute cache write costs, as a multiple of the input price.
const CACHE_WRITE_5M: f64 = 1.25;
/// What a 1-hour cache write costs, as a multiple of the input price.
const CACHE_WRITE_1H: f64 = 2.0;

/// One model's API list prices in US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ModelRates {
    id: &'static str,
    input: f64,
    output: f64,
    cache_read: f64,
}

/// The Claude models' API list prices. Each family's first row prices a
/// model of that family no row names.
const MODEL_RATES: [ModelRates; 14] = [
    rates("claude-fable-5-1", 10.0, 50.0, 0.25),
    rates("claude-mythos-5-1", 10.0, 50.0, 0.25),
    rates("claude-fable-5", 10.0, 50.0, 1.00),
    rates("claude-mythos-5", 10.0, 50.0, 1.00),
    rates("claude-opus-5-5", 4.0, 20.0, 0.20),
    rates("claude-opus-5", 5.0, 25.0, 0.50),
    rates("claude-opus-4-8", 5.0, 25.0, 0.50),
    rates("claude-opus-4-7", 5.0, 25.0, 0.50),
    rates("claude-opus-4-6", 5.0, 25.0, 0.50),
    rates("claude-sonnet-5-5", 2.0, 10.0, 0.20),
    rates("claude-sonnet-5", 2.0, 10.0, 0.20),
    rates("claude-sonnet-4-6", 3.0, 15.0, 0.30),
    rates("claude-haiku-5-5", 0.10, 0.50, 0.01),
    rates("claude-haiku-4-5", 1.0, 5.0, 0.10),
];

const fn rates(id: &'static str, input: f64, output: f64, cache_read: f64) -> ModelRates {
    ModelRates {
        id,
        input,
        output,
        cache_read,
    }
}

/// Each model id whose context window is known, with that window in tokens.
const CONTEXT_WINDOWS: [(&str, u64); 1] = [("claude-opus-5-5", 1_000_000)];

/// One message's token usage, as its `message.usage` gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_creation: u64,
    /// The cache writes split into (5-minute, 1-hour) tokens, when the usage
    /// carries the split.
    pub(crate) cache_split: Option<(u64, u64)>,
}

impl Usage {
    /// The tokens the model read: input, cache writes and cache reads.
    pub(crate) fn tokens_in(&self) -> u64 {
        self.input
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
    }

    /// Each field's larger value of `self` and `other`.
    pub(crate) fn largest(self, other: Self) -> Self {
        let cache_split = match (self.cache_split, other.cache_split) {
            (Some(mine), Some(theirs)) => Some((mine.0.max(theirs.0), mine.1.max(theirs.1))),
            (mine, theirs) => mine.or(theirs),
        };
        Self {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_read: self.cache_read.max(other.cache_read),
            cache_creation: self.cache_creation.max(other.cache_creation),
            cache_split,
        }
    }
}

/// What a message of `model` with `usage` costs, in US dollars at its
/// model's list prices; `None` for a model [`rates_of`] does not price. A
/// usage without the cache writes' split counts all of them at the 5-minute
/// rate.
#[expect(
    clippy::cast_precision_loss,
    reason = "token counts stay far below 2^52, where f64 starts to lose whole numbers"
)]
pub(crate) fn cost_usd(model: &str, usage: &Usage) -> Option<f64> {
    let rates = rates_of(model)?;
    let tokens = |count: u64| count as f64;
    let (five_minute, one_hour) = usage.cache_split.unwrap_or((usage.cache_creation, 0));
    let per_million = tokens(usage.input) * rates.input
        + tokens(usage.output) * rates.output
        + tokens(five_minute) * CACHE_WRITE_5M * rates.input
        + tokens(one_hour) * CACHE_WRITE_1H * rates.input
        + tokens(usage.cache_read) * rates.cache_read;
    Some(per_million / 1_000_000.0)
}

/// The context window of `model`, in tokens; `None` for a model the table
/// does not know.
pub(crate) fn context_window(model: &str) -> Option<u64> {
    CONTEXT_WINDOWS
        .iter()
        .find(|(known, _)| *known == model)
        .map(|(_, window)| *window)
}

/// The percent of a `window`-token context window that `tokens` fill, as a
/// whole percent rounded half up and capped at 100; a window of 0 tokens
/// reads 100.
pub(crate) fn context_percent(tokens: u64, window: u64) -> u8 {
    if window == 0 {
        return 100;
    }
    let percent = tokens.saturating_mul(100).saturating_add(window / 2) / window;
    u8::try_from(percent.min(100)).unwrap_or(100)
}

/// The list prices of `model`: the longest [`MODEL_RATES`] id that names it
/// ([`names`]); else, for a `claude-` model one of whose `-` segments is a
/// family in [`FAMILIES`], that family's first row; else `None` (a `DeepSeek`
/// model, `<synthetic>`).
fn rates_of(model: &str) -> Option<ModelRates> {
    let named = MODEL_RATES
        .iter()
        .filter(|rates| names(model, rates.id))
        .max_by_key(|rates| rates.id.len());
    if let Some(rates) = named {
        return Some(*rates);
    }
    let family = model
        .strip_prefix("claude-")?
        .split(['-', '[', '@'])
        .find(|segment| FAMILIES.contains(segment))?;
    MODEL_RATES
        .iter()
        .find(|rates| rates.id.split('-').nth(1) == Some(family))
        .copied()
}

/// Whether `id` names `model`: `model` is `id`, or `id` followed by a `[` (a
/// context-window suffix such as `[1m]`), an `@` or a `-` and an 8-digit
/// date; so `claude-opus-5` names neither `claude-opus-5-5` nor
/// `claude-opus-5-1`.
fn names(model: &str, id: &str) -> bool {
    let Some(rest) = model.strip_prefix(id) else {
        return false;
    };
    if rest.is_empty() || rest.starts_with(['[', '@']) {
        return true;
    }
    rest.strip_prefix('-').is_some_and(|after| {
        let date = after.split(['-', '[', '@']).next().unwrap_or_default();
        date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::{Usage, context_percent, context_window, cost_usd};

    /// A million tokens of each kind, one kind at a time, so a cost reads as
    /// the row's price.
    fn million(field: &str) -> Usage {
        let mut usage = Usage::default();
        match field {
            "input" => usage.input = 1_000_000,
            "output" => usage.output = 1_000_000,
            _ => usage.cache_read = 1_000_000,
        }
        usage
    }

    #[test]
    fn a_known_model_is_priced_at_its_row_and_has_its_window() {
        assert_eq!(cost_usd("claude-opus-5-5", &million("input")), Some(4.0));
        assert_eq!(cost_usd("claude-opus-5-5", &million("output")), Some(20.0));
        assert_eq!(
            cost_usd("claude-opus-5-5-20261001", &million("input")),
            Some(4.0)
        );
        assert_eq!(
            cost_usd("claude-opus-5-5[1m]", &million("input")),
            Some(4.0)
        );
        assert_eq!(cost_usd("claude-opus-5", &million("input")), Some(5.0));
        assert_eq!(cost_usd("claude-haiku-4-5", &million("cache")), Some(0.10));

        assert_eq!(context_window("claude-opus-5-5"), Some(1_000_000));
        assert_eq!(context_percent(404_020, 1_000_000), 40);
        assert_eq!(context_percent(5_000, 1_000_000), 1);
        assert_eq!(context_percent(4_999, 1_000_000), 0);
        assert_eq!(context_percent(2_000_000, 1_000_000), 100);
        assert_eq!(context_percent(1, 0), 100);
    }

    #[test]
    fn cache_writes_cost_by_their_lifetime() {
        let unsplit = Usage {
            cache_creation: 1_000_000,
            ..Usage::default()
        };
        let split = Usage {
            cache_creation: 1_000_000,
            cache_split: Some((0, 1_000_000)),
            ..Usage::default()
        };

        assert_eq!(cost_usd("claude-opus-5-5", &unsplit), Some(5.0));
        assert_eq!(cost_usd("claude-opus-5-5", &split), Some(8.0));
    }

    #[test]
    fn an_unnamed_claude_model_falls_back_to_its_family_for_cost_only() {
        assert_eq!(cost_usd("claude-sonnet-9", &million("input")), Some(2.0));
        assert_eq!(cost_usd("claude-opus-9-9", &million("input")), Some(4.0));
        assert_eq!(context_window("claude-sonnet-9"), None);
        assert_eq!(context_window("claude-sonnet-5-5"), None);
    }

    #[test]
    fn an_unknown_model_gets_no_cost_and_no_window() {
        for model in [
            "deepseek-chat",
            "<synthetic>",
            "gpt-5",
            "claude-unknown-1",
            "",
        ] {
            assert_eq!(cost_usd(model, &million("input")), None, "{model:?}");
            assert_eq!(context_window(model), None, "{model:?}");
        }
    }
}
