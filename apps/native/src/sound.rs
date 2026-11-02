//! The toast sounds: the sounds bundled with the client, which clip a choice
//! plays, the default each kind of toast falls back to when a custom file is
//! missing, oversized or undecodable, and the sounder that plays a clip on
//! this PC's default output device, off the UI thread, one at a time with at
//! most one waiting.

use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

use crate::sidebar::ToastSound;

/// The file types the Custom choice's picker offers.
pub(crate) const SOUND_EXTENSIONS: [&str; 2] = ["wav", "ogg"];

/// The largest custom file the client reads: 5 MiB.
const MAX_CUSTOM_BYTES: u64 = 5 * 1024 * 1024;

/// A sound's decoder, over the whole encoded file held in memory.
type Sound = rodio::Decoder<Cursor<Arc<[u8]>>>;

/// A sound bundled with the client (`assets/sounds/README.md`), saved by its
/// [`Self::id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundledSound {
    MalletRisingFifth,
    MalletFallingFifth,
    MalletRisingThird,
    BellRisingFifth,
    BellFallingFifth,
    Question1,
    Question2,
    Question3,
    Confirm1,
    Confirm2,
    Confirm3,
    GlassBright,
    GlassShort,
    Bong,
    ClassicRising,
    ClassicFalling,
}

impl BundledSound {
    /// Every bundled sound, in the order Settings lists them.
    pub(crate) const ALL: [Self; 16] = [
        Self::MalletRisingFifth,
        Self::MalletFallingFifth,
        Self::MalletRisingThird,
        Self::BellRisingFifth,
        Self::BellFallingFifth,
        Self::Question1,
        Self::Question2,
        Self::Question3,
        Self::Confirm1,
        Self::Confirm2,
        Self::Confirm3,
        Self::GlassBright,
        Self::GlassShort,
        Self::Bong,
        Self::ClassicRising,
        Self::ClassicFalling,
    ];

    /// The sound's saved id.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::MalletRisingFifth => "mallet_rising_fifth",
            Self::MalletFallingFifth => "mallet_falling_fifth",
            Self::MalletRisingThird => "mallet_rising_third",
            Self::BellRisingFifth => "bell_rising_fifth",
            Self::BellFallingFifth => "bell_falling_fifth",
            Self::Question1 => "question1",
            Self::Question2 => "question2",
            Self::Question3 => "question3",
            Self::Confirm1 => "confirm1",
            Self::Confirm2 => "confirm2",
            Self::Confirm3 => "confirm3",
            Self::GlassBright => "glass_bright",
            Self::GlassShort => "glass_short",
            Self::Bong => "bong",
            Self::ClassicRising => "classic_rising",
            Self::ClassicFalling => "classic_falling",
        }
    }

    /// The sound's name in Settings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MalletRisingFifth => "Mallet, rising fifth",
            Self::MalletFallingFifth => "Mallet, falling fifth",
            Self::MalletRisingThird => "Mallet, rising third",
            Self::BellRisingFifth => "Bell, rising fifth",
            Self::BellFallingFifth => "Bell, falling fifth",
            Self::Question1 => "Question 1",
            Self::Question2 => "Question 2",
            Self::Question3 => "Question 3",
            Self::Confirm1 => "Confirm 1",
            Self::Confirm2 => "Confirm 2",
            Self::Confirm3 => "Confirm 3",
            Self::GlassBright => "Glass, bright",
            Self::GlassShort => "Glass, short",
            Self::Bong => "Bong",
            Self::ClassicRising => "Classic, rising",
            Self::ClassicFalling => "Classic, falling",
        }
    }

    /// The sound's encoded file.
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::MalletRisingFifth => include_bytes!("../assets/sounds/mallet-rising-fifth.ogg"),
            Self::MalletFallingFifth => include_bytes!("../assets/sounds/mallet-falling-fifth.ogg"),
            Self::MalletRisingThird => include_bytes!("../assets/sounds/mallet-rising-third.ogg"),
            Self::BellRisingFifth => include_bytes!("../assets/sounds/bell-rising-fifth.ogg"),
            Self::BellFallingFifth => include_bytes!("../assets/sounds/bell-falling-fifth.ogg"),
            Self::Question1 => include_bytes!("../assets/sounds/question-1.ogg"),
            Self::Question2 => include_bytes!("../assets/sounds/question-2.ogg"),
            Self::Question3 => include_bytes!("../assets/sounds/question-3.ogg"),
            Self::Confirm1 => include_bytes!("../assets/sounds/confirm-1.ogg"),
            Self::Confirm2 => include_bytes!("../assets/sounds/confirm-2.ogg"),
            Self::Confirm3 => include_bytes!("../assets/sounds/confirm-3.ogg"),
            Self::GlassBright => include_bytes!("../assets/sounds/glass-bright.ogg"),
            Self::GlassShort => include_bytes!("../assets/sounds/glass-short.ogg"),
            Self::Bong => include_bytes!("../assets/sounds/bong.ogg"),
            Self::ClassicRising => include_bytes!("../assets/sounds/classic-rising.ogg"),
            Self::ClassicFalling => include_bytes!("../assets/sounds/classic-falling.ogg"),
        }
    }
}

/// Which kind of toast a sound plays for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    /// A session waiting for the owner.
    NeedsYou,
    /// A turn that ended.
    Finished,
}

impl NotifyKind {
    /// The kind's name in a log line.
    const fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Finished => "Finished",
        }
    }

    /// The sound the kind's Default choice plays.
    #[must_use]
    pub const fn default_sound(self) -> BundledSound {
        match self {
            Self::NeedsYou => BundledSound::MalletRisingFifth,
            Self::Finished => BundledSound::MalletFallingFifth,
        }
    }
}

/// Where a clip's sound came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipSource {
    /// This sound bundled with the client.
    Bundled(BundledSound),
    /// The custom file at this path.
    File(PathBuf),
}

/// A sound ready to play: decoded once, when it was read, and played from
/// that decoder.
pub struct Clip {
    pub source: ClipSource,
    /// Which kind of toast it plays for.
    kind: NotifyKind,
    sound: Sound,
}

impl Clip {
    /// `bundled` to play for `kind`; `None`, with a warning, should it not
    /// decode.
    fn bundled(bundled: BundledSound, kind: NotifyKind) -> Option<Self> {
        match decode(Arc::from(bundled.bytes())) {
            Ok(sound) => Some(Self {
                source: ClipSource::Bundled(bundled),
                kind,
                sound,
            }),
            Err(problem) => {
                tracing::warn!("the bundled sound {} {problem}", bundled.label());
                None
            }
        }
    }
}

/// Plays sounds.
pub trait Sounder: Send + Sync {
    /// Starts `clip` playing, or keeps it waiting while another plays, and
    /// returns at once; a clip that would wait behind one already waiting
    /// is dropped. Called off the UI thread; a failure is logged, never
    /// returned.
    fn play(&self, clip: Clip);

    /// Drops the clip waiting to play when it is `kind`'s, if any; the one
    /// playing plays on.
    fn drop_waiting(&self, kind: NotifyKind);
}

/// The clip `choice` plays for `kind`: none when it is off; the bundled sound
/// it names; the custom file when it reads and decodes, else `kind`'s default
/// sound with a warning naming the path. Reads the file, so it runs off the UI
/// thread.
#[must_use]
pub(crate) fn clip_for(choice: &ToastSound, kind: NotifyKind) -> Option<Clip> {
    match choice {
        ToastSound::Off => None,
        ToastSound::Default => Clip::bundled(kind.default_sound(), kind),
        ToastSound::Bundled(sound) => Clip::bundled(*sound, kind),
        ToastSound::Custom(path) => match read_sound(path) {
            Ok(sound) => Some(Clip {
                source: ClipSource::File(path.clone()),
                kind,
                sound,
            }),
            Err(problem) => {
                tracing::warn!(
                    "the {} sound {} {problem}; playing the default sound",
                    kind.label(),
                    path.display()
                );
                Clip::bundled(kind.default_sound(), kind)
            }
        },
    }
}

/// What is wrong with `choice`'s custom file, worded for Settings; `None`
/// when the choice plays as chosen. Reads the file, so it runs off the UI
/// thread.
#[must_use]
pub(crate) fn problem_of(choice: &ToastSound) -> Option<String> {
    let ToastSound::Custom(path) = choice else {
        return None;
    };
    read_sound(path)
        .err()
        .map(|problem| format!("This file {problem}; the default sound plays instead."))
}

/// `path`'s sound once it is at most [`MAX_CUSTOM_BYTES`] and decodes,
/// else what went wrong. The size is checked before the file is read.
fn read_sound(path: &Path) -> Result<Sound, String> {
    let unreadable = |err: std::io::Error| format!("could not be read ({err})");
    let too_big = || "is over 5 MB".to_owned();
    let file = std::fs::File::open(path).map_err(unreadable)?;
    if file.metadata().map_err(unreadable)?.len() > MAX_CUSTOM_BYTES {
        return Err(too_big());
    }
    // The cap holds even should the file grow between the check and the read.
    let mut bytes = Vec::new();
    file.take(MAX_CUSTOM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if !u64::try_from(bytes.len()).is_ok_and(|read| read <= MAX_CUSTOM_BYTES) {
        return Err(too_big());
    }
    decode(bytes.into())
}

/// `bytes` opened as a sound, else why not.
fn decode(bytes: Arc<[u8]>) -> Result<Sound, String> {
    rodio::Decoder::new(Cursor::new(bytes))
        .map_err(|err| format!("is not a sound this client plays ({err})"))
}

/// What became of a clip offered to the player.
#[derive(Debug, PartialEq, Eq)]
enum Offer<T> {
    /// Nothing plays: start this one now.
    Start(T),
    /// Another plays: this one plays when it ends.
    Waits,
    /// Another plays and one waits already: this one never plays.
    Dropped(T),
}

/// The player's rule: at most one clip plays and at most one waits.
#[derive(Debug)]
struct PlayQueue<T> {
    playing: bool,
    /// The clip waiting to play, and the kind of toast it is for.
    waiting: Option<(NotifyKind, T)>,
}

impl<T> Default for PlayQueue<T> {
    fn default() -> Self {
        Self {
            playing: false,
            waiting: None,
        }
    }
}

impl<T> PlayQueue<T> {
    /// Takes `clip` for `kind` in under the rule: a Needs you clip takes a
    /// waiting Finished clip's place, and every other pairing keeps the clip
    /// that already waits.
    fn offer(&mut self, kind: NotifyKind, clip: T) -> Offer<T> {
        if !self.playing {
            self.playing = true;
            return Offer::Start(clip);
        }
        match self.waiting.take() {
            None => {
                self.waiting = Some((kind, clip));
                Offer::Waits
            }
            Some((waiting_kind, waiting)) => {
                if kind == NotifyKind::NeedsYou && waiting_kind == NotifyKind::Finished {
                    self.waiting = Some((kind, clip));
                    Offer::Dropped(waiting)
                } else {
                    self.waiting = Some((waiting_kind, waiting));
                    Offer::Dropped(clip)
                }
            }
        }
    }

    /// The playing clip ended: the waiting one, which now plays, if any.
    fn finished(&mut self) -> Option<T> {
        let next = self.waiting.take().map(|(_, clip)| clip);
        self.playing = next.is_some();
        next
    }

    /// Drops the waiting clip when it is `kind`'s, returning it.
    fn drop_waiting(&mut self, kind: NotifyKind) -> Option<T> {
        let waiting = self.waiting.as_ref().map(|(waiting_kind, _)| *waiting_kind);
        if waiting != Some(kind) {
            return None;
        }
        self.waiting.take().map(|(_, clip)| clip)
    }
}

/// The sounder the client runs with: one thread of its own plays the clips
/// on the default output device, opened for each clip, one at a time with
/// at most one waiting. When there is no device, or the thread cannot
/// start, the first failure is a warning and the rest are quiet.
#[derive(Default)]
pub struct SystemSounder {
    /// The playing thread, started on the first clip; `None` when it could
    /// not start.
    player: OnceLock<Option<PlayerThread>>,
}

/// The playing thread's handles: the rule it plays by, and where a clip to
/// start now is sent.
struct PlayerThread {
    queue: Arc<Mutex<PlayQueue<Clip>>>,
    start: Mutex<Sender<Clip>>,
}

/// `queue` locked, past a panic in another holder.
fn lock(queue: &Mutex<PlayQueue<Clip>>) -> MutexGuard<'_, PlayQueue<Clip>> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Sounder for SystemSounder {
    fn play(&self, clip: Clip) {
        let Some(player) = self.player.get_or_init(start_player) else {
            tracing::debug!("no sound player: {:?} not played", clip.source);
            return;
        };
        let kind = clip.kind;
        let offer = lock(&player.queue).offer(kind, clip);
        match offer {
            Offer::Start(clip) => {
                let sent = player
                    .start
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .send(clip);
                if let Err(unsent) = sent {
                    tracing::debug!(
                        "the sound player has stopped: {:?} not played",
                        unsent.0.source
                    );
                }
            }
            Offer::Waits => {}
            Offer::Dropped(clip) => {
                tracing::debug!(
                    "a toast sound plays and another waits: {:?} dropped",
                    clip.source
                );
            }
        }
    }

    fn drop_waiting(&self, kind: NotifyKind) {
        let Some(Some(player)) = self.player.get() else {
            return;
        };
        if let Some(clip) = lock(&player.queue).drop_waiting(kind) {
            tracing::debug!(
                "the {} sound is off: {:?} dropped",
                kind.label(),
                clip.source
            );
        }
    }
}

/// Starts the playing thread; `None`, with a warning, when it cannot start.
fn start_player() -> Option<PlayerThread> {
    let (start, clips) = std::sync::mpsc::channel();
    let queue = Arc::new(Mutex::new(PlayQueue::default()));
    let thread_queue = Arc::clone(&queue);
    match std::thread::Builder::new()
        .name("toast-sound".to_owned())
        .spawn(move || play_each(&clips, &thread_queue))
    {
        Ok(_) => Some(PlayerThread {
            queue,
            start: Mutex::new(start),
        }),
        Err(err) => {
            tracing::warn!("no toast sound: its player thread did not start: {err}");
            None
        }
    }
}

/// Plays each clip it is told to start, to its end, then the one waiting
/// behind it, until the sounder is gone.
fn play_each(clips: &Receiver<Clip>, queue: &Mutex<PlayQueue<Clip>>) {
    let mut warned = false;
    for first in clips {
        let mut next = Some(first);
        while let Some(clip) = next {
            if let Err(err) = play_to_end(clip) {
                if warned {
                    tracing::debug!("the toast sound did not play: {err}");
                } else {
                    tracing::warn!("the toast sound did not play: {err}");
                    warned = true;
                }
            }
            next = lock(queue).finished();
        }
    }
}

/// Plays `clip` on the default output device and waits for its end.
fn play_to_end(clip: Clip) -> Result<(), String> {
    let mut device = rodio::DeviceSinkBuilder::open_default_sink()
        .map_err(|err| format!("no audio output device opened ({err})"))?;
    device.log_on_drop(false);
    let player = rodio::Player::connect_new(device.mixer());
    player.append(clip.sound);
    player.sleep_until_end();
    Ok(())
}

/// A sounder that plays nothing, for the cloaked smoke-tier window and the
/// screenshot tool, which must never make a sound.
pub struct SilentSounder;

impl Sounder for SilentSounder {
    fn play(&self, clip: Clip) {
        tracing::debug!("offscreen window: sound {:?} not played", clip.source);
    }

    fn drop_waiting(&self, _kind: NotifyKind) {}
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_plays_at_most_two_clips() {
        let mut queue = PlayQueue::default();
        assert_eq!(
            queue.offer(NotifyKind::Finished, 1),
            Offer::Start(1),
            "nothing plays yet"
        );
        assert_eq!(
            queue.offer(NotifyKind::Finished, 2),
            Offer::Waits,
            "one may wait"
        );
        assert_eq!(
            queue.offer(NotifyKind::Finished, 3),
            Offer::Dropped(3),
            "a second may not"
        );
        assert_eq!(queue.offer(NotifyKind::Finished, 4), Offer::Dropped(4));
        assert_eq!(queue.finished(), Some(2), "the waiting one plays next");
        assert_eq!(
            queue.offer(NotifyKind::Finished, 5),
            Offer::Waits,
            "behind 2, 5 may wait"
        );
        assert_eq!(queue.finished(), Some(5));
        assert_eq!(queue.finished(), None, "nothing is left to play");
        assert_eq!(
            queue.offer(NotifyKind::Finished, 6),
            Offer::Start(6),
            "an idle player starts at once"
        );
    }

    #[test]
    fn off_drops_the_waiting_clip() {
        let mut queue = PlayQueue::default();
        assert_eq!(queue.offer(NotifyKind::Finished, 1), Offer::Start(1));
        assert_eq!(queue.offer(NotifyKind::Finished, 2), Offer::Waits);
        assert_eq!(queue.drop_waiting(NotifyKind::Finished), Some(2));
        assert_eq!(queue.finished(), None, "the dropped clip never plays");
        assert_eq!(queue.offer(NotifyKind::Finished, 3), Offer::Start(3));
    }

    #[test]
    fn a_needs_you_clip_replaces_a_waiting_finished_clip() {
        let mut queue = PlayQueue::default();
        assert_eq!(queue.offer(NotifyKind::Finished, 1), Offer::Start(1));
        assert_eq!(queue.offer(NotifyKind::Finished, 2), Offer::Waits);
        assert_eq!(
            queue.offer(NotifyKind::NeedsYou, 3),
            Offer::Dropped(2),
            "the waiting Finished clip gives way"
        );
        assert_eq!(queue.finished(), Some(3), "the Needs you clip plays next");
        assert_eq!(queue.finished(), None);
    }

    #[test]
    fn dropping_waiting_finished_clips_keeps_a_waiting_needs_you_clip() {
        let mut queue = PlayQueue::default();
        assert_eq!(queue.offer(NotifyKind::Finished, 1), Offer::Start(1));
        assert_eq!(queue.offer(NotifyKind::NeedsYou, 2), Offer::Waits);
        assert_eq!(
            queue.offer(NotifyKind::Finished, 3),
            Offer::Dropped(3),
            "a Finished clip does not take a waiting Needs you clip's place"
        );
        assert_eq!(
            queue.drop_waiting(NotifyKind::Finished),
            None,
            "the waiting clip is Needs you's, not Finished's"
        );
        assert_eq!(queue.finished(), Some(2), "and it still plays");
    }

    #[test]
    fn a_custom_file_over_5_mb_falls_back_to_the_default() {
        let path = std::env::temp_dir().join(format!(
            "rt-native-sound-over-5-mb-{}.wav",
            std::process::id()
        ));
        let size = usize::try_from(MAX_CUSTOM_BYTES).expect("5 MiB fits a usize") + 1;
        std::fs::write(&path, vec![0_u8; size]).expect("write the oversized file");
        let choice = ToastSound::Custom(path.clone());
        let problem = problem_of(&choice);
        let source = clip_for(&choice, NotifyKind::NeedsYou).map(|clip| clip.source);
        std::fs::remove_file(&path).expect("remove the oversized file");
        assert_eq!(
            problem.as_deref(),
            Some("This file is over 5 MB; the default sound plays instead.")
        );
        assert_eq!(
            source,
            Some(ClipSource::Bundled(BundledSound::MalletRisingFifth))
        );
    }

    fn source_of(choice: &ToastSound, kind: NotifyKind) -> Option<ClipSource> {
        clip_for(choice, kind).map(|clip| clip.source)
    }

    #[test]
    fn the_default_sound_is_the_kinds_mallet_fifth() {
        assert_eq!(
            source_of(&ToastSound::Default, NotifyKind::NeedsYou),
            Some(ClipSource::Bundled(BundledSound::MalletRisingFifth))
        );
        assert_eq!(
            source_of(&ToastSound::Default, NotifyKind::Finished),
            Some(ClipSource::Bundled(BundledSound::MalletFallingFifth))
        );
    }

    #[test]
    fn a_bundled_choice_plays_that_sound_for_either_kind() {
        let choice = ToastSound::Bundled(BundledSound::Bong);
        for kind in [NotifyKind::NeedsYou, NotifyKind::Finished] {
            assert_eq!(
                source_of(&choice, kind),
                Some(ClipSource::Bundled(BundledSound::Bong))
            );
        }
    }

    #[test]
    fn every_bundled_sound_decodes_and_its_id_is_its_saved_id() {
        for sound in BundledSound::ALL {
            assert_eq!(
                source_of(&ToastSound::Bundled(sound), NotifyKind::Finished),
                Some(ClipSource::Bundled(sound)),
                "{sound:?} decodes"
            );
            assert_eq!(
                serde_json::to_value(sound).expect("a sound serializes"),
                serde_json::json!(sound.id())
            );
        }
    }

    #[test]
    fn a_missing_custom_file_falls_back_to_the_kinds_default() {
        let choice = ToastSound::Custom(PathBuf::from("rt-native-no-such-sound.ogg"));
        assert_eq!(
            source_of(&choice, NotifyKind::NeedsYou),
            Some(ClipSource::Bundled(BundledSound::MalletRisingFifth))
        );
        assert_eq!(
            source_of(&choice, NotifyKind::Finished),
            Some(ClipSource::Bundled(BundledSound::MalletFallingFifth))
        );
    }
}
