//! The daemon's side of an agent's recorded conversations: the rows a lookup
//! reads with, and the questions it asks the agent's plugin. Every id a
//! plugin answers must be a plain id, since it later becomes a CLI argument
//! and a path.

#[cfg(windows)]
mod job;
pub(crate) mod pre_create;

use std::path::Path;

use agent_api::{AgentPlugin, ConversationQuery, FoundConversation, ResolvedRows, is_plain_id};
use chrono::{DateTime, Utc};
use protocol::env_rows::{Reference, reference};
use tracing::{debug, warn};

use crate::user_env::{self, Origin, Secret};

/// The rows a conversation lookup reads with, from a session's stored
/// `extra_env`: literals as they are and each `${env:}` reference resolved,
/// skipping one that is unset. A `${secret:}` row is skipped, so a lookup
/// never opens Credential Manager.
pub(crate) fn lookup_rows(extra_env: &[(String, String)]) -> ResolvedRows {
    lookup_rows_with(extra_env, user_env::resolve)
}

fn lookup_rows_with(
    extra_env: &[(String, String)],
    lookup: impl Fn(&str) -> Option<(Secret, Origin)>,
) -> ResolvedRows {
    let rows = extra_env
        .iter()
        .filter_map(|(key, value)| match reference(value) {
            None => Some((key.clone(), value.clone())),
            Some(Reference::Env(name)) => {
                let resolved = lookup(&name).map(|(secret, _)| secret.expose().to_owned());
                if resolved.is_none() {
                    debug!(key = %key, name = %name, "conversation lookup: env reference unset; row skipped");
                }
                resolved.map(|value| (key.clone(), value))
            }
            Some(Reference::Secret(_)) => None,
        })
        .collect();
    ResolvedRows::new(rows)
}

/// The conversation `id` that `plugin` still holds for `folder`, with its
/// last activity and title.
pub(crate) fn known(
    plugin: &dyn AgentPlugin,
    folder: &Path,
    id: &str,
    rows: &ResolvedRows,
) -> Option<FoundConversation> {
    let query = ConversationQuery {
        folder,
        window: None,
        known_id: Some(id),
        rows,
    };
    plain_ids(plugin, plugin.find_conversations(&query))
        .into_iter()
        .find(|found| found.id == id)
}

/// Whether `plugin` still holds conversation `id` for `folder`.
pub(crate) fn exists(
    plugin: &dyn AgentPlugin,
    folder: &Path,
    id: &str,
    rows: &ResolvedRows,
) -> bool {
    known(plugin, folder, id, rows).is_some()
}

/// The conversations `plugin` recorded for `folder` that were active inside
/// `window`, newest first.
pub(crate) fn candidates(
    plugin: &dyn AgentPlugin,
    folder: &Path,
    window: (DateTime<Utc>, DateTime<Utc>),
    rows: &ResolvedRows,
) -> Vec<FoundConversation> {
    let query = ConversationQuery {
        folder,
        window: Some(window),
        known_id: None,
        rows,
    };
    plain_ids(plugin, plugin.find_conversations(&query))
}

/// `found` without the conversations whose id is not a plain id, each logged.
fn plain_ids(plugin: &dyn AgentPlugin, found: Vec<FoundConversation>) -> Vec<FoundConversation> {
    found
        .into_iter()
        .filter(|conversation| {
            let plain = is_plain_id(&conversation.id);
            if !plain {
                warn!(
                    agent = %plugin.descriptor().id.0,
                    "conversation lookup: dropped a conversation whose id is not a plain id"
                );
            }
            plain
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Origin, Secret, lookup_rows_with};

    #[test]
    fn lookup_rows_keeps_literals_resolves_env_and_skips_secrets() {
        let stored = [
            ("LITERAL", "plain value"),
            ("FROM_ENV", "${env:RT_LOOKUP_SET}"),
            ("UNSET", "${env:RT_LOOKUP_UNSET}"),
            ("SEALED", "${secret:0123456789abcdef0123456789abcdef}"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));

        let rows = lookup_rows_with(&stored, |name| {
            (name == "RT_LOOKUP_SET")
                .then(|| (Secret::new("resolved".to_owned()), Origin::UserScope))
        });

        let rows: Vec<(&str, &str)> = rows.iter().collect();
        assert_eq!(rows, [("LITERAL", "plain value"), ("FROM_ENV", "resolved")]);
    }
}
