//! The summarizer's result cache: a model's summary kept by the provider, the
//! trigger and the SHA-256 of the shaped input, so an unchanged text costs
//! nothing. In memory only; the oldest entry goes first past [`CAPACITY`].

use std::collections::VecDeque;

use sha2::{Digest as _, Sha256};

use crate::summarizer::prompt::Trigger;
use crate::summarizer::reply::Summary;

/// The most entries the cache holds.
pub(crate) const CAPACITY: usize = 64;

/// What a summary is cached by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    provider: String,
    trigger: Trigger,
    digest: [u8; 32],
}

impl CacheKey {
    /// The key for `input`, the shaped text, sent to `provider` for `trigger`.
    pub(crate) fn new(provider: &str, trigger: Trigger, input: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            trigger,
            digest: Sha256::digest(input.as_bytes()).into(),
        }
    }
}

/// The cached summaries, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    entries: VecDeque<(CacheKey, Summary)>,
}

impl Cache {
    /// The summary cached under `key`.
    pub(crate) fn get(&self, key: &CacheKey) -> Option<&Summary> {
        self.entries
            .iter()
            .find(|(cached, _)| cached == key)
            .map(|(_, summary)| summary)
    }

    /// Cache `summary` under `key`, replacing an entry with that key in its
    /// place, else adding it as the newest and dropping the oldest past
    /// [`CAPACITY`].
    pub(crate) fn insert(&mut self, key: CacheKey, summary: Summary) {
        if let Some(entry) = self.entries.iter_mut().find(|(cached, _)| *cached == key) {
            entry.1 = summary;
            return;
        }
        self.entries.push_back((key, summary));
        while self.entries.len() > CAPACITY {
            self.entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::AlertKind;

    fn summary(text: &str) -> Summary {
        Summary {
            kind: AlertKind::DoneWaiting,
            headline: text.to_owned(),
            notification: text.to_owned(),
        }
    }

    #[test]
    fn an_unchanged_text_for_the_same_trigger_and_provider_is_served_from_the_cache() {
        let mut cache = Cache::default();
        cache.insert(
            CacheKey::new("provider-a", Trigger::Working, "The tests pass."),
            summary("cached"),
        );
        assert_eq!(
            cache.get(&CacheKey::new(
                "provider-a",
                Trigger::Working,
                "The tests pass."
            )),
            Some(&summary("cached"))
        );
        for miss in [
            CacheKey::new("provider-a", Trigger::Stop, "The tests pass."),
            CacheKey::new("provider-b", Trigger::Working, "The tests pass."),
            CacheKey::new("provider-a", Trigger::Working, "The tests fail."),
        ] {
            assert_eq!(cache.get(&miss), None, "{miss:?}");
        }
    }

    #[test]
    fn the_cache_evicts_its_oldest_entry_past_64() {
        let mut cache = Cache::default();
        let key = |n: usize| CacheKey::new("provider-a", Trigger::Stop, &format!("text {n}"));
        for n in 0..CAPACITY {
            cache.insert(key(n), summary(&n.to_string()));
        }
        assert!(cache.get(&key(0)).is_some(), "64 entries fit");
        cache.insert(key(CAPACITY), summary("newest"));
        assert_eq!(cache.get(&key(0)), None, "the oldest entry went");
        assert!(cache.get(&key(1)).is_some());
        assert_eq!(cache.get(&key(CAPACITY)), Some(&summary("newest")));
    }
}
