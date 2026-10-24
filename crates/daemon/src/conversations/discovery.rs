//! Finding the conversation of a session whose plugin plans
//! `DiscoverAfterLaunch`: the spawn registers a waiter for the session,
//! a pass asks the plugin which conversations it recorded for the waiter's
//! folder since launch, and arbitration gives each new one to the one
//! eligible waiter, or marks it contested when two or more are eligible.
//!
//! The [`Discovery`] lock is never held across a registry lock, a record
//! lock, file I/O or an await: a pass copies the waiters it needs first.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::{AgentPlugin, FoundConversation, ResolvedRows};
use chrono::{DateTime, Utc};
use protocol::{HistoryEntry, SessionStatus};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use super::contested;
use crate::history::{self, HISTORY_RETENTION};
use crate::paths::{Dirs, normalize_path_key};
use crate::session::SessionRegistry;
use crate::sync::lock;

/// What a `DiscoverAfterLaunch` spawn knows before its program starts.
pub(crate) struct Baseline {
    /// The conversations the plugin already held for the folder.
    pub(crate) ids: HashSet<String>,
    /// The session's plugin, which every later lookup asks.
    pub(crate) plugin: &'static dyn AgentPlugin,
    /// The rows lookups read with.
    pub(crate) rows: ResolvedRows,
    /// The session's primary folder.
    pub(crate) folder: PathBuf,
    /// When the baseline was taken, just before the program started: the
    /// session's launch as discovery counts it.
    pub(crate) launched_at: DateTime<Utc>,
}

/// A session waiting for its conversation id.
#[derive(Clone)]
pub(crate) struct Waiter {
    session_id: String,
    agent: String,
    folder: PathBuf,
    folder_key: String,
    baseline: HashSet<String>,
    launched_at: DateTime<Utc>,
    plugin: &'static dyn AgentPlugin,
    rows: ResolvedRows,
}

impl Waiter {
    /// The waiter of `session_id`, launched when `baseline` was taken.
    pub(crate) fn new(session_id: String, baseline: Baseline) -> Self {
        Self {
            session_id,
            agent: baseline.plugin.descriptor().id.0.clone(),
            folder_key: normalize_path_key(&baseline.folder.to_string_lossy()),
            folder: baseline.folder,
            baseline: baseline.ids,
            launched_at: baseline.launched_at,
            plugin: baseline.plugin,
            rows: baseline.rows,
        }
    }

    /// The conversations the plugin held for the folder before launch.
    #[cfg(test)]
    pub(crate) fn baseline(&self) -> &HashSet<String> {
        &self.baseline
    }

    /// Whether `id` is new to this waiter: outside its baseline and last
    /// active at or after its launch.
    fn may_claim(&self, conversation: &FoundConversation) -> bool {
        !self.baseline.contains(&conversation.id) && conversation.last_active >= self.launched_at
    }
}

/// The sessions waiting for their conversation ids, keyed by session id.
#[derive(Default)]
pub(crate) struct Discovery {
    waiters: Mutex<HashMap<String, Waiter>>,
}

impl Discovery {
    /// Start waiting for `waiter`'s conversation.
    pub(crate) fn register(&self, waiter: Waiter) {
        lock(&self.waiters).insert(waiter.session_id.clone(), waiter);
    }

    /// Stop waiting for `session_id`'s conversation, answering its waiter.
    pub(crate) fn deregister(&self, session_id: &str) -> Option<Waiter> {
        lock(&self.waiters).remove(session_id)
    }

    /// A copy of `session_id`'s waiter, while it waits.
    pub(crate) fn waiter(&self, session_id: &str) -> Option<Waiter> {
        lock(&self.waiters).get(session_id).cloned()
    }

    fn all(&self) -> Vec<Waiter> {
        lock(&self.waiters).values().cloned().collect()
    }
}

/// The ids no waiter may claim.
#[derive(Debug, Default)]
pub(crate) struct Excluded {
    /// Ids a registry record holds, whatever its status.
    pub(crate) held: HashSet<String>,
    /// Ids of history entries that ended within [`HISTORY_RETENTION`].
    pub(crate) recent: HashSet<String>,
    /// Ids in the agent's contested store.
    pub(crate) contested: HashSet<String>,
}

impl Excluded {
    fn contains(&self, id: &str) -> bool {
        self.held.contains(id) || self.recent.contains(id) || self.contested.contains(id)
    }
}

/// What arbitration decided for one conversation.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Outcome {
    /// The one eligible waiter, `session_id`, gets `id`.
    Claim { session_id: String, id: String },
    /// Two or more waiters were eligible for this id; nobody gets it.
    Contest(String),
}

/// Decide who gets each conversation in `found`, which `scope`'s plugin
/// found for `scope`'s folder. A waiter in `waiters` (each one without an id
/// yet) is eligible for a conversation when its agent and folder key are
/// `scope`'s and the conversation is new to it; an id in `excluded` goes to
/// nobody. Eligibility is settled for every id before any claim: an id with
/// two or more eligible waiters is contested, and a waiter that alone is
/// eligible for several ids claims only the newest (latest `last_active`,
/// then the greater id), leaving the rest to nobody this time. The outcomes
/// come sorted, so they do not depend on the order of `found`.
pub(crate) fn arbitrate(
    scope: &Waiter,
    found: &[FoundConversation],
    waiters: &[Waiter],
    excluded: &Excluded,
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    let mut sole: HashMap<&str, &FoundConversation> = HashMap::new();
    for conversation in found {
        if excluded.contains(&conversation.id) {
            continue;
        }
        let eligible: Vec<&Waiter> = waiters
            .iter()
            .filter(|waiter| {
                waiter.agent == scope.agent
                    && waiter.folder_key == scope.folder_key
                    && waiter.may_claim(conversation)
            })
            .collect();
        match eligible.as_slice() {
            [] => {}
            [only] => {
                let best = sole.entry(only.session_id.as_str()).or_insert(conversation);
                if newer(conversation, best) {
                    *best = conversation;
                }
            }
            _ => outcomes.push(Outcome::Contest(conversation.id.clone())),
        }
    }
    outcomes.extend(
        sole.into_iter()
            .map(|(session_id, conversation)| Outcome::Claim {
                session_id: session_id.to_owned(),
                id: conversation.id.clone(),
            }),
    );
    outcomes.sort();
    outcomes.dedup();
    outcomes
}

/// Whether `a` was active after `b`, the greater id breaking a tie.
fn newer(a: &FoundConversation, b: &FoundConversation) -> bool {
    (a.last_active, &a.id) > (b.last_active, &b.id)
}

/// The conversation ids of the entries in `entries` that ended within
/// [`HISTORY_RETENTION`] of `now`.
pub(crate) fn recent_history_ids(entries: &[HistoryEntry], now: DateTime<Utc>) -> HashSet<String> {
    entries
        .iter()
        .filter(|entry| now - entry.ended_at <= HISTORY_RETENTION)
        .filter_map(|entry| entry.conversation_id.clone())
        .collect()
}

/// Give `id` to `session_id`'s record, taking its waiter out first: a claim
/// for a session that already stopped waiting (its final look ran, or its
/// poller stopped) is dropped, unless `even_if_deregistered` (the final
/// look's claim for its own session). A record that gained an id meanwhile
/// (from a hook) keeps it. Answers whether `id` was written.
pub(crate) fn claim(
    registry: &SessionRegistry,
    session_id: &str,
    id: &str,
    even_if_deregistered: bool,
) -> bool {
    let waiting = registry.discovery().deregister(session_id).is_some();
    if !waiting && !even_if_deregistered {
        info!(%session_id, "discovery: the session stopped waiting; claim dropped");
        return false;
    }
    let mut claimed = false;
    registry.update_if_changed(session_id, |rec| {
        if rec.conversation_id.is_some() {
            return false;
        }
        rec.conversation_id = Some(id.to_owned());
        claimed = true;
        true
    });
    if claimed {
        info!(%session_id, id_len = id.len(), "discovery: claimed the session's conversation");
    } else {
        info!(%session_id, "discovery: the session already had a conversation; claim dropped");
    }
    claimed
}

/// Whether `session_id`'s record exists and holds no conversation id.
fn has_no_id(registry: &SessionRegistry, session_id: &str) -> bool {
    registry
        .get(session_id)
        .is_some_and(|rec| lock(&rec).conversation_id.is_none())
}

/// Who runs a [`pass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassBy {
    /// The session's poller: a claim needs every claimant still waiting.
    Poller,
    /// The session's final look: its own session takes part, and claims,
    /// even when something deregistered it during the pass.
    FinalLook,
}

/// One lookup for `waiter` at `now`: ask its plugin for the conversations
/// active in its folder since launch, arbitrate them across the registered
/// waiters, and write the claims and contests. Blocking: it runs the
/// plugin's lookup and reads and writes files.
pub(crate) fn pass(
    registry: &SessionRegistry,
    dirs: &Dirs,
    waiter: &Waiter,
    now: DateTime<Utc>,
    by: PassBy,
) {
    let found = super::candidates(
        waiter.plugin,
        &waiter.folder,
        (waiter.launched_at, now),
        &waiter.rows,
    );
    if found.is_empty() {
        return;
    }
    let excluded = Excluded {
        held: registry
            .snapshots()
            .into_iter()
            .filter_map(|snap| snap.conversation_id)
            .collect(),
        recent: recent_history_ids(&history::read_all(dirs), now),
        contested: contested::read(dirs, &waiter.agent, now),
    };
    let mut waiters = registry.discovery().all();
    let final_look = by == PassBy::FinalLook;
    if final_look && !waiters.iter().any(|w| w.session_id == waiter.session_id) {
        waiters.push(waiter.clone());
    }
    waiters.retain(|w| has_no_id(registry, &w.session_id));
    for outcome in arbitrate(waiter, &found, &waiters, &excluded) {
        match outcome {
            Outcome::Claim { session_id, id } => {
                let own = final_look && session_id == waiter.session_id;
                claim(registry, &session_id, &id, own);
            }
            Outcome::Contest(id) => {
                info!(agent = %waiter.agent, id_len = id.len(), "discovery: conversation contested; nobody claims it");
                if let Err(err) = contested::add(dirs, &waiter.agent, &id, now) {
                    warn!(?err, agent = %waiter.agent, "discovery: could not store a contested id");
                }
            }
        }
    }
}

/// The last lookup for `session_id` as its session ends: while its record
/// still has no id one more [`pass`] runs with its waiter still registered
/// (so its siblings still count it eligible), then the waiter is
/// deregistered. A conversation the session created since the last poll so
/// reaches its record, and the history entry built from it. A panic in the
/// pass is logged and the session ends with no discovered id. Blocking, and
/// run inline by [`history::record_session_end`], beside its own file I/O.
pub(crate) fn final_look(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    now: DateTime<Utc>,
) {
    let Some(waiter) = registry.discovery().waiter(session_id) else {
        return;
    };
    if has_no_id(registry, session_id) {
        debug!(%session_id, "discovery: final look");
        let looked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            pass(registry, dirs, &waiter, now, PassBy::FinalLook);
        }));
        if looked.is_err() {
            error!(
                %session_id,
                agent = %waiter.agent,
                "discovery: the final look panicked; the session ends with no discovered conversation"
            );
        }
    }
    registry.discovery().deregister(session_id);
}

/// How long a poller waits before its next lookup, `elapsed` after it
/// started: every 2 s for 5 minutes, every 10 s up to an hour, then every
/// minute for as long as the session runs.
pub(crate) fn poll_interval(elapsed: Duration) -> Duration {
    const FAST_FOR: Duration = Duration::from_mins(5);
    const MEDIUM_FOR: Duration = Duration::from_hours(1);
    if elapsed < FAST_FOR {
        Duration::from_secs(2)
    } else if elapsed < MEDIUM_FOR {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(60)
    }
}

/// Why a poller stops before its next lookup, if it does: its record is
/// gone, has ended, or holds an id from any source.
fn stop_reason(registry: &SessionRegistry, session_id: &str) -> Option<&'static str> {
    let Some(rec) = registry.get(session_id) else {
        return Some("the session was removed");
    };
    let rec = lock(&rec);
    if matches!(rec.status, SessionStatus::Stopped | SessionStatus::Error) {
        Some("the session ended")
    } else if rec.conversation_id.is_some() {
        Some("the session has a conversation id")
    } else {
        None
    }
}

/// Start waiting for `waiter`'s conversation. A spawn registers it before
/// the session's exit watcher starts, so an exit at once still gets its
/// final look.
pub(crate) fn register(registry: &SessionRegistry, waiter: Waiter) {
    info!(
        session_id = %waiter.session_id,
        agent = %waiter.agent,
        baseline = waiter.baseline.len(),
        "discovery: waiting for the session's conversation"
    );
    registry.discovery().register(waiter);
}

/// Start the poller of the registered waiter `session_id`, which runs a
/// [`pass`] on each tick of [`poll_interval`] until the waiter is
/// deregistered (by a claim or the final look) or [`stop_reason`] gives a
/// reason.
pub(crate) fn start_poller(
    registry: &Arc<SessionRegistry>,
    dirs: Dirs,
    session_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(poll(Arc::clone(registry), dirs, session_id))
}

/// [`register`] `waiter`, then [`start_poller`].
#[cfg(test)]
pub(crate) fn start(
    registry: &Arc<SessionRegistry>,
    dirs: Dirs,
    waiter: Waiter,
) -> tokio::task::JoinHandle<()> {
    let session_id = waiter.session_id.clone();
    register(registry, waiter);
    start_poller(registry, dirs, session_id)
}

async fn poll(registry: Arc<SessionRegistry>, dirs: Dirs, session_id: String) {
    let started = Instant::now();
    loop {
        tokio::time::sleep(poll_interval(started.elapsed())).await;
        let Some(waiter) = registry.discovery().waiter(&session_id) else {
            debug!(%session_id, "discovery: poller ends; the waiter is gone");
            return;
        };
        if let Some(reason) = stop_reason(&registry, &session_id) {
            registry.discovery().deregister(&session_id);
            debug!(%session_id, reason, "discovery: poller ends");
            return;
        }
        let (pass_registry, pass_dirs) = (Arc::clone(&registry), dirs.clone());
        let lookup = tokio::task::spawn_blocking(move || {
            pass(
                &pass_registry,
                &pass_dirs,
                &waiter,
                Utc::now(),
                PassBy::Poller,
            );
        });
        if let Err(err) = lookup.await {
            warn!(?err, %session_id, "discovery: a lookup failed; trying again on the next tick");
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        Baseline, Excluded, Outcome, PassBy, Waiter, arbitrate, claim, pass, poll_interval,
        recent_history_ids, start,
    };
    use crate::conversations::contested;
    use crate::history::{self, test_support};
    use crate::session::SessionRegistry;
    use crate::sync::lock;
    use agent_api::test_agent::TestAgent;
    use agent_api::{AgentPlugin, ConversationPlan, FoundConversation, ResolvedRows};
    use chrono::{DateTime, TimeDelta, Utc};
    use protocol::{SessionEnd, SessionMode, SessionStatus};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    /// A test agent that lives as long as the test binary, as a waiter needs.
    fn leaked_agent() -> &'static TestAgent {
        Box::leak(Box::new(TestAgent::new(
            ConversationPlan::DiscoverAfterLaunch,
        )))
    }

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    fn waiter(
        agent: &'static TestAgent,
        session_id: &str,
        folder: &str,
        baseline: &[&str],
        launched_at: DateTime<Utc>,
    ) -> Waiter {
        let plugin: &'static dyn AgentPlugin = agent;
        Waiter::new(
            session_id.to_owned(),
            Baseline {
                ids: ids(baseline),
                plugin,
                rows: ResolvedRows::default(),
                folder: PathBuf::from(folder),
                launched_at,
            },
        )
    }

    #[test]
    fn arbitration_does_not_depend_on_found_order() {
        let agent = leaked_agent();
        let s1 = waiter(agent, "s1", r"D:\order", &[], launch());
        let s2 = waiter(agent, "s2", r"D:\order", &["c-x"], launch());
        let x = found("c-x", Utc::now());
        let y = found("c-y", Utc::now() - TimeDelta::seconds(5));
        let expected = [claimed("s1", "c-x"), Outcome::Contest("c-y".to_owned())];
        for order in [[x.clone(), y.clone()], [y, x]] {
            let outcomes = arbitrate(&s1, &order, &[s1.clone(), s2.clone()], &Excluded::default());
            assert_eq!(outcomes, expected, "found in order {order:?}");
        }
    }

    #[test]
    fn sole_waiter_for_two_ids_claims_the_newest() {
        let s1 = waiter(leaked_agent(), "s1", r"D:\newest", &[], launch());
        let older = found("c-older", Utc::now() - TimeDelta::seconds(5));
        let newer = found("c-newer", Utc::now());
        for order in [[older.clone(), newer.clone()], [newer, older]] {
            let outcomes = arbitrate(&s1, &order, std::slice::from_ref(&s1), &Excluded::default());
            assert_eq!(
                outcomes,
                [claimed("s1", "c-newer")],
                "found in order {order:?}"
            );
        }
    }

    #[test]
    fn final_look_survives_a_panicking_lookup() {
        let (registry, dirs) = registry_with("discovery-final-panic", &["s1"]);
        let agent = leaked_agent();
        agent.set_panics(true);
        registry
            .discovery()
            .register(waiter(agent, "s1", r"D:\final-panic", &[], launch()));

        history::record_session_end(&registry, &dirs, "s1", SessionEnd::Exited { code: 0 });

        let entry = history::read_one(&dirs, "s1").expect("the history entry");
        assert_eq!(entry.conversation_id, None);
        assert!(registry.discovery().waiter("s1").is_none(), "deregistered");
    }

    #[test]
    fn final_look_counts_its_siblings_and_contests() {
        let (registry, dirs) = registry_with("discovery-final-sibling", &["a", "b"]);
        let agent = leaked_agent();
        let folder = r"D:\final-sibling";
        registry
            .discovery()
            .register(waiter(agent, "a", folder, &[], launch()));
        registry
            .discovery()
            .register(waiter(agent, "b", folder, &[], launch()));
        agent.set_found(folder, vec![found("c-x", Utc::now())]);

        history::record_session_end(&registry, &dirs, "a", SessionEnd::Exited { code: 0 });

        assert_eq!(conversation_of(&registry, "a"), None);
        assert_eq!(conversation_of(&registry, "b"), None, "b does not claim it");
        assert_eq!(
            contested::read(&dirs, "test-agent", Utc::now()),
            ids(&["c-x"])
        );
        assert!(registry.discovery().waiter("a").is_none());
        assert!(registry.discovery().waiter("b").is_some());
    }

    #[test]
    fn a_claim_after_deregistration_is_dropped() {
        let (registry, dirs) = registry_with("discovery-claim-late", &["s1"]);
        let agent = leaked_agent();
        let folder = r"D:\claim-late";
        agent.set_found(folder, vec![found("c-x", Utc::now())]);
        registry
            .discovery()
            .register(waiter(agent, "s1", folder, &[], launch()));
        let copy = registry.discovery().waiter("s1").expect("the waiter");
        registry.discovery().deregister("s1");

        pass(&registry, &dirs, &copy, Utc::now(), PassBy::Poller);

        assert_eq!(conversation_of(&registry, "s1"), None);
    }

    fn found(id: &str, last_active: DateTime<Utc>) -> FoundConversation {
        FoundConversation {
            id: id.to_owned(),
            last_active,
            title: None,
        }
    }

    fn claimed(session_id: &str, id: &str) -> Outcome {
        Outcome::Claim {
            session_id: session_id.to_owned(),
            id: id.to_owned(),
        }
    }

    fn launch() -> DateTime<Utc> {
        Utc::now() - TimeDelta::minutes(1)
    }

    #[test]
    fn sole_waiter_claims_a_new_conversation() {
        let s1 = waiter(leaked_agent(), "s1", r"D:\proj", &[], launch());
        let outcomes = arbitrate(
            &s1,
            &[found("c-new", Utc::now())],
            std::slice::from_ref(&s1),
            &Excluded::default(),
        );
        assert_eq!(outcomes, [claimed("s1", "c-new")]);
    }

    #[test]
    fn baseline_ids_are_never_claimed() {
        let s1 = waiter(leaked_agent(), "s1", r"D:\proj", &["c-old"], launch());
        let outcomes = arbitrate(
            &s1,
            &[found("c-old", Utc::now())],
            std::slice::from_ref(&s1),
            &Excluded::default(),
        );
        assert_eq!(outcomes, []);
    }

    #[test]
    fn conversation_last_active_before_launch_is_not_claimed() {
        let launched_at = launch();
        let s1 = waiter(leaked_agent(), "s1", r"D:\proj", &[], launched_at);
        let before = arbitrate(
            &s1,
            &[found("c-1", launched_at - TimeDelta::seconds(1))],
            std::slice::from_ref(&s1),
            &Excluded::default(),
        );
        assert_eq!(before, []);
        let at = arbitrate(
            &s1,
            &[found("c-1", launched_at)],
            std::slice::from_ref(&s1),
            &Excluded::default(),
        );
        assert_eq!(
            at,
            [claimed("s1", "c-1")],
            "active exactly at launch counts"
        );
    }

    #[test]
    fn other_agent_or_folder_is_not_eligible() {
        let agent = leaked_agent();
        let s1 = waiter(agent, "s1", r"D:\Proj\", &[], launch());
        let other_folder = waiter(agent, "s2", r"D:\elsewhere", &[], launch());
        let mut other_agent = waiter(agent, "s3", r"D:\proj", &[], launch());
        other_agent.agent = "other-agent".to_owned();
        let outcomes = arbitrate(
            &s1,
            &[found("c-new", Utc::now())],
            &[s1.clone(), other_folder, other_agent],
            &Excluded::default(),
        );
        assert_eq!(outcomes, [claimed("s1", "c-new")]);
    }

    #[test]
    fn two_eligible_waiters_contest_and_neither_claims() {
        let agent = leaked_agent();
        let s1 = waiter(agent, "s1", r"D:\proj", &[], launch());
        let s2 = waiter(agent, "s2", r"d:/proj", &[], launch());
        let outcomes = arbitrate(
            &s1,
            &[found("c-new", Utc::now())],
            &[s1.clone(), s2],
            &Excluded::default(),
        );
        if cfg!(windows) {
            assert_eq!(outcomes, [Outcome::Contest("c-new".to_owned())]);
        } else {
            assert_eq!(outcomes, [claimed("s1", "c-new")], "the folders differ");
        }
    }

    /// A registry holding an interactive record for each of `sessions`, and
    /// its config dir.
    fn registry_with(tag: &str, sessions: &[&str]) -> (Arc<SessionRegistry>, crate::paths::Dirs) {
        let dirs = test_support::scratch_dirs(tag);
        let registry = SessionRegistry::new(dirs.clone());
        for id in sessions {
            registry.insert(test_support::record(id, SessionMode::Interactive));
        }
        (registry, dirs)
    }

    fn conversation_of(registry: &SessionRegistry, session_id: &str) -> Option<String> {
        let rec = registry.get(session_id).expect("the record");
        lock(&rec).conversation_id.clone()
    }

    #[test]
    fn contested_id_is_never_claimed_later() {
        let (registry, dirs) = registry_with("discovery-contested", &["s1", "s2"]);
        let agent = leaked_agent();
        let folder = r"D:\contested";
        agent.set_found(folder, vec![found("c-new", Utc::now())]);
        let s1 = waiter(agent, "s1", folder, &[], launch());
        registry.discovery().register(s1.clone());
        registry
            .discovery()
            .register(waiter(agent, "s2", folder, &[], launch()));

        pass(&registry, &dirs, &s1, Utc::now(), PassBy::Poller);
        assert_eq!(conversation_of(&registry, "s1"), None);
        assert_eq!(conversation_of(&registry, "s2"), None);
        assert_eq!(
            contested::read(&dirs, "test-agent", Utc::now()),
            ids(&["c-new"])
        );

        registry.discovery().deregister("s2");
        pass(&registry, &dirs, &s1, Utc::now(), PassBy::Poller);
        assert_eq!(
            conversation_of(&registry, "s1"),
            None,
            "a lone waiter still may not claim a contested id"
        );
    }

    #[test]
    fn id_held_by_any_record_is_not_claimed() {
        let (registry, dirs) = registry_with("discovery-held", &["s1", "done"]);
        registry.update("done", |rec| {
            rec.status = SessionStatus::Stopped;
            rec.conversation_id = Some("c-held".to_owned());
        });
        let agent = leaked_agent();
        let folder = r"D:\held";
        agent.set_found(folder, vec![found("c-held", Utc::now())]);
        let s1 = waiter(agent, "s1", folder, &[], launch());
        registry.discovery().register(s1.clone());

        pass(&registry, &dirs, &s1, Utc::now(), PassBy::Poller);
        assert_eq!(conversation_of(&registry, "s1"), None);
        assert!(registry.discovery().waiter("s1").is_some(), "still waiting");
    }

    #[test]
    fn id_in_history_ended_within_seven_days_is_not_claimed() {
        let (registry, dirs) = registry_with("discovery-history", &["s1"]);
        let now = Utc::now();
        let mut ended = test_support::record("ended", SessionMode::Interactive);
        ended.conversation_id = Some("c-hist".to_owned());
        let entry = history::entry_from_record(
            &ended,
            SessionEnd::Exited { code: 0 },
            now - TimeDelta::days(1),
        );
        history::write_if_absent(&dirs, &entry).expect("write the history entry");
        let agent = leaked_agent();
        let folder = r"D:\history";
        agent.set_found(folder, vec![found("c-hist", now)]);
        let s1 = waiter(agent, "s1", folder, &[], launch());
        registry.discovery().register(s1.clone());

        pass(&registry, &dirs, &s1, now, PassBy::Poller);
        assert_eq!(conversation_of(&registry, "s1"), None);

        let mut old = entry.clone();
        old.conversation_id = Some("c-old".to_owned());
        old.ended_at = now - TimeDelta::days(8);
        assert_eq!(recent_history_ids(&[entry, old], now), ids(&["c-hist"]));
    }

    #[test]
    fn poll_cadence_is_2s_then_10s_then_60s() {
        let at = |secs: u64| poll_interval(Duration::from_secs(secs));
        assert_eq!(at(0), Duration::from_secs(2));
        assert_eq!(at(5 * 60 - 1), Duration::from_secs(2));
        assert_eq!(at(5 * 60), Duration::from_secs(10));
        assert_eq!(at(60 * 60 - 1), Duration::from_secs(10));
        assert_eq!(at(60 * 60), Duration::from_secs(60));
        assert_eq!(at(24 * 60 * 60), Duration::from_secs(60), "no give-up");
    }

    /// How long, on the paused clock, a test waits for a poller to end.
    const POLLER_END: Duration = Duration::from_secs(30);

    #[tokio::test(start_paused = true)]
    async fn poller_claims_a_conversation_that_appears_after_launch() {
        let (registry, dirs) = registry_with("discovery-poll-claim", &["s1"]);
        let agent = leaked_agent();
        let folder = r"D:\poll-claim";
        let poller = start(&registry, dirs, waiter(agent, "s1", folder, &[], launch()));

        tokio::time::sleep(Duration::from_secs(7)).await;
        assert_eq!(conversation_of(&registry, "s1"), None, "nothing found yet");
        agent.set_found(folder, vec![found("c-late", Utc::now())]);

        tokio::time::timeout(POLLER_END, poller)
            .await
            .expect("the poller ends once it claims")
            .expect("the poller task");
        assert_eq!(conversation_of(&registry, "s1").as_deref(), Some("c-late"));
        assert!(registry.discovery().waiter("s1").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn poller_stops_when_a_hook_sets_the_id() {
        let (registry, dirs) = registry_with("discovery-poll-hook", &["s1"]);
        let agent = leaked_agent();
        let folder = r"D:\poll-hook";
        let poller = start(&registry, dirs, waiter(agent, "s1", folder, &[], launch()));
        registry.update("s1", |rec| {
            rec.conversation_id = Some("from-hook".to_owned());
        });
        agent.set_found(folder, vec![found("c-other", Utc::now())]);

        tokio::time::timeout(POLLER_END, poller)
            .await
            .expect("the poller ends")
            .expect("the poller task");
        assert_eq!(
            conversation_of(&registry, "s1").as_deref(),
            Some("from-hook")
        );
        assert!(registry.discovery().waiter("s1").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn poller_stops_on_stopped_error_and_removal() {
        let (registry, dirs) =
            registry_with("discovery-poll-stop", &["stopped", "error", "removed"]);
        let agent = leaked_agent();
        let folder = r"D:\poll-stop";
        let pollers: Vec<_> = ["stopped", "error", "removed"]
            .into_iter()
            .map(|id| {
                start(
                    &registry,
                    dirs.clone(),
                    waiter(agent, id, folder, &[], launch()),
                )
            })
            .collect();
        registry.update("stopped", |rec| rec.status = SessionStatus::Stopped);
        registry.update("error", |rec| rec.status = SessionStatus::Error);
        registry.remove("removed");

        for (id, poller) in ["stopped", "error", "removed"].into_iter().zip(pollers) {
            let ended = tokio::time::timeout(POLLER_END, poller).await;
            assert!(ended.is_ok_and(|task| task.is_ok()), "the {id} poller ends");
            assert!(
                registry.discovery().waiter(id).is_none(),
                "{id} deregistered"
            );
        }
    }

    #[test]
    fn final_look_records_a_late_id_in_the_history_entry() {
        let (registry, dirs) = registry_with("discovery-final-late", &["s1"]);
        let agent = leaked_agent();
        let folder = r"D:\final-late";
        registry
            .discovery()
            .register(waiter(agent, "s1", folder, &[], launch()));
        agent.set_found(folder, vec![found("c-late", Utc::now())]);

        history::record_session_end(&registry, &dirs, "s1", SessionEnd::Exited { code: 0 });

        let entry = history::read_one(&dirs, "s1").expect("the history entry");
        assert_eq!(entry.conversation_id.as_deref(), Some("c-late"));
        assert_eq!(conversation_of(&registry, "s1").as_deref(), Some("c-late"));
    }

    #[test]
    fn final_look_deregisters_the_waiter() {
        let (registry, dirs) = registry_with("discovery-final-none", &["s1"]);
        registry.discovery().register(waiter(
            leaked_agent(),
            "s1",
            r"D:\final-none",
            &[],
            launch(),
        ));

        history::record_session_end(&registry, &dirs, "s1", SessionEnd::Exited { code: 0 });

        assert!(registry.discovery().waiter("s1").is_none());
        let entry = history::read_one(&dirs, "s1").expect("the history entry");
        assert_eq!(entry.conversation_id, None);
    }

    #[test]
    fn claim_loses_to_a_hook_that_set_the_id_first() {
        let (registry, _dirs) = registry_with("discovery-hook-first", &["s1"]);
        registry.update("s1", |rec| {
            rec.conversation_id = Some("from-hook".to_owned());
        });
        let s1 = waiter(leaked_agent(), "s1", r"D:\hook", &[], launch());
        registry.discovery().register(s1);

        assert!(!claim(&registry, "s1", "c-new", false));
        assert_eq!(
            conversation_of(&registry, "s1").as_deref(),
            Some("from-hook")
        );
        assert!(registry.discovery().waiter("s1").is_none(), "deregistered");
    }
}
