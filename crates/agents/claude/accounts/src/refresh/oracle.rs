//! The ownership oracle: whose account a credential's access token belongs to, asked of the profile endpoint, and
//! the verdicts kept per credential lineage.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;

use crate::paths::Email;
use crate::store::AccountRecord;
use crate::transport::RawProfile;

/// The account a profile answer names: its uuid (never empty), and its email and organization uuid when the answer
/// carries them as strings. Its `Debug` names none of them.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResolvedProfile {
    pub(crate) uuid: String,
    pub(crate) email: Option<String>,
    pub(crate) org_uuid: Option<String>,
}

impl fmt::Debug for ResolvedProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedProfile")
            .finish_non_exhaustive()
    }
}

/// The account a profile answer resolves to: a 2xx answer whose `account`
/// object carries a non-empty string `uuid` (trimmed); `account.email` and `organization.uuid` only when they are
/// strings. Anything else is unresolved.
pub(crate) fn resolve(answer: &RawProfile) -> Option<ResolvedProfile> {
    if !(200..300).contains(&answer.status) {
        return None;
    }
    let account = answer.body.get("account")?.as_object()?;
    let uuid = account.get("uuid")?.as_str()?.trim();
    if uuid.is_empty() {
        return None;
    }
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    Some(ResolvedProfile {
        uuid: uuid.to_owned(),
        email: text(account.get("email")),
        org_uuid: text(
            answer
                .body
                .get("organization")
                .and_then(|organization| organization.get("uuid")),
        ),
    })
}

/// Whether a resolved profile is a slot's account, a tri-state answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The same account, by uuid.
    Same,
    /// The same account by email and organization; the slot has no uuid yet and should record the resolved one.
    SameWithoutUuid,
    /// Definitely another account. Safe to remember.
    Foreign,
    /// Too little to tell: never remembered, and nothing is written on it.
    Unverifiable,
}

/// Whether `resolved` is `own`'s account. With a uuid on both
/// sides, the uuids decide, the organizations only corroborating (a missing one on either side is tolerated). A slot
/// without a uuid is affirmed only by an exact email and organization match with the resolved organization present;
/// the same email under another organization, or a resolved email and organization matching nothing, is foreign; a
/// profile too partial for either is unverifiable.
pub(crate) fn resolved_matches_slot_identity(
    own: &AccountRecord,
    resolved: &ResolvedProfile,
) -> Verdict {
    resolved_matches_identity(own.email.as_str(), &own.org_uuid, &own.uuid, resolved)
}

/// As [`resolved_matches_slot_identity`] decides it, for a login no record holds yet: `email`, `org_uuid` and `uuid`
/// are the login's own (an empty one counting as none).
pub(crate) fn resolved_matches_identity(
    email: &str,
    org_uuid: &str,
    uuid: &str,
    resolved: &ResolvedProfile,
) -> Verdict {
    if !resolved.uuid.is_empty() && !uuid.is_empty() {
        return matches_by_uuid(org_uuid, uuid, resolved);
    }
    matches_by_email_and_organization(email, org_uuid, resolved)
}

/// Both sides carry a uuid: the uuids decide, and an organization present on both sides must agree.
fn matches_by_uuid(org_uuid: &str, uuid: &str, resolved: &ResolvedProfile) -> Verdict {
    let org_compatible = match resolved.org_uuid.as_deref() {
        None | Some("") => true,
        Some(org) => org_uuid.is_empty() || org == org_uuid,
    };
    if resolved.uuid == uuid && org_compatible {
        Verdict::Same
    } else {
        Verdict::Foreign
    }
}

/// The slot has no uuid: an exact email and organization match, with the resolved organization present, affirms it.
fn matches_by_email_and_organization(
    own_email: &str,
    org_uuid: &str,
    resolved: &ResolvedProfile,
) -> Verdict {
    let Some(email) = resolved.email.as_deref().filter(|email| !email.is_empty()) else {
        return Verdict::Unverifiable;
    };
    match resolved.org_uuid.as_deref() {
        None => Verdict::Unverifiable,
        Some(org) if email == own_email && org == org_uuid => Verdict::SameWithoutUuid,
        Some(_) => Verdict::Foreign,
    }
}

/// A credential lineage as one slot sees it: the account number, the caller's email, the
/// slot's stored email, organization and uuid, and the credential's fingerprint. A slot re-created for another account
/// builds another key, so it never inherits its predecessor's verdicts. Its `Debug` shows the account number alone.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct LineageKey {
    number: u32,
    caller_email: String,
    email: String,
    org_uuid: String,
    uuid: String,
    fingerprint: String,
}

impl fmt::Debug for LineageKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LineageKey")
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

impl LineageKey {
    /// The key of the lineage `fingerprint` in slot `own`, as the caller holding `caller_email` consults it.
    pub(crate) fn new(caller_email: &Email, own: &AccountRecord, fingerprint: &str) -> Self {
        Self {
            number: own.number,
            caller_email: caller_email.as_str().to_owned(),
            email: own.email.as_str().to_owned(),
            org_uuid: own.org_uuid.clone(),
            uuid: own.uuid.clone(),
            fingerprint: fingerprint.to_owned(),
        }
    }

    /// The key a verdict is remembered under once the oracle answered: `own` is the slot as read before the answer, and
    /// only its uuid is replaced, by `backfilled`, when the match recorded the resolved uuid on it.
    pub(crate) fn after_match(
        caller_email: &Email,
        own: &AccountRecord,
        backfilled: Option<&str>,
        fingerprint: &str,
    ) -> Self {
        let mut key = Self::new(caller_email, own, fingerprint);
        if let Some(uuid) = backfilled {
            uuid.clone_into(&mut key.uuid);
        }
        key
    }
}

/// The ownership verdicts given so far, per credential lineage: `true` when the lineage is
/// the slot's own (the oracle affirmed it, or this process's own refresh produced it), `false` when the oracle named
/// another account. Held in memory only, for the life of the poller; an unverifiable answer or an oracle failure is
/// never remembered. Its `Debug` shows how many verdicts it holds, never whose.
#[derive(Default)]
pub struct ProbeVerdicts {
    verdicts: Mutex<HashMap<LineageKey, bool>>,
}

impl fmt::Debug for ProbeVerdicts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeVerdicts")
            .field("entries", &self.lock().len())
            .finish()
    }
}

impl ProbeVerdicts {
    /// An empty memo.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The verdict remembered for `key`, if any.
    pub(crate) fn get(&self, key: &LineageKey) -> Option<bool> {
        self.lock().get(key).copied()
    }

    /// Remembers the oracle's `verdict` for `key` and returns it as a memo entry: an unverifiable one is not stored.
    pub(crate) fn remember(&self, key: LineageKey, verdict: Verdict) -> Option<bool> {
        let owned = match verdict {
            Verdict::Same | Verdict::SameWithoutUuid => true,
            Verdict::Foreign => false,
            Verdict::Unverifiable => return None,
        };
        self.lock().insert(key, owned);
        Some(owned)
    }

    /// Remembers `key` as the slot's own: this process's refresh produced it.
    pub(crate) fn affirm(&self, key: LineageKey) {
        self.lock().insert(key, true);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<LineageKey, bool>> {
        // The map is only ever read or written whole under the lock, so a panic elsewhere cannot leave it half-made.
        self.verdicts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose fixture email is refused must fail loudly"
)]
mod tests {
    use serde_json::json;

    use super::{
        LineageKey, ProbeVerdicts, ResolvedProfile, Verdict, resolve,
        resolved_matches_slot_identity,
    };
    use crate::paths::Email;
    use crate::store::{AccountKind, AccountRecord};
    use crate::transport::RawProfile;

    const EMAIL: &str = "one@example.test";
    const ORG: &str = "org-1";
    const UUID: &str = "uuid-1";

    fn slot(uuid: &str, org_uuid: &str) -> AccountRecord {
        AccountRecord {
            number: 1,
            email: Email::parse(EMAIL).expect("the test email is safe"),
            uuid: uuid.to_owned(),
            org_uuid: org_uuid.to_owned(),
            org_name: String::new(),
            added: String::new(),
            alias: None,
            disabled: false,
            may_spend_credits: false,
            kind: AccountKind::Oauth,
            generation: 0,
        }
    }

    fn profile(uuid: &str, email: Option<&str>, org_uuid: Option<&str>) -> ResolvedProfile {
        ResolvedProfile {
            uuid: uuid.to_owned(),
            email: email.map(str::to_owned),
            org_uuid: org_uuid.map(str::to_owned),
        }
    }

    #[test]
    fn resolved_identity_matches_the_slot_in_every_case() {
        let cases = [
            (
                "uuid and org equal",
                slot(UUID, ORG),
                profile(UUID, Some(EMAIL), Some(ORG)),
                Verdict::Same,
            ),
            (
                "uuid equal, profile has no org",
                slot(UUID, ORG),
                profile(UUID, Some(EMAIL), None),
                Verdict::Same,
            ),
            (
                "uuid equal, profile org empty",
                slot(UUID, ORG),
                profile(UUID, None, Some("")),
                Verdict::Same,
            ),
            (
                "uuid equal, slot has no org",
                slot(UUID, ""),
                profile(UUID, None, Some(ORG)),
                Verdict::Same,
            ),
            (
                "uuid equal, orgs differ",
                slot(UUID, ORG),
                profile(UUID, Some(EMAIL), Some("org-2")),
                Verdict::Foreign,
            ),
            (
                "uuid conflict",
                slot(UUID, ORG),
                profile("uuid-2", Some(EMAIL), Some(ORG)),
                Verdict::Foreign,
            ),
            (
                "no slot uuid, email and org equal",
                slot("", ORG),
                profile(UUID, Some(EMAIL), Some(ORG)),
                Verdict::SameWithoutUuid,
            ),
            (
                "no slot uuid, both orgs empty",
                slot("", ""),
                profile(UUID, Some(EMAIL), Some("")),
                Verdict::SameWithoutUuid,
            ),
            (
                "no slot uuid, same email in another org",
                slot("", ORG),
                profile(UUID, Some(EMAIL), Some("org-2")),
                Verdict::Foreign,
            ),
            (
                "no slot uuid, same email, no resolved org",
                slot("", ORG),
                profile(UUID, Some(EMAIL), None),
                Verdict::Unverifiable,
            ),
            (
                "no slot uuid, another email and an org",
                slot("", ORG),
                profile(UUID, Some("two@example.test"), Some(ORG)),
                Verdict::Foreign,
            ),
            (
                "no slot uuid, another email, no org",
                slot("", ORG),
                profile(UUID, Some("two@example.test"), None),
                Verdict::Unverifiable,
            ),
            (
                "no slot uuid, no email",
                slot("", ORG),
                profile(UUID, None, Some(ORG)),
                Verdict::Unverifiable,
            ),
            (
                "no slot uuid, empty email",
                slot("", ORG),
                profile(UUID, Some(""), Some(ORG)),
                Verdict::Unverifiable,
            ),
        ];
        for (name, own, resolved, expected) in cases {
            assert_eq!(
                resolved_matches_slot_identity(&own, &resolved),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn profile_without_an_account_uuid_resolves_nothing() {
        let answer = |status: u16, body: serde_json::Value| RawProfile { status, body };
        let full = json!({ "account": { "uuid": " uuid-1 ", "email": EMAIL }, "organization": { "uuid": ORG } });
        assert_eq!(
            resolve(&answer(200, full.clone())),
            Some(profile(UUID, Some(EMAIL), Some(ORG))),
            "trimmed"
        );
        assert_eq!(resolve(&answer(401, full)), None, "a refused token");
        assert_eq!(
            resolve(&answer(200, json!({ "account": { "email": EMAIL } }))),
            None,
            "no uuid"
        );
        assert_eq!(
            resolve(&answer(200, json!({ "account": { "uuid": "  " } }))),
            None,
            "an empty uuid"
        );
        assert_eq!(
            resolve(&answer(200, json!({ "account": { "uuid": 7 } }))),
            None,
            "a uuid that is not a string"
        );
        assert_eq!(
            resolve(&answer(200, json!({ "uuid": UUID }))),
            None,
            "no account object"
        );
        assert_eq!(
            resolve(&answer(200, serde_json::Value::Null)),
            None,
            "no body"
        );
        let partial = json!({ "account": { "uuid": UUID, "email": 3 }, "organization": "org-1" });
        assert_eq!(
            resolve(&answer(200, partial)),
            Some(profile(UUID, None, None)),
            "a uuid-only answer resolves"
        );
    }

    #[test]
    fn memo_never_stores_an_unverifiable_verdict() {
        let verdicts = ProbeVerdicts::new();
        let own = slot(UUID, ORG);
        let key = |fingerprint: &str| LineageKey::new(&own.email, &own, fingerprint);

        assert_eq!(
            verdicts.remember(key("sha256:a"), Verdict::Unverifiable),
            None
        );
        assert_eq!(verdicts.get(&key("sha256:a")), None);
        assert_eq!(verdicts.len(), 0, "nothing was stored");

        assert_eq!(
            verdicts.remember(key("sha256:a"), Verdict::Foreign),
            Some(false)
        );
        assert_eq!(
            verdicts.remember(key("sha256:b"), Verdict::SameWithoutUuid),
            Some(true)
        );
        assert_eq!(verdicts.get(&key("sha256:a")), Some(false));
        assert_eq!(verdicts.get(&key("sha256:b")), Some(true));
        let recreated = slot("uuid-2", ORG);
        assert_eq!(
            verdicts.get(&LineageKey::new(&own.email, &recreated, "sha256:b")),
            None,
            "a slot re-created for another account does not inherit the verdict"
        );
    }

    #[test]
    fn profile_types_debug_prints_no_identity() {
        const FINGERPRINT: &str = "sha256:0123456789abcdef";
        let own = slot(UUID, ORG);
        let key = LineageKey::new(&own.email, &own, FINGERPRINT);
        let verdicts = ProbeVerdicts::new();
        verdicts.affirm(key.clone());
        let raw = RawProfile {
            status: 200,
            body: json!({ "account": { "uuid": UUID, "email": EMAIL }, "organization": { "uuid": ORG } }),
        };
        let resolved = profile(UUID, Some(EMAIL), Some(ORG));

        let printed = [
            format!("{raw:?}"),
            format!("{resolved:?}"),
            format!("{key:?}"),
            format!("{verdicts:?}"),
        ];
        for text in printed {
            for identity in [EMAIL, UUID, ORG, FINGERPRINT] {
                assert!(!text.contains(identity), "{text} carries {identity}");
            }
        }
    }

    #[test]
    fn key_after_a_match_takes_only_the_backfilled_uuid() {
        let read = slot("", ORG);
        let mut filled = read.clone();
        filled.uuid = UUID.to_owned();

        assert_eq!(
            LineageKey::after_match(&read.email, &read, Some(UUID), "sha256:a"),
            LineageKey::new(&read.email, &filled, "sha256:a"),
            "the record as read, with the uuid the match recorded"
        );
        assert_eq!(
            LineageKey::after_match(&read.email, &read, None, "sha256:a"),
            LineageKey::new(&read.email, &read, "sha256:a"),
            "no backfill: the record as read"
        );
        let mut moved = filled.clone();
        moved.org_uuid = "org-2".to_owned();
        assert_ne!(
            LineageKey::after_match(&read.email, &read, Some(UUID), "sha256:a"),
            LineageKey::new(&read.email, &moved, "sha256:a"),
            "a record that changed meanwhile builds another key"
        );
    }
}
