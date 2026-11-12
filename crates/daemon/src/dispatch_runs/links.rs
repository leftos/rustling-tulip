//! The conversations that live dispatch runs name: a session whose
//! conversation id is linked counts as running background work.

use crate::sync::lock;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The conversation ids at least one live dispatch run names, each with the
/// number of runs naming it. Clones share one set. Its lock is a leaf:
/// nothing else is locked or called while it is held.
#[derive(Clone, Debug, Default)]
pub(crate) struct DispatchLinks {
    counts: Arc<Mutex<HashMap<String, usize>>>,
}

impl DispatchLinks {
    /// Count one more live run naming `conversation_id`; true when the id
    /// was not linked before.
    pub(crate) fn link(&self, conversation_id: &str) -> bool {
        let mut counts = lock(&self.counts);
        let count = counts.entry(conversation_id.to_owned()).or_insert(0);
        *count += 1;
        *count == 1
    }

    /// Count one fewer live run naming `conversation_id`; true when that was
    /// its last. An id that is not linked is left alone and answers false.
    pub(crate) fn unlink(&self, conversation_id: &str) -> bool {
        let mut counts = lock(&self.counts);
        let Some(count) = counts.get_mut(conversation_id) else {
            return false;
        };
        *count -= 1;
        if *count > 0 {
            return false;
        }
        counts.remove(conversation_id);
        true
    }

    /// Whether a live run names `conversation_id`.
    pub(crate) fn is_linked(&self, conversation_id: &str) -> bool {
        lock(&self.counts).contains_key(conversation_id)
    }
}

#[cfg(test)]
mod tests {
    use super::DispatchLinks;

    #[test]
    fn dispatch_link_counts_runs_per_conversation() {
        let links = DispatchLinks::default();
        assert!(links.link("c1"), "the first run links the conversation");
        assert!(!links.link("c1"), "a second run adds to the count");
        assert!(!links.unlink("c1"), "one run is still live");
        assert!(links.is_linked("c1"));
        assert!(links.unlink("c1"), "the last run unlinks it");
        assert!(!links.is_linked("c1"));
        assert!(!links.unlink("c1"), "an unknown id is a no-op");
        assert!(!links.unlink("other"));
    }
}
