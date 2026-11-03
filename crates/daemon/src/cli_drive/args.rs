//! Hand-parsed arguments of the session-driving verbs.

use protocol::SessionStatus;
use regex::Regex;
use std::ffi::OsString;
use std::time::Duration;

const USAGE_SESSIONS: &str = "usage: rustling-tulipd sessions [--json]";
const USAGE_READ: &str = "usage: rustling-tulipd read <session id> [--scrollback] [--json]";
const USAGE_SPAWN: &str = "usage: rustling-tulipd spawn [--agent <id>] [--repo <name>] \
                           [--cwd <folder>] [--prompt <text>] [--label <label>] [--json]";
const USAGE_SEND: &str =
    "usage: rustling-tulipd send <session id> <text> | --keys <spec> [--raw] [--json]";
const USAGE_KILL: &str = "usage: rustling-tulipd kill <session id> [--json]";
const USAGE_WAIT: &str = "usage: rustling-tulipd wait <session id> (--status <status> | \
                          --text <regex>) [--timeout <seconds>] [--json]";
const USAGE_WATCH: &str =
    "usage: rustling-tulipd watch <session id> [--timeout <seconds>] [--json]";

/// How long `wait` waits unless `--timeout` says.
const DEFAULT_WAIT: Duration = Duration::from_secs(300);

/// Ends the flags on a command line: what follows is positional text.
const END_OF_FLAGS: &str = "--";

/// Where a spawned session runs.
#[derive(Debug)]
pub enum Place {
    /// In place in the registered repo of this name.
    Repo(String),
    /// In this folder.
    Cwd(String),
}

/// What `spawn` was asked to start.
#[derive(Debug)]
pub struct SpawnOptions {
    pub agent: Option<String>,
    pub place: Place,
    pub prompt: Option<String>,
    pub label: Option<String>,
}

/// What `send` delivers.
#[derive(Debug)]
pub enum Payload {
    Text { text: String, raw: bool },
    Keys(Vec<u8>),
}

/// What `wait` waits for.
#[derive(Debug)]
pub enum Until {
    Status(SessionStatus),
    Text(Regex),
}

/// What a verb was asked to do.
#[derive(Debug)]
pub enum Verb {
    Sessions,
    Read {
        session_id: String,
        scrollback: bool,
    },
    Spawn(SpawnOptions),
    Send {
        session_id: String,
        payload: Payload,
    },
    Kill {
        session_id: String,
    },
    Wait {
        session_id: String,
        until: Until,
        timeout: Duration,
    },
    Watch {
        session_id: String,
        timeout: Option<Duration>,
    },
}

impl Verb {
    /// The word that names the verb on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Read { .. } => "read",
            Self::Spawn(_) => "spawn",
            Self::Send { .. } => "send",
            Self::Kill { .. } => "kill",
            Self::Wait { .. } => "wait",
            Self::Watch { .. } => "watch",
        }
    }
}

/// One parsed command line.
#[derive(Debug)]
pub struct Command {
    pub verb: Verb,
    pub json: bool,
}

/// True when `args` carry `--json` as a flag, not inside the text after `--`.
pub fn wants_json(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|arg| *arg != END_OF_FLAGS)
        .any(|arg| arg == "--json")
}

/// True when a failure of `args` prints as JSON: the parsed flag, or for a
/// command line that does not parse, what a scan of it shows.
pub fn json_errors(args: &[OsString]) -> bool {
    parse(args.iter().cloned()).map_or_else(|_| wants_json(args), |command| command.json)
}

/// The flags a verb takes.
struct Grammar {
    /// Flags that stand alone, beside `--json`.
    switches: &'static [&'static str],
    /// Flags that take the next argument as their value, whatever it looks like.
    valued: &'static [&'static str],
    /// How many positionals it takes.
    positionals: usize,
    usage: &'static str,
}

/// A command line split by its verb's [`Grammar`].
#[derive(Default)]
struct Scanned {
    json: bool,
    switches: Vec<&'static str>,
    values: Vec<(&'static str, String)>,
    positionals: Vec<String>,
}

impl Scanned {
    fn has(&self, switch: &str) -> bool {
        self.switches.contains(&switch)
    }

    /// The value of `flag`, the last one when it was given twice.
    fn value(&self, flag: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(name, _)| *name == flag)
            .map(|(_, value)| value.as_str())
    }

    fn positional(&self, index: usize) -> Option<&str> {
        self.positionals.get(index).map(String::as_str)
    }
}

fn scan(args: impl IntoIterator<Item = OsString>, grammar: &Grammar) -> Result<Scanned, String> {
    let mut scanned = Scanned::default();
    let mut args = args.into_iter();
    let mut flags_ended = false;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        if flags_ended {
            add_positional(&mut scanned, &arg, text, grammar)?;
        } else if text == END_OF_FLAGS {
            flags_ended = true;
        } else if text == "--json" {
            scanned.json = true;
        } else if let Some(switch) = grammar.switches.iter().find(|name| **name == text) {
            scanned.switches.push(switch);
        } else if let Some(flag) = grammar.valued.iter().find(|name| **name == text) {
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} takes a value; {}", grammar.usage))?;
            scanned
                .values
                .push((flag, value.to_string_lossy().into_owned()));
        } else if text.starts_with("--") {
            return Err(unexpected(&arg, grammar.usage));
        } else {
            add_positional(&mut scanned, &arg, text, grammar)?;
        }
    }
    Ok(scanned)
}

fn add_positional(
    scanned: &mut Scanned,
    arg: &OsString,
    text: String,
    grammar: &Grammar,
) -> Result<(), String> {
    if scanned.positionals.len() == grammar.positionals {
        return Err(unexpected(arg, grammar.usage));
    }
    scanned.positionals.push(text);
    Ok(())
}

/// The command `args` ask for, the verb first, or the usage error to print.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let verb = args.next().unwrap_or_default();
    if verb == "sessions" {
        parse_sessions(args)
    } else if verb == "read" {
        parse_read(args)
    } else if verb == "spawn" {
        parse_spawn(args)
    } else if verb == "send" {
        parse_send(args)
    } else if verb == "kill" {
        parse_kill(args)
    } else if verb == "wait" {
        parse_wait(args)
    } else if verb == "watch" {
        parse_watch(args)
    } else {
        Err(format!("unknown verb {}; {USAGE_SESSIONS}", verb.display()))
    }
}

fn parse_sessions(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &[],
        valued: &[],
        positionals: 0,
        usage: USAGE_SESSIONS,
    };
    let scanned = scan(args, &grammar)?;
    Ok(Command {
        verb: Verb::Sessions,
        json: scanned.json,
    })
}

fn parse_read(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &["--scrollback"],
        valued: &[],
        positionals: 1,
        usage: USAGE_READ,
    };
    let scanned = scan(args, &grammar)?;
    let session_id = session_id(&scanned, USAGE_READ)?;
    Ok(Command {
        verb: Verb::Read {
            session_id,
            scrollback: scanned.has("--scrollback"),
        },
        json: scanned.json,
    })
}

fn parse_spawn(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &[],
        valued: &["--agent", "--repo", "--cwd", "--prompt", "--label"],
        positionals: 0,
        usage: USAGE_SPAWN,
    };
    let scanned = scan(args, &grammar)?;
    let place = match (scanned.value("--repo"), scanned.value("--cwd")) {
        (Some(repo), None) => Place::Repo(repo.to_owned()),
        (None, Some(cwd)) => Place::Cwd(cwd.to_owned()),
        _ => return Err(format!("give one of --repo and --cwd; {USAGE_SPAWN}")),
    };
    let agent = scanned.value("--agent").map(str::to_owned);
    let prompt = scanned.value("--prompt").map(str::to_owned);
    if prompt.is_some() && agent.is_none() {
        return Err(format!("--prompt needs --agent; {USAGE_SPAWN}"));
    }
    Ok(Command {
        verb: Verb::Spawn(SpawnOptions {
            agent,
            place,
            prompt,
            label: scanned.value("--label").map(str::to_owned),
        }),
        json: scanned.json,
    })
}

fn parse_send(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &["--raw"],
        valued: &["--keys"],
        positionals: 2,
        usage: USAGE_SEND,
    };
    let scanned = scan(args, &grammar)?;
    let session_id = session_id(&scanned, USAGE_SEND)?;
    let raw = scanned.has("--raw");
    let payload = match (scanned.positional(1), scanned.value("--keys")) {
        (Some(text), None) => Payload::Text {
            text: text.to_owned(),
            raw,
        },
        (None, Some(_)) if raw => return Err(format!("--raw is for text; {USAGE_SEND}")),
        (None, Some(spec)) => {
            Payload::Keys(super::keys::decode(spec).map_err(|err| format!("{err}; {USAGE_SEND}"))?)
        }
        _ => return Err(format!("give one of <text> and --keys; {USAGE_SEND}")),
    };
    Ok(Command {
        verb: Verb::Send {
            session_id,
            payload,
        },
        json: scanned.json,
    })
}

fn parse_kill(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &[],
        valued: &[],
        positionals: 1,
        usage: USAGE_KILL,
    };
    let scanned = scan(args, &grammar)?;
    Ok(Command {
        verb: Verb::Kill {
            session_id: session_id(&scanned, USAGE_KILL)?,
        },
        json: scanned.json,
    })
}

fn parse_wait(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &[],
        valued: &["--status", "--text", "--timeout"],
        positionals: 1,
        usage: USAGE_WAIT,
    };
    let scanned = scan(args, &grammar)?;
    let session_id = session_id(&scanned, USAGE_WAIT)?;
    let until = match (scanned.value("--status"), scanned.value("--text")) {
        (Some(status), None) => Until::Status(parse_status(status)?),
        (None, Some(pattern)) => Until::Text(
            Regex::new(pattern).map_err(|err| format!("bad regex: {err}; {USAGE_WAIT}"))?,
        ),
        _ => return Err(format!("give one of --status and --text; {USAGE_WAIT}")),
    };
    Ok(Command {
        verb: Verb::Wait {
            session_id,
            until,
            timeout: timeout(&scanned, USAGE_WAIT)?.unwrap_or(DEFAULT_WAIT),
        },
        json: scanned.json,
    })
}

fn parse_watch(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let grammar = Grammar {
        switches: &[],
        valued: &["--timeout"],
        positionals: 1,
        usage: USAGE_WATCH,
    };
    let scanned = scan(args, &grammar)?;
    Ok(Command {
        verb: Verb::Watch {
            session_id: session_id(&scanned, USAGE_WATCH)?,
            timeout: timeout(&scanned, USAGE_WATCH)?,
        },
        json: scanned.json,
    })
}

fn session_id(scanned: &Scanned, usage: &str) -> Result<String, String> {
    scanned
        .positional(0)
        .map(str::to_owned)
        .ok_or_else(|| format!("no session id; {usage}"))
}

fn timeout(scanned: &Scanned, usage: &str) -> Result<Option<Duration>, String> {
    scanned
        .value("--timeout")
        .map(|seconds| {
            seconds
                .parse()
                .map(Duration::from_secs)
                .map_err(|_| format!("--timeout takes whole seconds; {usage}"))
        })
        .transpose()
}

fn parse_status(word: &str) -> Result<SessionStatus, String> {
    match word {
        "spawning" => Ok(SessionStatus::Spawning),
        "idle" => Ok(SessionStatus::Idle),
        "working" => Ok(SessionStatus::Working),
        "awaiting_input" => Ok(SessionStatus::AwaitingInput),
        "stopped" => Ok(SessionStatus::Stopped),
        "error" => Ok(SessionStatus::Error),
        other => Err(format!("unknown status {other}; {USAGE_WAIT}")),
    }
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

    fn sent_text(command: &Command) -> Option<(&str, bool)> {
        match &command.verb {
            Verb::Send {
                payload: Payload::Text { text, raw },
                ..
            } => Some((text, *raw)),
            _ => None,
        }
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

    #[test]
    fn text_after_a_double_dash_may_look_like_a_flag() {
        let command = parsed(&["send", "s1", "--", "--raw"]).expect("text after the dashes");
        assert_eq!(sent_text(&command), Some(("--raw", false)));
        let command = parsed(&["send", "s1", "--raw", "--", "-x"]).expect("flag, then text");
        assert_eq!(sent_text(&command), Some(("-x", true)));
    }

    #[test]
    fn a_label_that_looks_like_the_json_flag_stays_a_label() {
        let args: Vec<OsString> = ["spawn", "--cwd", ".", "--label", "--json"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(!json_errors(&args));
        let command = parse(args).expect("a value may look like a flag");
        assert!(!command.json);

        let bad: Vec<OsString> = ["read", "--wide", "--json"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(json_errors(&bad), "a usage error still honours the flag");
    }

    #[test]
    fn json_in_the_text_does_not_turn_json_on() {
        let args: Vec<OsString> = ["send", "s1", "--", "--json"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(!wants_json(&args));
        let command = parse(args).expect("text after the dashes");
        assert!(!command.json);
        assert_eq!(sent_text(&command), Some(("--json", false)));

        let flag: Vec<OsString> = ["send", "s1", "x", "--json"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(wants_json(&flag));
    }
}
