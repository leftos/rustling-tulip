//! Hand-parsed arguments of the session-driving verbs.

use std::ffi::OsString;

const USAGE_SESSIONS: &str = "usage: rustling-tulipd sessions [--json]";
const USAGE_READ: &str = "usage: rustling-tulipd read <session id> [--scrollback] [--json]";

/// What a verb was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Verb {
    Sessions,
    Read {
        session_id: String,
        scrollback: bool,
    },
}

impl Verb {
    /// The word that names the verb on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Read { .. } => "read",
        }
    }
}

/// One parsed command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    pub verb: Verb,
    pub json: bool,
}

/// The command `args` ask for, the verb first, or the usage error to print.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let verb = args.next().unwrap_or_default();
    if verb == "sessions" {
        parse_sessions(args)
    } else if verb == "read" {
        parse_read(args)
    } else {
        Err(format!("unknown verb {}; {USAGE_SESSIONS}", verb.display()))
    }
}

fn parse_sessions(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else {
            return Err(unexpected(&arg, USAGE_SESSIONS));
        }
    }
    Ok(Command {
        verb: Verb::Sessions,
        json,
    })
}

fn parse_read(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut session_id: Option<String> = None;
    let mut scrollback = false;
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else if arg == "--scrollback" {
            scrollback = true;
        } else if arg.to_string_lossy().starts_with("--") || session_id.is_some() {
            return Err(unexpected(&arg, USAGE_READ));
        } else {
            session_id = Some(arg.to_string_lossy().into_owned());
        }
    }
    let session_id = session_id.ok_or_else(|| format!("no session id; {USAGE_READ}"))?;
    Ok(Command {
        verb: Verb::Read {
            session_id,
            scrollback,
        },
        json,
    })
}

fn unexpected(arg: &OsString, usage: &str) -> String {
    format!("unexpected argument {}; {usage}", arg.display())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn parsed(list: &[&str]) -> Result<Command, String> {
        parse(list.iter().map(OsString::from))
    }

    #[test]
    fn read_without_an_id_is_a_usage_error() {
        let err = parsed(&["read", "--json"]).expect_err("the id is required");
        assert_eq!(err, format!("no session id; {USAGE_READ}"));
    }

    #[test]
    fn an_unknown_flag_is_a_usage_error() {
        let err = parsed(&["read", "s1", "--wide"]).expect_err("the flag is unknown");
        assert_eq!(err, format!("unexpected argument --wide; {USAGE_READ}"));
        let err = parsed(&["sessions", "--wide"]).expect_err("the flag is unknown");
        assert_eq!(err, format!("unexpected argument --wide; {USAGE_SESSIONS}"));
    }

    #[test]
    fn a_second_positional_is_a_usage_error() {
        let err = parsed(&["read", "s1", "s2"]).expect_err("one id only");
        assert_eq!(err, format!("unexpected argument s2; {USAGE_READ}"));
        let err = parsed(&["sessions", "s1"]).expect_err("sessions takes no id");
        assert_eq!(err, format!("unexpected argument s1; {USAGE_SESSIONS}"));
    }
}
