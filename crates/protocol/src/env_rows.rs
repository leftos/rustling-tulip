//! Helpers for the spawn dialog's environment rows, shared by the daemon and
//! its clients so the two sides cannot drift.
//!
//! Two questions about a row come up on both sides of the wire:
//!
//! - Is it a *secret row*, one whose key names a credential? The dialog warns
//!   on one and ticks its Secret toggle; the daemon seals one at spawn.
//! - Is its value a *reference*, a placeholder resolved at spawn, or a literal?
//!   The dialog renders a reference as a reference; the daemon resolves one.
//!
//! Both answers live here rather than in each caller.

/// Length of a secret id in characters.
const SECRET_ID_LEN: usize = 32;

/// What an environment row's value points at when it is a reference rather
/// than a literal. A value that is *exactly* `${env:NAME}` or `${secret:ID}` is
/// a reference; anything else, a reference embedded in other text included, is
/// a literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// `${env:NAME}`: at spawn the daemon reads `NAME` from its own
    /// environment, then (on Windows) from the user's persistent environment,
    /// then from the system environment.
    Env(String),
    /// `${secret:ID}`: at spawn the daemon reads the value saved under `ID` in
    /// Windows Credential Manager.
    Secret(String),
}

/// Parse `value` as a reference.
///
/// `NAME` is an environment variable name: a leading ASCII letter or `_`, then
/// ASCII letters, digits or `_`. `ID` is 32 lowercase hexadecimal characters
/// (the simple form of a UUID v4). A value that doesn't match one of those two
/// shapes exactly — embedded in other text, unclosed, a bad name, a short or
/// upper-case id — is a literal, not a reference.
#[must_use]
pub fn reference(value: &str) -> Option<Reference> {
    if let Some(name) = value
        .strip_prefix("${env:")
        .and_then(|rest| rest.strip_suffix('}'))
        && is_env_name(name)
    {
        return Some(Reference::Env(name.to_owned()));
    }
    if let Some(id) = value
        .strip_prefix("${secret:")
        .and_then(|rest| rest.strip_suffix('}'))
        && is_secret_id(id)
    {
        return Some(Reference::Secret(id.to_owned()));
    }
    None
}

/// Whether `key` names a credential: any whole `_`-delimited segment of it
/// equals `KEY`, `TOKEN`, `SECRET` or `PASSWORD`, in any case.
/// `ANTHROPIC_API_KEY` and `db_password` match; `KEYBOARD` and
/// `MONKEY_BUSINESS` do not.
#[must_use]
pub fn is_secret_key(key: &str) -> bool {
    const SECRET_SEGMENTS: [&str; 4] = ["KEY", "TOKEN", "SECRET", "PASSWORD"];
    key.split('_').any(|segment| {
        SECRET_SEGMENTS
            .iter()
            .any(|word| segment.eq_ignore_ascii_case(word))
    })
}

/// An environment variable name: a leading ASCII letter or `_`, then ASCII
/// letters, digits or `_`. Empty is not a name.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A secret id: exactly 32 lowercase hexadecimal characters.
fn is_secret_id(id: &str) -> bool {
    id.len() == SECRET_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_parses_env_and_secret_forms_and_rejects_the_rest() {
        assert_eq!(
            reference("${env:HOME}"),
            Some(Reference::Env("HOME".to_owned()))
        );
        assert_eq!(
            reference("${env:_PRIVATE}"),
            Some(Reference::Env("_PRIVATE".to_owned()))
        );
        assert_eq!(
            reference("${env:RT_TEST_2}"),
            Some(Reference::Env("RT_TEST_2".to_owned()))
        );
        assert_eq!(
            reference("${secret:0123456789abcdef0123456789abcdef}"),
            Some(Reference::Secret(
                "0123456789abcdef0123456789abcdef".to_owned()
            ))
        );

        // Embedded in other text: not a reference.
        assert_eq!(reference("prefix${env:HOME}"), None);
        assert_eq!(reference("${env:HOME}suffix"), None);
        assert_eq!(
            reference("x${secret:0123456789abcdef0123456789abcdef}"),
            None
        );

        // Unclosed.
        assert_eq!(reference("${env:HOME"), None);
        assert_eq!(reference("${secret:0123456789abcdef0123456789abcdef"), None);

        // A bad env name.
        assert_eq!(reference("${env:}"), None);
        assert_eq!(reference("${env:1BAD}"), None);
        assert_eq!(reference("${env:HOM E}"), None);
        assert_eq!(reference("${env:HOME.bad}"), None);

        // A 31-character id, and an upper-case one.
        assert_eq!(reference("${secret:0123456789abcdef0123456789abcde}"), None);
        assert_eq!(
            reference("${secret:0123456789ABCDEF0123456789ABCDEF}"),
            None
        );

        // Plain literals.
        assert_eq!(reference("sk-ant-literal"), None);
        assert_eq!(reference(""), None);
    }

    #[test]
    fn secret_key_matches_whole_segments_only() {
        assert!(is_secret_key("ANTHROPIC_API_KEY"));
        assert!(is_secret_key("db_password"));
        assert!(is_secret_key("GH_TOKEN"));
        assert!(is_secret_key("MY_SECRET"));
        assert!(is_secret_key("key"));
        assert!(!is_secret_key("KEYBOARD"));
        assert!(!is_secret_key("MONKEY_BUSINESS"));
        assert!(!is_secret_key("APIKEY"));
        assert!(!is_secret_key("RUST_LOG"));
        assert!(!is_secret_key(""));
    }
}
