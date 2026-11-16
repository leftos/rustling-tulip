//! The followed files that the Tail boxes under a chat's tool cards and the
//! follow tabs read: each followed file of a session, keyed by session and
//! path and held once for all its readers, with the host and request its
//! `TailFile` went out on, whether it still waits for the follow's first
//! (`reset`) chunk, its decoder (ANSI escape sequences stripped, `\r`
//! dropped, UTF-8 decoded lossily across chunk edges), its newest
//! [`MAX_LINES`] lines and the refusal it shows; and its readers, each with
//! whether it follows its bottom. The first reader sends the `TailFile`,
//! the last to go the `UntailFile`.
//!
//! It is plain data: the view sends the messages it returns and draws what
//! it holds.

use std::collections::{BTreeMap, HashMap, VecDeque};

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

/// One follow of a file, held once for every reader of it: the Chat box
/// under a tool card and the follow tabs of the same session and path all
/// read the same lines, each keeping its own following flag.
#[derive(Debug)]
pub struct Tail {
    host: HostKey,
    request_id: String,
    /// Chunks are dropped until the follow's first, which carries `reset`.
    waiting: bool,
    /// Its follow ran on a connection since replaced, or its session
    /// ended; asked again once a reader is ready for it.
    stale: bool,
    decoder: Decoder,
    lines: Lines,
    error: Option<String>,
    /// The Chat box reading it: the tool card it opened under and whether
    /// it follows its bottom.
    card: Option<(TailCard, bool)>,
    /// The follow tabs reading it, by tab id, each with whether it follows
    /// its bottom.
    tabs: BTreeMap<String, bool>,
}

impl Tail {
    fn new(host: HostKey, request_id: String) -> Self {
        Self {
            host,
            request_id,
            waiting: true,
            stale: false,
            decoder: Decoder::default(),
            lines: Lines::default(),
            error: None,
            card: None,
            tabs: BTreeMap::new(),
        }
    }

    /// The tool card the Chat box reading it was opened from.
    #[must_use]
    pub fn card(&self) -> Option<&TailCard> {
        self.card.as_ref().map(|(card, _)| card)
    }

    /// Whether the Chat box follows its bottom: it does until scrolled up,
    /// and again once scrolled back down.
    #[must_use]
    pub fn following(&self) -> bool {
        self.card.as_ref().is_some_and(|(_, following)| *following)
    }

    /// Whether follow tab `tab_id` follows its bottom, by the same rule.
    #[must_use]
    pub fn tab_following(&self, tab_id: &str) -> bool {
        self.tabs.get(tab_id).copied().unwrap_or(false)
    }

    /// What its readers draw.
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

    /// Whether nothing reads it any more.
    fn unread(&self) -> bool {
        self.card.is_none() && self.tabs.is_empty()
    }
}

/// Who reads a follow: a Chat box from its tool card, or a follow tab.
enum Reader<'a> {
    Card(TailCard),
    Tab(&'a str),
}

/// Every followed file, by session id and path.
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

/// Whether `scrolled` of `max_scroll` pixels is at the bottom.
fn at_bottom(scrolled: f32, max_scroll: f32) -> bool {
    scrolled >= max_scroll - BOTTOM_SLACK
}

impl ChatTails {
    /// The tail of `path` in session `session_id`, while its Chat box is
    /// open.
    #[must_use]
    pub fn get(&self, session_id: &str, path: &str) -> Option<&Tail> {
        self.tails
            .get(&key(session_id, path))
            .filter(|tail| tail.card.is_some())
    }

    /// Whether `path` in session `session_id` has its Chat box open.
    #[must_use]
    pub fn is_open(&self, session_id: &str, path: &str) -> bool {
        self.get(session_id, path).is_some()
    }

    /// The follow that follow tab `tab_id` reads, while it reads one.
    #[must_use]
    pub fn tab(&self, tab_id: &str) -> Option<&Tail> {
        self.tails
            .values()
            .find(|tail| tail.tabs.contains_key(tab_id))
    }

    /// The ids of every follow tab reading a follow, sorted.
    #[must_use]
    pub fn tab_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .tails
            .values()
            .flat_map(|tail| tail.tabs.keys().cloned())
            .collect();
        ids.sort();
        ids
    }

    /// Opens the Chat box of `path` in session `session_id` on `host`, from
    /// `card`: the `TailFile` to send under `request_id` when no reader
    /// follows the file yet or its follow was refused (the refusal clears
    /// for every reader); else the box reads the follow as it stands.
    pub fn open(
        &mut self,
        host: HostKey,
        session_id: &str,
        path: &str,
        card: TailCard,
        request_id: String,
    ) -> Option<ClientMessage> {
        self.read(host, (session_id, path), Reader::Card(card), request_id)
    }

    /// Follow tab `tab_id` reads `path` in session `session_id` on `host`:
    /// the `TailFile` to send under `request_id` when no reader follows the
    /// file yet or its follow was refused, as [`Self::open`]; else the tab
    /// reads the follow as it stands.
    pub fn open_tab(
        &mut self,
        host: HostKey,
        session_id: &str,
        path: &str,
        tab_id: &str,
        request_id: String,
    ) -> Option<ClientMessage> {
        self.read(host, (session_id, path), Reader::Tab(tab_id), request_id)
    }

    fn read(
        &mut self,
        host: HostKey,
        (session_id, path): (&str, &str),
        reader: Reader<'_>,
        request_id: String,
    ) -> Option<ClientMessage> {
        let tail = self
            .tails
            .entry(key(session_id, path))
            .or_insert_with(|| Tail::new(host, request_id.clone()));
        // A new follow asks; so does a refused one, whose refusal clears
        // for every reader until the daemon answers again.
        let fresh = tail.card.is_none() && tail.tabs.is_empty();
        let ask = (fresh || tail.error.is_some()).then(|| {
            tail.error = None;
            tail.stale = false;
            tail.waiting = true;
            tail.request_id.clone_from(&request_id);
            ClientMessage::TailFile {
                session_id: session_id.to_owned(),
                path: path.to_owned(),
                request_id: Some(request_id),
            }
        });
        match reader {
            Reader::Card(card) => tail.card = Some((card, true)),
            Reader::Tab(tab_id) => {
                tail.tabs.insert(tab_id.to_owned(), true);
            }
        }
        ask
    }

    /// Closes the Chat box of `path` in session `session_id`: the
    /// `UntailFile` to send, with its host, when it was the follow's last
    /// reader and the daemon follows the file.
    pub fn close(&mut self, session_id: &str, path: &str) -> Option<(HostKey, ClientMessage)> {
        let key = key(session_id, path);
        self.tails.get_mut(&key)?.card = None;
        self.retire(key)
    }

    /// Follow tab `tab_id` stops reading its follow: the `UntailFile` to
    /// send, with its host, when it was the last reader and the daemon
    /// follows the file.
    pub fn close_tab(&mut self, tab_id: &str) -> Option<(HostKey, ClientMessage)> {
        let (key, tail) = self
            .tails
            .iter_mut()
            .find(|(_, tail)| tail.tabs.contains_key(tab_id))?;
        tail.tabs.remove(tab_id);
        let key = key.clone();
        self.retire(key)
    }

    /// The follow of `key`, once nothing reads it, goes: its `UntailFile`
    /// while the daemon follows the file.
    fn retire(&mut self, key: (String, String)) -> Option<(HostKey, ClientMessage)> {
        if !self.tails.get(&key)?.unread() {
            return None;
        }
        let tail = self.tails.remove(&key)?;
        tail.followed().then(|| (tail.host, untail(key)))
    }

    /// Closes every Chat box of session `session_id`: the `UntailFile`s to
    /// send, with their hosts, for the follows no tab reads.
    pub fn close_session(&mut self, session_id: &str) -> Vec<(HostKey, ClientMessage)> {
        self.close_cards_where(|(session, _), _| session == session_id)
    }

    /// Closes every Chat box of session `session_id` whose tool card
    /// `has_card` does not find in its chat: the `UntailFile`s to send.
    pub fn close_missing_cards(
        &mut self,
        session_id: &str,
        has_card: impl Fn(&str) -> bool,
    ) -> Vec<(HostKey, ClientMessage)> {
        self.close_cards_where(|(session, _), card| {
            session == session_id && !has_card(&card.tool_id)
        })
    }

    fn close_cards_where(
        &mut self,
        closes: impl Fn(&(String, String), &TailCard) -> bool,
    ) -> Vec<(HostKey, ClientMessage)> {
        let mut keys: Vec<(String, String)> = self
            .tails
            .iter()
            .filter(|(key, tail)| tail.card().is_some_and(|card| closes(key, card)))
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|key| {
                if let Some(tail) = self.tails.get_mut(&key) {
                    tail.card = None;
                }
                self.retire(key)
            })
            .collect()
    }

    /// Session `session_id` ended: the daemon stopped its follows, so the
    /// ones a tab still reads are no longer followed and keep what they
    /// hold.
    pub fn session_ended(&mut self, session_id: &str) {
        for ((session, _), tail) in &mut self.tails {
            if session == session_id && tail.error.is_none() {
                tail.stale = true;
                tail.waiting = true;
            }
        }
    }

    /// A `FileTail` from `host` for `path` in session `session_id`: a
    /// follow waiting for its first chunk drops it unless it carries
    /// `reset`; `reset` clears the follow, then the data is appended, for
    /// every reader. Returns whether a follow took it.
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

    /// A refusal from `host` answering `request_id`: the follow whose
    /// `TailFile` it answers shows `message` to every reader until it is
    /// asked for again: by a reader opening on it, or by [`Self::resend`]
    /// after a reconnect. Returns whether it answered a follow.
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

    /// `host`'s connection was replaced: its followed files, refused ones
    /// included, drop what arrives until they are asked again
    /// ([`Self::resend`]).
    pub fn reset_host(&mut self, host: &HostKey) {
        for tail in self.tails.values_mut() {
            if tail.host == *host {
                tail.stale = true;
                tail.waiting = true;
            }
        }
    }

    /// The `TailFile`s that ask again for each follow a replaced connection
    /// ran, once one of its readers is ready: its Chat box when
    /// `card_ready` says its session's chat is loaded again, a follow tab
    /// when `tab_ready` says its host lists its session. Each goes under a
    /// fresh request id from `new_id`, once per follow, and each such
    /// follow waits for its first chunk again.
    pub fn resend(
        &mut self,
        card_ready: impl Fn(&str) -> bool,
        tab_ready: impl Fn(&HostKey, &str) -> bool,
        mut new_id: impl FnMut() -> String,
    ) -> Vec<(HostKey, ClientMessage)> {
        let ready = |(session_id, _): &(String, String), tail: &Tail| {
            (tail.card.is_some() && card_ready(session_id))
                || (!tail.tabs.is_empty() && tab_ready(&tail.host, session_id))
        };
        let mut keys: Vec<(String, String)> = self
            .tails
            .iter()
            .filter(|(key, tail)| tail.stale && ready(key, tail))
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
            tail.error = None;
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

    /// The Chat box of `path` in session `session_id` was scrolled to
    /// `scrolled` of `max_scroll` pixels: it follows its bottom only while
    /// at it.
    pub fn on_scrolled(&mut self, session_id: &str, path: &str, scrolled: f32, max_scroll: f32) {
        let card = self
            .tails
            .get_mut(&key(session_id, path))
            .and_then(|tail| tail.card.as_mut());
        if let Some((_, following)) = card {
            *following = at_bottom(scrolled, max_scroll);
        }
    }

    /// Follow tab `tab_id` was scrolled, by the same rule as
    /// [`Self::on_scrolled`].
    pub fn on_tab_scrolled(&mut self, tab_id: &str, scrolled: f32, max_scroll: f32) {
        let following = self
            .tails
            .values_mut()
            .find_map(|tail| tail.tabs.get_mut(tab_id));
        if let Some(following) = following {
            *following = at_bottom(scrolled, max_scroll);
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
        assert!(
            msg.as_ref().is_some_and(|msg| is_tail_file(msg, "r1")),
            "{msg:?}"
        );
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
        let again = tails.resend(|_| true, |_, _| false, || ids.next().unwrap_or_default());
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
    fn a_refusal_shows_its_message_until_asked_again() {
        let mut tails = asked();
        assert!(!tails.on_refused(&HostKey::Local, "other", "no"));
        assert!(tails.on_refused(&HostKey::Local, "r1", "refusing to follow"));
        assert_eq!(shown(&tails), ["<error> refusing to follow"]);
        assert!(
            !chunk(&mut tails, b"x\n", true),
            "a refused box takes no data"
        );
        assert!(
            tails
                .resend(|_| true, |_, _| true, || "r2".to_owned())
                .is_empty(),
            "nothing asks again on its own"
        );
        assert!(tails.close(S, P).is_none(), "nothing to untail");
        assert!(!tails.is_open(S, P));
    }

    #[test]
    fn a_reader_joining_a_refused_follow_asks_again_for_every_reader() {
        let mut tails = ChatTails::default();
        tails.open_tab(HostKey::Local, S, P, "f1", "r1".to_owned());
        assert!(tails.on_refused(&HostKey::Local, "r1", "limit reached"));
        let ask = tails.open(HostKey::Local, S, P, card("t1"), "r2".to_owned());
        assert!(
            ask.as_ref().is_some_and(|msg| is_tail_file(msg, "r2")),
            "{ask:?}"
        );
        assert_eq!(shown(&tails), ["<nothing>"], "the refusal is cleared");
        assert_eq!(tab_shown(&tails, "f1"), ["<nothing>"], "for the tab too");
        assert!(chunk(&mut tails, b"x\n", true));
        assert_eq!(tab_shown(&tails, "f1"), ["x"]);
        assert!(
            !tails.on_refused(&HostKey::Local, "r1", "old"),
            "the old ask"
        );
        assert!(tails.on_refused(&HostKey::Local, "r2", "again"));
        assert_eq!(shown(&tails), ["<error> again"], "a refusal shows again");
        assert_eq!(tab_shown(&tails, "f1"), ["<error> again"]);
    }

    #[test]
    fn a_reconnect_asks_a_refused_follow_again() {
        let mut tails = asked();
        assert!(tails.on_refused(&HostKey::Local, "r1", "limit reached"));
        tails.reset_host(&HostKey::Local);
        assert!(
            tails
                .resend(|_| false, |_, _| false, || "r2".to_owned())
                .is_empty(),
            "not until a reader is ready"
        );
        let again = tails.resend(|_| true, |_, _| false, || "r2".to_owned());
        assert!(
            again.len() == 1 && again.iter().all(|(_, msg)| is_tail_file(msg, "r2")),
            "{again:?}"
        );
        assert_eq!(shown(&tails), ["<nothing>"], "the refusal is cleared");
        assert!(chunk(&mut tails, b"back\n", true));
        assert_eq!(shown(&tails), ["back"]);
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

    /// What follow tab `tab_id` draws, as [`shown`] says it for the box.
    fn tab_shown(tails: &ChatTails, tab_id: &str) -> Vec<String> {
        match tails.tab(tab_id).map(Tail::shows) {
            Some(TailShows::Lines(lines)) => lines.into_iter().map(str::to_owned).collect(),
            Some(TailShows::Nothing) => vec!["<nothing>".to_owned()],
            Some(TailShows::Error(message)) => vec![format!("<error> {message}")],
            None => Vec::new(),
        }
    }

    #[test]
    fn a_second_reader_sends_no_tail_file_and_sees_the_lines_so_far() {
        let mut tails = opened();
        chunk(&mut tails, b"one\ntwo\n", false);
        let ask = tails.open_tab(HostKey::Local, S, P, "f1", "r2".to_owned());
        assert!(ask.is_none(), "the follow is reused: {ask:?}");
        assert_eq!(tab_shown(&tails, "f1"), ["one", "two"]);
        assert_eq!(shown(&tails), ["one", "two"]);
    }

    #[test]
    fn a_box_opened_beside_a_tab_sends_no_tail_file() {
        let mut tails = ChatTails::default();
        let first = tails.open_tab(HostKey::Local, S, P, "f1", "r1".to_owned());
        assert!(
            first.as_ref().is_some_and(|msg| is_tail_file(msg, "r1")),
            "{first:?}"
        );
        assert!(tails.tab("f1").is_some());
        assert!(!tails.is_open(S, P), "no box yet");
        let second = tails.open(HostKey::Local, S, P, card("t1"), "r2".to_owned());
        assert!(second.is_none(), "{second:?}");
        assert!(tails.is_open(S, P));
        assert_eq!(tails.tab_ids(), ["f1"]);
    }

    #[test]
    fn only_the_last_reader_to_close_untails() {
        let mut tails = opened();
        tails.open_tab(HostKey::Local, S, P, "f1", "r2".to_owned());
        tails.open_tab(HostKey::Local, S, P, "f2", "r3".to_owned());
        assert!(tails.close(S, P).is_none(), "the tabs still read it");
        assert!(!tails.is_open(S, P), "the box is closed");
        assert!(tails.close_tab("f1").is_none(), "f2 still reads it");
        let last = tails.close_tab("f2");
        assert!(
            last.is_some_and(|(host, msg)| host == HostKey::Local && is_untail(&msg)),
            "the last reader stops the follow"
        );
        assert_eq!(tails.tab_ids(), Vec::<String>::new());
        assert!(tails.close_tab("f2").is_none(), "nothing left to close");
    }

    #[test]
    fn closing_a_session_s_boxes_keeps_a_tab_s_follow() {
        let mut tails = opened();
        tails.open_tab(HostKey::Local, S, P, "f1", "r2".to_owned());
        assert!(tails.close_session(S).is_empty(), "the tab still reads it");
        assert!(tails.close_missing_cards(S, |_| false).is_empty());
        assert!(!tails.is_open(S, P));
        assert!(chunk(&mut tails, b"still\n", false), "the follow runs on");
        assert_eq!(tab_shown(&tails, "f1"), ["still"]);
    }

    #[test]
    fn a_file_tail_feeds_every_reader() {
        let mut tails = asked();
        tails.open_tab(HostKey::Local, S, P, "f1", "r2".to_owned());
        tails.open_tab(HostKey::Local, S, P, "f2", "r3".to_owned());
        assert!(chunk(&mut tails, b"a\nb\n", true));
        assert!(chunk(&mut tails, b"c\n", false));
        assert_eq!(shown(&tails), ["a", "b", "c"]);
        assert_eq!(tab_shown(&tails, "f1"), ["a", "b", "c"]);
        assert_eq!(tab_shown(&tails, "f2"), ["a", "b", "c"]);
        assert!(chunk(&mut tails, b"d\n", true));
        assert_eq!(
            tab_shown(&tails, "f1"),
            ["d"],
            "a reset replaces them for all"
        );
    }

    #[test]
    fn a_refusal_reaches_every_reader() {
        let mut tails = asked();
        tails.open_tab(HostKey::Local, S, P, "f1", "r2".to_owned());
        assert!(
            !tails.on_refused(&HostKey::Local, "r2", "no"),
            "only r1 went out"
        );
        assert!(tails.on_refused(&HostKey::Local, "r1", "limit reached"));
        assert_eq!(shown(&tails), ["<error> limit reached"]);
        assert_eq!(tab_shown(&tails, "f1"), ["<error> limit reached"]);
        assert!(tails.close(S, P).is_none());
        assert!(
            tails.close_tab("f1").is_none(),
            "a refused follow has nothing to untail"
        );
    }

    #[test]
    fn each_reader_follows_on_its_own() {
        let mut tails = opened();
        tails.open_tab(HostKey::Local, S, P, "f1", "r2".to_owned());
        tails.open_tab(HostKey::Local, S, P, "f2", "r3".to_owned());
        tails.on_tab_scrolled("f1", 10.0, 300.0);
        let tail = tails.tab("f1");
        assert!(
            tail.is_some_and(|tail| !tail.tab_following("f1")),
            "f1 stopped"
        );
        assert!(
            tail.is_some_and(|tail| tail.tab_following("f2")),
            "f2 follows"
        );
        assert!(
            tails.get(S, P).is_some_and(Tail::following),
            "the box follows"
        );
        tails.on_scrolled(S, P, 0.0, 300.0);
        tails.on_tab_scrolled("f1", 300.0, 300.0);
        let tail = tails.tab("f1");
        assert!(
            tail.is_some_and(|tail| tail.tab_following("f1")),
            "f1 is back"
        );
        assert!(
            !tails.get(S, P).is_some_and(Tail::following),
            "the box stopped"
        );
    }

    #[test]
    fn a_tab_s_follow_is_asked_again_once_its_session_is_listed() {
        let mut tails = ChatTails::default();
        tails.open_tab(HostKey::Local, S, P, "f1", "r1".to_owned());
        tails.reset_host(&HostKey::Local);
        assert!(
            tails
                .resend(|_| true, |_, _| false, || "r2".to_owned())
                .is_empty(),
            "not listed yet"
        );
        let again = tails.resend(
            |_| false,
            |host, id| *host == HostKey::Local && id == S,
            || "r2".to_owned(),
        );
        assert!(
            again.len() == 1 && again.iter().all(|(_, msg)| is_tail_file(msg, "r2")),
            "{again:?}"
        );
        assert!(
            tails
                .resend(|_| true, |_, _| true, || "r3".to_owned())
                .is_empty(),
            "once"
        );
    }

    #[test]
    fn an_ended_session_s_follow_is_closed_without_an_untail() {
        let mut tails = ChatTails::default();
        tails.open_tab(HostKey::Local, S, P, "f1", "r1".to_owned());
        assert!(chunk(&mut tails, b"last\n", true));
        tails.session_ended(S);
        assert!(
            !chunk(&mut tails, b"late\n", true),
            "the follow has stopped"
        );
        assert_eq!(tab_shown(&tails, "f1"), ["last"]);
        assert!(tails.close_tab("f1").is_none());
    }
}
