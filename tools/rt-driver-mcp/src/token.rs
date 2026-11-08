//! The driver token: the `claude setup-token` token a driven run's real
//! Claude logs in with, held in one Windows Credential Manager entry
//! (service [`SERVICE`], user [`USER`]) that the owner fills by hand with
//! `tools/rt-driver-mcp/set-driver-token.ps1`.
//!
//! The server sets it on the isolated daemon only, as [`TOKEN_ENV`], never on
//! the client and never from an agent's `env`. The server never puts its
//! value in a tool argument, a tool result, a log line or an error; a shell
//! pane in the run can still read it from the environment it inherits from
//! the isolated daemon.

use std::ffi::OsString;
use std::io::BufRead;

/// The Credential Manager service the driver token is filed under.
pub const SERVICE: &str = "rustling-tulip-driver";

/// The Credential Manager user the driver token is filed under.
pub const USER: &str = "driver-token";

/// A launch whose `env` sets this runs a real Claude, which needs the token.
pub const REAL_AGENT_ENV: &str = "RUSTLING_TULIP_CLAUDE";

/// The variable the token is set as on the isolated daemon.
pub const TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Why a real-Claude launch with no stored token is refused.
pub const NO_TOKEN: &str = "no driver token stored: run tools/rt-driver-mcp/set-driver-token.ps1 once \
     (it asks for the token from claude setup-token)";

/// Why `set-token` refuses an empty line.
pub const EMPTY_TOKEN: &str = "set-token read an empty line: pipe the token from claude setup-token \
     on stdin (tools/rt-driver-mcp/set-driver-token.ps1 does)";

/// Where the driver token is kept.
pub trait TokenStore {
    /// The stored token; `None` when none is stored.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be read; the message never holds the token.
    fn get(&self) -> Result<Option<String>, String>;

    /// Stores `token`, replacing any token stored before.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be written; the message never holds the
    /// token.
    fn set(&self, token: &str) -> Result<(), String>;
}

/// The driver token's Credential Manager entry.
pub struct CredentialManager;

#[cfg(windows)]
impl CredentialManager {
    /// The entry, with Windows Credential Manager installed as the process's
    /// credential store on first use.
    fn entry() -> Result<keyring_core::Entry, String> {
        static STORE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        STORE
            .get_or_init(|| {
                let store = windows_native_keyring_store::Store::new()
                    .map_err(|err| format!("creating the Windows credential store: {err}"))?;
                keyring_core::set_default_store(store);
                Ok(())
            })
            .clone()?;
        keyring_core::Entry::new(SERVICE, USER)
            .map_err(|err| format!("opening the driver token credential: {err}"))
    }
}

#[cfg(windows)]
impl TokenStore for CredentialManager {
    fn get(&self) -> Result<Option<String>, String> {
        match Self::entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(format!("reading the driver token: {err}")),
        }
    }

    fn set(&self, token: &str) -> Result<(), String> {
        Self::entry()?
            .set_password(token)
            .map_err(|err| format!("saving the driver token to Credential Manager: {err}"))
    }
}

#[cfg(not(windows))]
impl TokenStore for CredentialManager {
    fn get(&self) -> Result<Option<String>, String> {
        Err(
            "the driver token lives in Windows Credential Manager; there is none on this system"
                .to_owned(),
        )
    }

    fn set(&self, _token: &str) -> Result<(), String> {
        self.get().map(|_| ())
    }
}

/// Whether a launch's `env` runs a real Claude.
#[must_use]
pub fn uses_real_agent(env: &[(&'static str, OsString)]) -> bool {
    env.iter().any(|(name, _)| *name == REAL_AGENT_ENV)
}

/// The rows to set on the isolated daemon only: the stored token as
/// [`TOKEN_ENV`] when `env` runs a real Claude, else none.
///
/// # Errors
///
/// [`NO_TOKEN`] when a real-Claude launch finds no token stored, or why the
/// store could not be read.
pub fn daemon_only_env(
    env: &[(&'static str, OsString)],
    store: &dyn TokenStore,
) -> Result<Vec<(&'static str, OsString)>, String> {
    if !uses_real_agent(env) {
        return Ok(Vec::new());
    }
    match store.get()? {
        Some(token) if !token.is_empty() => Ok(vec![(TOKEN_ENV, OsString::from(token))]),
        _ => Err(NO_TOKEN.to_owned()),
    }
}

/// Reads one line from `input`, trims it and stores it in `store`.
///
/// # Errors
///
/// [`EMPTY_TOKEN`] for an empty line, or why reading or storing failed.
pub fn set_token(input: &mut dyn BufRead, store: &dyn TokenStore) -> Result<(), String> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|err| format!("reading the token from stdin: {err}"))?;
    let token = line.trim();
    if token.is_empty() {
        return Err(EMPTY_TOKEN.to_owned());
    }
    store.set(token)
}

/// A token held in memory, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    token: std::sync::Mutex<Option<String>>,
}

#[cfg(test)]
impl MemoryStore {
    /// The token stored so far.
    pub fn stored(&self) -> Option<String> {
        self.token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
impl TokenStore for MemoryStore {
    fn get(&self) -> Result<Option<String>, String> {
        Ok(self.stored())
    }

    fn set(&self, token: &str) -> Result<(), String> {
        *self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token.to_owned());
        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use super::{EMPTY_TOKEN, MemoryStore, TokenStore as _, set_token};

    /// `set-token` stores its one line trimmed and refuses an empty one,
    /// storing nothing.
    #[test]
    fn set_token_trims_and_refuses_an_empty_line() {
        let store = MemoryStore::default();
        set_token(&mut "  tok-123 \r\nignored\n".as_bytes(), &store).expect("a token is stored");
        assert_eq!(store.stored().as_deref(), Some("tok-123"));

        let empty = MemoryStore::default();
        for line in ["", "\n", "   \r\n"] {
            let refused =
                set_token(&mut line.as_bytes(), &empty).expect_err("an empty line is refused");
            assert_eq!(refused, EMPTY_TOKEN, "{line:?}");
        }
        assert_eq!(empty.get().expect("reads"), None, "nothing was stored");
    }
}
