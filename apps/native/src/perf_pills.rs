//! The status bar's CPU and RAM pills, one per machine the user tracks:
//! what each pill says, how it is coloured and what its tip lists, the
//! saved choice of tracked machines, and the latest sample each tracked
//! host's daemon sent. No GPUI here; the view draws [`Pill`]s.

use std::collections::HashMap;

use protocol::MachineSample;

use crate::sidebar::LOCAL_HOST_KEY;

/// Where a tracked machine's figures stand.
#[derive(Debug, Clone, PartialEq)]
pub enum PerfHostState {
    /// The latest sample its daemon sent.
    Live(MachineSample),
    /// Connected to a daemon that reports CPU and RAM, no sample yet.
    Waiting,
    /// No open connection to it.
    Offline,
    /// Connected to a daemon that does not report CPU and RAM.
    Unsupported,
}

/// How a pill draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillLevel {
    Normal,
    /// CPU or memory is high.
    Warn,
    /// No figures to show.
    Muted,
}

/// One machine's pill: its text, how it draws and its tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    pub text: String,
    pub level: PillLevel,
    pub tip: String,
}

/// The pill for machine `name` in `state`; `None` for a daemon that does
/// not report CPU and RAM.
#[must_use]
pub fn pill(name: &str, state: &PerfHostState) -> Option<Pill> {
    match state {
        PerfHostState::Live(sample) => Some(live_pill(name, sample)),
        PerfHostState::Waiting => Some(Pill {
            text: format!("{name} CPU – · RAM –"),
            level: PillLevel::Muted,
            tip: format!("{name}\nWaiting for the first reading"),
        }),
        PerfHostState::Offline => Some(Pill {
            text: format!("{name} offline"),
            level: PillLevel::Muted,
            tip: format!("{name} is offline"),
        }),
        PerfHostState::Unsupported => None,
    }
}

/// CPU above this percent warns.
const CPU_WARN_PERCENT: f32 = 80.0;
/// Memory in use above this percent warns.
const MEM_WARN_PERCENT: f64 = 90.0;
/// Bytes in the GB the tip counts in.
const BYTES_PER_GB: f64 = 1_073_741_824.0;

/// The pill of a machine whose daemon sent `sample`.
fn live_pill(name: &str, sample: &MachineSample) -> Pill {
    let cpu = sample.cpu_percent;
    let mem = mem_percent(sample);
    let cpu_text = cpu.map_or_else(|| "–".to_owned(), |cpu| format!("{:.0}%", cpu.round()));
    let ram_text = mem.map_or_else(|| "–".to_owned(), |mem| format!("{:.0}%", mem.round()));
    let warn = cpu.is_some_and(|cpu| cpu > CPU_WARN_PERCENT)
        || mem.is_some_and(|mem| mem > MEM_WARN_PERCENT);
    let cpu_line = cpu.map_or_else(
        || "CPU: waiting for a second reading".to_owned(),
        |cpu| format!("CPU {cpu:.1}%"),
    );
    let ram_line = format!(
        "RAM {:.1} of {:.1} GB",
        gb(sample.mem_used_bytes),
        gb(sample.mem_total_bytes)
    );
    Pill {
        text: format!("{name} CPU {cpu_text} · RAM {ram_text}"),
        level: if warn {
            PillLevel::Warn
        } else {
            PillLevel::Normal
        },
        tip: format!("{name}\n{cpu_line}\n{ram_line}"),
    }
}

/// The memory in use as a percent of the total; `None` with no total.
#[expect(
    clippy::cast_precision_loss,
    reason = "a percent needs no more than f64's 52 bits of a byte count"
)]
fn mem_percent(sample: &MachineSample) -> Option<f64> {
    (sample.mem_total_bytes > 0)
        .then(|| sample.mem_used_bytes as f64 * 100.0 / sample.mem_total_bytes as f64)
}

/// `bytes` in GB.
#[expect(
    clippy::cast_precision_loss,
    reason = "a figure shown to one decimal needs no more than f64's 52 bits"
)]
fn gb(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_GB
}

/// The tracked machines' keys: the saved list, else This PC alone.
#[must_use]
pub fn tracked_keys(saved: Option<&[String]>) -> Vec<String> {
    saved.map_or_else(|| vec![LOCAL_HOST_KEY.to_owned()], <[String]>::to_vec)
}

/// `keys` with `key` removed when it is there, else appended.
#[must_use]
pub fn toggled(keys: &[String], key: &str) -> Vec<String> {
    if keys.iter().any(|kept| kept == key) {
        keys.iter().filter(|kept| *kept != key).cloned().collect()
    } else {
        let mut keys = keys.to_vec();
        keys.push(key.to_owned());
        keys
    }
}

/// The latest sample each tracked host's daemon sent, by host key.
#[derive(Debug, Default)]
pub struct PerfSamples {
    by_host: HashMap<String, MachineSample>,
}

impl PerfSamples {
    /// Keeps `sample` as host `key`'s latest.
    pub fn set(&mut self, key: &str, sample: MachineSample) {
        self.by_host.insert(key.to_owned(), sample);
    }

    /// Drops host `key`'s sample.
    pub fn clear(&mut self, key: &str) {
        self.by_host.remove(key);
    }

    /// Host `key`'s latest sample, if it has one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&MachineSample> {
        self.by_host.get(key)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use protocol::MachineSample;

    use super::{PerfHostState, Pill, PillLevel, pill, toggled, tracked_keys};

    const GIB: u64 = 1_073_741_824;

    fn live(cpu: Option<f32>, used: u64, total: u64) -> PerfHostState {
        PerfHostState::Live(MachineSample {
            cpu_percent: cpu,
            mem_used_bytes: used,
            mem_total_bytes: total,
        })
    }

    fn pill_of(state: &PerfHostState) -> Pill {
        pill("This PC", state).expect("a pill")
    }

    #[test]
    fn live_pill_shows_rounded_cpu_and_ram_percent() {
        let pill = pill_of(&live(Some(23.4), 8 * GIB, 32 * GIB));
        assert_eq!(pill.text, "This PC CPU 23% · RAM 25%");
        assert_eq!(pill.level, PillLevel::Normal);
    }

    #[test]
    fn cpu_above_80_warns() {
        let pill = pill_of(&live(Some(80.4), 8 * GIB, 32 * GIB));
        assert_eq!(pill.level, PillLevel::Warn);
        assert!(pill.text.contains("CPU 80%"), "{}", pill.text);
    }

    #[test]
    fn cpu_at_80_does_not_warn() {
        let pill = pill_of(&live(Some(80.0), 8 * GIB, 32 * GIB));
        assert_eq!(pill.level, PillLevel::Normal);
    }

    #[test]
    fn ram_above_90_warns() {
        let pill = pill_of(&live(Some(10.0), 29 * GIB, 32 * GIB));
        assert_eq!(pill.level, PillLevel::Warn);
        assert!(pill.text.contains("RAM 91%"), "{}", pill.text);
    }

    #[test]
    fn ram_at_90_does_not_warn() {
        let pill = pill_of(&live(Some(10.0), 9 * GIB, 10 * GIB));
        assert_eq!(pill.level, PillLevel::Normal);
        assert!(pill.text.contains("RAM 90%"), "{}", pill.text);
    }

    #[test]
    fn no_cpu_yet_shows_a_dash() {
        let pill = pill_of(&live(None, 8 * GIB, 32 * GIB));
        assert_eq!(pill.text, "This PC CPU – · RAM 25%");
        assert!(
            pill.tip
                .lines()
                .any(|line| line == "CPU: waiting for a second reading"),
            "{}",
            pill.tip
        );
    }

    #[test]
    fn zero_total_memory_shows_a_dash() {
        let pill = pill_of(&live(Some(23.4), 0, 0));
        assert_eq!(pill.text, "This PC CPU 23% · RAM –");
        assert_eq!(pill.level, PillLevel::Normal);
    }

    #[test]
    fn waiting_and_offline_are_muted() {
        let waiting = pill("box", &PerfHostState::Waiting).expect("a pill");
        assert_eq!(waiting.text, "box CPU – · RAM –");
        assert_eq!(waiting.level, PillLevel::Muted);
        let offline = pill("box", &PerfHostState::Offline).expect("a pill");
        assert_eq!(offline.text, "box offline");
        assert_eq!(offline.level, PillLevel::Muted);
    }

    #[test]
    fn unsupported_has_no_pill() {
        assert_eq!(pill("box", &PerfHostState::Unsupported), None);
    }

    #[test]
    fn tip_lists_cpu_and_ram_in_gb() {
        let pill = pill_of(&live(Some(23.4), 8 * GIB, 32 * GIB));
        assert_eq!(pill.tip, "This PC\nCPU 23.4%\nRAM 8.0 of 32.0 GB");
    }

    #[test]
    fn tracked_defaults_to_this_pc() {
        assert_eq!(tracked_keys(None), ["local"]);
        assert_eq!(tracked_keys(Some(&[])), [] as [String; 0]);
    }

    #[test]
    fn toggling_adds_then_removes() {
        let keys = vec!["local".to_owned()];
        let added = toggled(&keys, "box");
        assert_eq!(added, ["local", "box"]);
        assert_eq!(toggled(&added, "box"), ["local"]);
    }
}
