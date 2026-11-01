//! The Tail boxes under a chat's tool cards: each followed file of a
//! session, keyed by session and path, with the host and request its
//! `TailFile` went out on, whether it still waits for the follow's first
//! (`reset`) chunk, its decoder (ANSI escape sequences stripped, `\r`
//! dropped, UTF-8 decoded lossily across chunk edges), its newest
//! [`MAX_LINES`] lines, whether it follows its bottom and the refusal it
//! shows.
//!
//! It is plain data: the view sends the messages it returns and draws what
//! it holds.

use std::collections::{HashMap, VecDeque};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use protocol::ClientMessage;

use crate::target::HostKey;

/// How many lines a Tail box keeps; older ones drop off its top.
pub const MAX_LINES: usize = 2000;

/// What a Tail box says before its file sent any data.
pub const NO_OUTPUT: &str = "No output yet";

/// How near its bottom, in pixels, a Tail box still counts as at it.
const BOTTOM_SLACK: f32 = 1.0;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// The tool card a tail was opened from: the tool call's id and the path's
/// place in its `tail_paths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailCard {
    pub tool_id: String,
    pub index: usize,
}

/// What a Tail box draws.
#[derive(Debug, PartialEq, Eq)]
pub enum TailShows<'a> {
    /// No data yet: the box says [`NO_OUTPUT`].
    Nothing,
    /// The file's lines, oldest first.
    Lines(Vec<&'a str>),
    /// The daemon's refusal.
    Error(&'a str),
}

/// Where the escape-sequence filter stands between two bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Escape {
    /// Plain text.
    #[default]
    Text,
    /// Just after an ESC.
    Started,
    /// In an ESC sequence's intermediate bytes.
    Intermediate,
    /// In a CSI sequence (`ESC [`), until its final byte.
    Csi,
    /// In a string sequence (OSC, DCS, SOS, PM, APC), until BEL or ST.
    Str,
    /// An ESC inside a string sequence: a `\` after it ends the string.
    StrEsc,
}

impl Escape {
    /// Where the filter stands after the byte that follows an ESC.
    fn after_esc(byte: u8) -> Self {
        match byte {
            b'[' => Self::Csi,
            b']' | b'P' | b'X' | b'^' | b'_' => Self::Str,
            ESC => Self::Started,
            0x20..=0x2f => Self::Intermediate,
            _ => Self::Text,
        }
    }

    /// Where the filter stands after `byte`: back at text after plain text,
    /// a CSI's final byte, an ESC sequence's final byte, or a string's BEL
    /// or ST.
    fn next(self, byte: u8) -> Self {
        match (self, byte) {
            (Self::Text | Self::Intermediate | Self::Csi, ESC) => Self::Started,
            (Self::Started, _) | (Self::StrEsc, 0x00..=0x5b | 0x5d..=0xff) => Self::after_esc(byte),
            (Self::Intermediate, 0x20..=0x2f) => Self::Intermediate,
            (Self::Csi, 0x00..=0x3f | 0x7f..=0xff) => Self::Csi,
            (Self::Str, ESC) => Self::StrEsc,
            (Self::Str, _) if byte != BEL => Self::Str,
            _ => Self::Text,
        }
    }
}

/// Turns one follow's chunks into text: escape sequences and `\r` out,
/// UTF-8 decoded lossily, a sequence or a character split between two
/// chunks carried to the next.
#[derive(Debug, Default)]
struct Decoder {
    escape: Escape,
    /// The start of a UTF-8 character the last chunk ended inside.
    partial: Vec<u8>,
}

impl Decoder {
    fn decode(&mut self, bytes: &[u8]) -> String {
        let mut kept = std::mem::take(&mut self.partial);
        for &byte in bytes {
            let text = self.escape == Escape::Text && byte != ESC && byte != b'\r';
            self.escape = self.escape.next(byte);
            if text {
                kept.push(byte);
            }
        }
        self.utf8(&kept)
    }

    /// `bytes` as text, each invalid sequence a U+FFFD; a character cut
    /// off at the end is kept for the next chunk.
    fn utf8(&mut self, bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        let mut rest = bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    return out;
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    out.push_str(&String::from_utf8_lossy(valid));
                    let Some(bad) = err.error_len() else {
                        self.partial = after.to_vec();
                        return out;
                    };
                    out.push(char::REPLACEMENT_CHARACTER);
                    rest = after.get(bad..).unwrap_or_default();
                }
            }
        }
    }
}

/// A box's lines, the last one still open for more text, at most
/// [`MAX_LINES`] of them besides an empty open line.
#[derive(Debug, Default)]
struct Lines {
    lines: VecDeque<String>,
}

impl Lines {
    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            match self.lines.back_mut() {
                Some(open) => open.push_str(first),
                None => self.lines.push_back(first.to_owned()),
            }
        }
        self.lines.extend(parts.map(str::to_owned));
        let open_empty = self.lines.back().is_some_and(String::is_empty);
        let cap = MAX_LINES + usize::from(open_empty);
        let over = self.lines.len().saturating_sub(cap);
        self.lines.drain(..over);
    }

    /// The lines drawn: all but an empty open line.
    fn drawn(&self) -> Vec<&str> {
        let open_empty = self.lines.back().is_some_and(String::is_empty);
        let count = self.lines.len() - usize::from(open_empty);
        self.lines.iter().take(count).map(String::as_str).collect()
    }
}

/// One open Tail box.
#[derive(Debug)]
pub struct Tail {
    host: HostKey,
    card: TailCard,
    request_id: String,
    /// Chunks are dropped until the follow's first, which carries `reset`.
    waiting: bool,
    /// Its follow ran on a connection since replaced; asked again once its
    /// chat loads there.
    stale: bool,
    decoder: Decoder,
    lines: Lines,
    following: bool,
    error: Option<String>,
}

impl Tail {
    /// The tool card the tail was opened from.
    #[must_use]
    pub fn card(&self) -> &TailCard {
        &self.card
    }

    /// Whether the box follows its bottom: it does until scrolled up, and
    /// again once scrolled back down.
    #[must_use]
    pub fn following(&self) -> bool {
        self.following
    }

    /// What the box draws.
    #[must_use]
    pub fn shows(&self) -> TailShows<'_> {
        if let Some(error) = &self.error {
            return TailShows::Error(error);
        }
        let lines = self.lines.drawn();
        if lines.is_empty() {
            TailShows::Nothing
        } else {
            TailShows::Lines(lines)
        }
    }

    /// Whether the daemon follows the file for it now.
    fn followed(&self) -> bool {
        !self.stale && self.error.is_none()
    }
}

/// Every open Tail box, by session id and path.
#[derive(Debug, Default)]
pub struct ChatTails {
    tails: HashMap<(String, String), Tail>,
}

fn key(session_id: &str, path: &str) -> (String, String) {
    (session_id.to_owned(), path.to_owned())
}

fn untail((session_id, path): (String, String)) -> ClientMessage {
    ClientMessage::UntailFile { session_id, path }
}

impl ChatTails {
    /// The tail of `path` in session `session_id`, while its box is open.
    #[must_use]
    pub fn get(&self, session_id: &str, path: &str) -> Option<&Tail> {
        self.tails.get(&key(session_id, path))
    }

    /// Whether `path` in session `session_id` has its box open.
    #[must_use]
    pub fn is_open(&self, session_id: &str, path: &str) -> bool {
        self.get(session_id, path).is_some()
    }

    /// Opens the box of `path` in session `session_id` on `host`, from
    /// `card`, showing nothing yet: returns the `TailFile` to send under
    /// `request_id`.
    pub fn open(
        &mut self,
        host: HostKey,
        session_id: &str,
        path: &str,
        card: TailCard,
        request_id: String,
    ) -> ClientMessage {
        let tail = Tail {
            host,
            card,
            request_id: request_id.clone(),
            waiting: true,
            stale: false,
            decoder: Decoder::default(),
            lines: Lines::default(),
            following: true,
            error: None,
        };
        self.tails.insert(key(session_id, path), tail);
        ClientMessage::TailFile {
            session_id: session_id.to_owned(),
            path: path.to_owned(),
            request_id: Some(request_id),
        }
    }

    /// Closes the box of `path` in session `session_id`: the `UntailFile`
    /// to send, with its host, while the daemon follows the file.
    pub fn close(&mut self, session_id: &str, path: &str) -> Option<(HostKey, ClientMessage)> {
        let key = key(session_id, path);
        let tail = self.tails.remove(&key)?;
        tail.followed().then(|| (tail.host, untail(key)))
    }

    /// Closes every box of session `session_id`: the `UntailFile`s to
    /// send, with their hosts.
    pub fn close_session(&mut self, session_id: &str) -> Vec<(HostKey, ClientMessage)> {
        self.close_where(|(session, _), _| session == session_id)
    }

    /// Closes every box of session `session_id` whose tool card `has_card`
    /// does not find in its chat: the `UntailFile`s to send.
    pub fn close_missing_cards(
        &mut self,
        session_id: &str,
        has_card: impl Fn(&str) -> bool,
    ) -> Vec<(HostKey, ClientMessage)> {
        self.close_where(|(session, _), tail| {
            session == session_id && !has_card(&tail.card.tool_id)
        })
    }

    fn close_where(
        &mut self,
        closes: impl Fn(&(String, String), &Tail) -> bool,
    ) -> Vec<(HostKey, ClientMessage)> {
        let mut keys: Vec<(String, String)> = self
            .tails
            .iter()
            .filter(|(key, tail)| closes(key, tail))
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|key| {
                let tail = self.tails.remove(&key)?;
                tail.followed().then(|| (tail.host, untail(key)))
            })
            .collect()
    }

    /// A `FileTail` from `host` for `path` in session `session_id`: a box
    /// waiting for its follow's first chunk drops it unless it carries
    /// `reset`; `reset` clears the box, then the data is appended. Returns
    /// whether a box took it.
    pub fn on_file_tail(
        &mut self,
        host: &HostKey,
        session_id: &str,
        path: &str,
        data_b64: &str,
        reset: bool,
    ) -> bool {
        let Some(tail) = self.tails.get_mut(&key(session_id, path)) else {
            return false;
        };
        if tail.host != *host || !tail.followed() || (tail.waiting && !reset) {
            return false;
        }
        let bytes = match B64.decode(data_b64) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(%session_id, path, error = %err, "a FileTail whose data is not base64: dropped");
                return false;
            }
        };
        if reset {
            tail.waiting = false;
            tail.decoder = Decoder::default();
            tail.lines = Lines::default();
        }
        let text = tail.decoder.decode(&bytes);
        tail.lines.push(&text);
        true
    }

    /// A refusal from `host` answering `request_id`: the box whose
    /// `TailFile` it answers shows `message` until closed, and is never
    /// asked for again. Returns whether it answered a box.
    pub fn on_refused(&mut self, host: &HostKey, request_id: &str, message: &str) -> bool {
        let refused = self
            .tails
            .values_mut()
            .find(|tail| tail.host == *host && tail.followed() && tail.request_id == request_id);
        let Some(tail) = refused else {
            return false;
        };
        tail.error = Some(message.to_owned());
        true
    }

    /// `host`'s connection was replaced: its followed boxes drop what
    /// arrives until they are asked again ([`Self::resend`]).
    pub fn reset_host(&mut self, host: &HostKey) {
        for tail in self.tails.values_mut() {
            if tail.host == *host && tail.error.is_none() {
                tail.stale = true;
                tail.waiting = true;
            }
        }
    }

    /// The `TailFile`s that ask again for each box a replaced connection
    /// followed, once `loaded` says its session's chat is loaded again,
    /// each under a fresh request id from `new_id`; each such box waits for
    /// its follow's first chunk again.
    pub fn resend(
        &mut self,
        loaded: impl Fn(&str) -> bool,
        mut new_id: impl FnMut() -> String,
    ) -> Vec<(HostKey, ClientMessage)> {
        let mut keys: Vec<(String, String)> = self
            .tails
            .iter()
            .filter(|((session_id, _), tail)| tail.stale && loaded(session_id))
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort();
        let mut sends = Vec::with_capacity(keys.len());
        for (session_id, path) in keys {
            let Some(tail) = self.tails.get_mut(&(session_id.clone(), path.clone())) else {
                continue;
            };
            let request_id = new_id();
            tail.stale = false;
            tail.waiting = true;
            tail.request_id.clone_from(&request_id);
            let msg = ClientMessage::TailFile {
                session_id,
                path,
                request_id: Some(request_id),
            };
            sends.push((tail.host.clone(), msg));
        }
        sends
    }

    /// The box of `path` in session `session_id` was scrolled to
    /// `scrolled` of `max_scroll` pixels: it follows its bottom only while
    /// at it.
    pub fn on_scrolled(&mut self, session_id: &str, path: &str, scrolled: f32, max_scroll: f32) {
        if let Some(tail) = self.tails.get_mut(&key(session_id, path)) {
            tail.following = scrolled >= max_scroll - BOTTOM_SLACK;
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    use protocol::ClientMessage;

    use super::{ChatTails, MAX_LINES, Tail, TailCard, TailShows};
    use crate::target::HostKey;

    const S: &str = "s1";
    const P: &str = "C:/work/build.log";

    fn card(tool_id: &str) -> TailCard {
        TailCard {
            tool_id: tool_id.to_owned(),
            index: 0,
        }
    }

    /// A tail of `P` from card `t1`, asked under `r1`.
    fn asked() -> ChatTails {
        let mut tails = ChatTails::default();
        tails.open(HostKey::Local, S, P, card("t1"), "r1".to_owned());
        tails
    }

    /// A tail of `P` whose follow's first chunk (empty, `reset`) arrived.
    fn opened() -> ChatTails {
        let mut tails = asked();
        assert!(chunk(&mut tails, b"", true), "the first chunk is taken");
        tails
    }

    fn chunk(tails: &mut ChatTails, bytes: &[u8], reset: bool) -> bool {
        tails.on_file_tail(&HostKey::Local, S, P, &B64.encode(bytes), reset)
    }

    /// What the box draws, one string per line; a placeholder line for no
    /// data and for a refusal, none for a closed box.
    fn shown(tails: &ChatTails) -> Vec<String> {
        match tails.get(S, P).map(Tail::shows) {
            Some(TailShows::Lines(lines)) => lines.into_iter().map(str::to_owned).collect(),
            Some(TailShows::Nothing) => vec!["<nothing>".to_owned()],
            Some(TailShows::Error(message)) => vec![format!("<error> {message}")],
            None => Vec::new(),
        }
    }

    fn is_tail_file(msg: &ClientMessage, request: &str) -> bool {
        matches!(
            msg,
            ClientMessage::TailFile { session_id, path, request_id }
                if session_id == S && path == P && request_id.as_deref() == Some(request)
        )
    }

    fn is_untail(msg: &ClientMessage) -> bool {
        matches!(
            msg,
            ClientMessage::UntailFile { session_id, path } if session_id == S && path == P
        )
    }

    #[test]
    fn opening_asks_for_the_file_and_shows_nothing_yet() {
        let mut tails = ChatTails::default();
        let msg = tails.open(HostKey::Local, S, P, card("t1"), "r1".to_owned());
        assert!(is_tail_file(&msg, "r1"), "{msg:?}");
        assert_eq!(shown(&tails), ["<nothing>"]);
        assert!(tails.get(S, P).is_some_and(Tail::following));
    }

    #[test]
    fn strips_ansi_including_a_csi_split_across_two_chunks() {
        let mut tails = opened();
        assert!(chunk(&mut tails, b"\x1b[1;3", false));
        assert!(chunk(&mut tails, b"1mred\x1b[0m plain\n", false));
        assert_eq!(shown(&tails), ["red plain"]);
    }

    #[test]
    fn strips_osc_and_other_escape_sequences() {
        let mut tails = opened();
        chunk(
            &mut tails,
            b"\x1b]0;title\x07a\x1b]8;;http://x\x1b\\b\x1b(Bc\x1b7d\n",
            false,
        );
        chunk(&mut tails, b"\x1b]2;split", false);
        chunk(&mut tails, b" title\x1b", false);
        chunk(&mut tails, b"\\e\n", false);
        assert_eq!(shown(&tails), ["abcd", "e"]);
    }

    #[test]
    fn joins_a_utf8_character_split_across_two_chunks() {
        let mut tails = opened();
        chunk(&mut tails, b"caf\xc3", false);
        chunk(&mut tails, b"\xa9 \xe2\x9c", false);
        chunk(&mut tails, b"\x93\n", false);
        assert_eq!(shown(&tails), ["caf\u{e9} \u{2713}"]);
    }

    #[test]
    fn invalid_utf8_decodes_lossily() {
        let mut tails = opened();
        chunk(&mut tails, b"a\xffb\xc3\n", false);
        assert_eq!(shown(&tails), ["a\u{fffd}b\u{fffd}"]);
    }

    #[test]
    fn crlf_and_lf_break_lines_and_a_lone_cr_is_dropped() {
        let mut tails = opened();
        chunk(&mut tails, b"one\r\ntwo\nthr\ree\r", false);
        chunk(&mut tails, b"\nfour", false);
        assert_eq!(shown(&tails), ["one", "two", "three", "four"]);
    }

    #[test]
    fn an_unfinished_line_is_drawn_and_later_completed() {
        let mut tails = opened();
        chunk(&mut tails, b"half", false);
        assert_eq!(shown(&tails), ["half"]);
        chunk(&mut tails, b" done\nnext\n", false);
        assert_eq!(shown(&tails), ["half done", "next"]);
    }

    #[test]
    fn keeps_at_most_2000_lines() {
        let mut tails = opened();
        let lines: Vec<String> = (0..2500).map(|n| format!("line {n}")).collect();
        let text = lines.join("\n") + "\n";
        chunk(&mut tails, text.as_bytes(), false);
        let lines = shown(&tails);
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines.first().map(String::as_str), Some("line 500"));
        assert_eq!(lines.last().map(String::as_str), Some("line 2499"));
    }

    #[test]
    fn chunks_before_the_reset_are_dropped_after_a_start_and_a_restart() {
        let mut tails = asked();
        assert!(
            !chunk(&mut tails, b"queued\n", false),
            "dropped before reset"
        );
        assert_eq!(shown(&tails), ["<nothing>"]);
        assert!(chunk(&mut tails, b"first\n", true));
        assert_eq!(shown(&tails), ["first"]);
        tails.reset_host(&HostKey::Local);
        assert!(!chunk(&mut tails, b"late\n", false), "the old connection's");
        let mut ids = ["r2".to_owned()].into_iter();
        let again = tails.resend(|_| true, || ids.next().unwrap_or_default());
        assert_eq!(again.len(), 1, "{again:?}");
        assert!(
            again
                .iter()
                .all(|(host, msg)| *host == HostKey::Local && is_tail_file(msg, "r2"))
        );
        assert!(
            !chunk(&mut tails, b"stale\n", false),
            "dropped after a restart"
        );
        assert!(chunk(&mut tails, b"fresh\n", true));
        assert_eq!(shown(&tails), ["fresh"]);
    }

    #[test]
    fn a_reset_clears_the_box_then_appends() {
        let mut tails = opened();
        chunk(&mut tails, b"a\nb\n", false);
        assert!(chunk(&mut tails, b"c\n", true));
        assert_eq!(shown(&tails), ["c"]);
        assert!(chunk(&mut tails, b"", true));
        assert_eq!(shown(&tails), ["<nothing>"], "an empty reset empties it");
    }

    #[test]
    fn a_reset_drops_an_escape_left_open() {
        let mut tails = opened();
        chunk(&mut tails, b"x\x1b[3", true);
        chunk(&mut tails, b"y\n", true);
        assert_eq!(shown(&tails), ["y"]);
    }

    #[test]
    fn following_stops_off_the_bottom_and_resumes_at_it() {
        let mut tails = opened();
        let following = |tails: &ChatTails| tails.get(S, P).is_some_and(Tail::following);
        assert!(following(&tails), "a new box follows");
        tails.on_scrolled(S, P, 50.0, 200.0);
        assert!(!following(&tails), "scrolled up, it stops");
        tails.on_scrolled(S, P, 199.5, 200.0);
        assert!(following(&tails), "back at the bottom, it follows");
    }

    #[test]
    fn a_refusal_shows_its_message_and_is_never_asked_again() {
        let mut tails = asked();
        assert!(!tails.on_refused(&HostKey::Local, "other", "no"));
        assert!(tails.on_refused(&HostKey::Local, "r1", "refusing to follow"));
        assert_eq!(shown(&tails), ["<error> refusing to follow"]);
        assert!(
            !chunk(&mut tails, b"x\n", true),
            "a refused box takes no data"
        );
        tails.reset_host(&HostKey::Local);
        assert!(tails.resend(|_| true, || "r2".to_owned()).is_empty());
        assert!(tails.close(S, P).is_none(), "nothing to untail");
        assert!(!tails.is_open(S, P));
    }

    #[test]
    fn closing_a_followed_box_untails_it() {
        let mut tails = opened();
        let closed = tails.close(S, P);
        assert!(
            closed.is_some_and(|(host, msg)| host == HostKey::Local && is_untail(&msg)),
            "the follow stops"
        );
        assert!(!tails.is_open(S, P));
    }

    #[test]
    fn a_box_on_a_replaced_connection_is_closed_without_an_untail() {
        let mut tails = opened();
        tails.reset_host(&HostKey::Local);
        assert!(tails.close(S, P).is_none());
    }

    #[test]
    fn a_box_closes_when_its_card_leaves_the_chat() {
        let mut tails = opened();
        assert!(tails.close_missing_cards(S, |id| id == "t1").is_empty());
        let closed = tails.close_missing_cards(S, |id| id == "t2");
        assert!(closed.len() == 1 && closed.iter().all(|(_, msg)| is_untail(msg)));
        assert!(!tails.is_open(S, P));
    }

    #[test]
    fn closing_a_session_closes_its_boxes_only() {
        let mut tails = opened();
        tails.open(HostKey::Local, "s2", P, card("t9"), "r9".to_owned());
        let closed = tails.close_session(S);
        assert!(closed.len() == 1 && closed.iter().all(|(_, msg)| is_untail(msg)));
        assert!(!tails.is_open(S, P));
        assert!(tails.is_open("s2", P));
    }

    #[test]
    fn another_hosts_chunk_is_not_taken() {
        let mut tails = opened();
        let remote = HostKey::Remote("box".to_owned());
        assert!(!tails.on_file_tail(&remote, S, P, &B64.encode(b"x\n"), true));
        assert!(!tails.on_refused(&remote, "r1", "no"));
        assert_eq!(shown(&tails), ["<nothing>"]);
    }
}
