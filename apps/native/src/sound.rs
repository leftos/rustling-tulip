//! The Needs you sound: which clip a choice plays, the bundled clip a
//! missing, oversized or undecodable custom file falls back to, and the
//! sounder that plays a clip on this PC's default output device, off the UI
//! thread, one at a time with at most one waiting.

use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use crate::sidebar::NeedsYouSound;

/// The bundled two-tone chime (`assets/sounds/README.md`).
const BUNDLED: &[u8] = include_bytes!("../assets/sounds/needs-you.ogg");

/// The file types the Custom choice's picker offers.
pub(crate) const SOUND_EXTENSIONS: [&str; 2] = ["wav", "ogg"];

/// The largest custom file the client reads: 5 MiB.
const MAX_CUSTOM_BYTES: u64 = 5 * 1024 * 1024;

/// A sound's decoder, over the whole encoded file held in memory.
type Sound = rodio::Decoder<Cursor<Arc<[u8]>>>;

/// Where a clip's sound came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipSource {
    /// The sound bundled with the client.
    Bundled,
    /// The custom file at this path.
    File(PathBuf),
}

/// A sound ready to play: decoded once, when it was read, and played from
/// that decoder.
pub struct Clip {
    pub source: ClipSource,
    sound: Sound,
}

impl Clip {
    /// The bundled sound; `None`, with a warning, should it not decode.
    fn bundled() -> Option<Self> {
        match decode(Arc::from(BUNDLED)) {
            Ok(sound) => Some(Self {
                source: ClipSource::Bundled,
                sound,
            }),
            Err(problem) => {
                tracing::warn!("the bundled Needs you sound {problem}");
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

    /// Drops the clip waiting to play, if any; the one playing plays on.
    fn drop_waiting(&self);
}

/// The clip `choice` plays: none when it is off; the custom file when it
/// reads and decodes, else the bundled sound with a warning naming the path.
/// Reads the file, so it runs off the UI thread.
#[must_use]
pub(crate) fn clip_for(choice: &NeedsYouSound) -> Option<Clip> {
    match choice {
        NeedsYouSound::Off => None,
        NeedsYouSound::Default => Clip::bundled(),
        NeedsYouSound::Custom(path) => match read_sound(path) {
            Ok(sound) => Some(Clip {
                source: ClipSource::File(path.clone()),
                sound,
            }),
            Err(problem) => {
                tracing::warn!(
                    "the Needs you sound {} {problem}; playing the default sound",
                    path.display()
                );
                Clip::bundled()
            }
        },
    }
}

/// What is wrong with `choice`'s custom file, worded for Settings; `None`
/// when the choice plays as chosen. Reads the file, so it runs off the UI
/// thread.
#[must_use]
pub(crate) fn problem_of(choice: &NeedsYouSound) -> Option<String> {
    let NeedsYouSound::Custom(path) = choice else {
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
    waiting: Option<T>,
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
    /// Takes `clip` in under the rule.
    fn offer(&mut self, clip: T) -> Offer<T> {
        if !self.playing {
            self.playing = true;
            Offer::Start(clip)
        } else if self.waiting.is_none() {
            self.waiting = Some(clip);
            Offer::Waits
        } else {
            Offer::Dropped(clip)
        }
    }

    /// The playing clip ended: the waiting one, which now plays, if any.
    fn finished(&mut self) -> Option<T> {
        let next = self.waiting.take();
        self.playing = next.is_some();
        next
    }

    /// Drops the waiting clip, returning it.
    fn drop_waiting(&mut self) -> Option<T> {
        self.waiting.take()
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
        let offer = lock(&player.queue).offer(clip);
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
                    "a Needs you sound plays and another waits: {:?} dropped",
                    clip.source
                );
            }
        }
    }

    fn drop_waiting(&self) {
        let Some(Some(player)) = self.player.get() else {
            return;
        };
        if let Some(clip) = lock(&player.queue).drop_waiting() {
            tracing::debug!("the Needs you sound is off: {:?} dropped", clip.source);
        }
    }
}

/// Starts the playing thread; `None`, with a warning, when it cannot start.
fn start_player() -> Option<PlayerThread> {
    let (start, clips) = std::sync::mpsc::channel();
    let queue = Arc::new(Mutex::new(PlayQueue::default()));
    let thread_queue = Arc::clone(&queue);
    match std::thread::Builder::new()
        .name("needs-you-sound".to_owned())
        .spawn(move || play_each(&clips, &thread_queue))
    {
        Ok(_) => Some(PlayerThread {
            queue,
            start: Mutex::new(start),
        }),
        Err(err) => {
            tracing::warn!("no Needs you sound: its player thread did not start: {err}");
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
                    tracing::debug!("the Needs you sound did not play: {err}");
                } else {
                    tracing::warn!("the Needs you sound did not play: {err}");
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

    fn drop_waiting(&self) {}
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
        assert_eq!(queue.offer(1), Offer::Start(1), "nothing plays yet");
        assert_eq!(queue.offer(2), Offer::Waits, "one may wait");
        assert_eq!(queue.offer(3), Offer::Dropped(3), "a second may not");
        assert_eq!(queue.offer(4), Offer::Dropped(4));
        assert_eq!(queue.finished(), Some(2), "the waiting one plays next");
        assert_eq!(queue.offer(5), Offer::Waits, "behind 2, 5 may wait");
        assert_eq!(queue.finished(), Some(5));
        assert_eq!(queue.finished(), None, "nothing is left to play");
        assert_eq!(
            queue.offer(6),
            Offer::Start(6),
            "an idle player starts at once"
        );
    }

    #[test]
    fn off_drops_the_waiting_clip() {
        let mut queue = PlayQueue::default();
        assert_eq!(queue.offer(1), Offer::Start(1));
        assert_eq!(queue.offer(2), Offer::Waits);
        assert_eq!(queue.drop_waiting(), Some(2));
        assert_eq!(queue.finished(), None, "the dropped clip never plays");
        assert_eq!(queue.offer(3), Offer::Start(3));
    }

    #[test]
    fn a_custom_file_over_5_mb_falls_back_to_the_default() {
        let path = std::env::temp_dir().join(format!(
            "rt-native-sound-over-5-mb-{}.wav",
            std::process::id()
        ));
        let size = usize::try_from(MAX_CUSTOM_BYTES).expect("5 MiB fits a usize") + 1;
        std::fs::write(&path, vec![0_u8; size]).expect("write the oversized file");
        let choice = NeedsYouSound::Custom(path.clone());
        let problem = problem_of(&choice);
        let source = clip_for(&choice).map(|clip| clip.source);
        std::fs::remove_file(&path).expect("remove the oversized file");
        assert_eq!(
            problem.as_deref(),
            Some("This file is over 5 MB; the default sound plays instead.")
        );
        assert_eq!(source, Some(ClipSource::Bundled));
    }
}
