//! The account store's settings file, `settings.json` in the store's root ([`AccountsRoot::settings`]): the auto-switch
//! engine's settings (`autoswitch`) and the fleet verdict's (`fleet`).
//!
//! A missing file is [`Settings::default`]. A read is strict: an unknown field, a value of the wrong type or a value
//! outside its range is an error naming the field (`autoswitch.threshold`), never a silent default. A write checks the
//! same ranges first, then takes the settings lock (`.settings.lock`) and replaces the file through a temp file in the
//! root; both sections are always written.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::path::Path;

use jiff::SignedDuration;
use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::locks::{ACCOUNT_LOCK_TIMEOUT, LockError, acquire_in_root};
use crate::paths::AccountsRoot;
use crate::store::{AutoSwitchSettings, Strategy, write_json_atomic};

/// Why the settings could not be read or written. The text names the file kind and the field, never a path.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsError {
    /// The file exists but could not be read.
    #[error("settings.json could not be read")]
    Read(#[source] io::Error),
    /// The file, or the settings to write, are not valid: the text names the field and what is wrong with it.
    #[error("settings.json is not valid: {0}")]
    Invalid(String),
    /// The settings lock could not be taken.
    #[error("settings.json could not be written: its lock could not be taken")]
    Lock(#[source] LockError),
    /// The file could not be written; the old file is unchanged.
    #[error("settings.json could not be written")]
    Write(#[source] io::Error),
}

/// The settings; a missing file is [`Settings::default`], and a missing section is that section's default.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Settings {
    /// The auto-switch engine.
    #[serde(default)]
    pub autoswitch: AutoSwitchSection,
    /// The fleet verdict.
    #[serde(default)]
    pub fleet: FleetSettings,
}

impl Settings {
    /// The settings in the file at `path`, or the defaults when there is no file there.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Read`] when the file cannot be read, [`SettingsError::Invalid`] when it is not JSON, holds an
    /// unknown field, a value of the wrong type or a value outside its range ([`Settings::check`]), naming the field.
    pub fn read(path: &Path) -> Result<Self, SettingsError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(SettingsError::Read(error)),
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| SettingsError::Invalid(error.to_string()))?;
        let settings = Self::deserialize(&value).map_err(|error| {
            SettingsError::Invalid(failing_field(&value).unwrap_or_else(|| error.to_string()))
        })?;
        settings.check().map_err(SettingsError::Invalid)?;
        Ok(settings)
    }

    /// Writes `settings` to `root`'s `settings.json`: checks them first ([`Settings::check`]), then, under the settings
    /// lock, writes a temp file in the root and renames it onto the file. Both sections are always written.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Invalid`] for a value outside its range, with nothing written; [`SettingsError::Lock`] when
    /// the root is refused or the lock stays held past [`ACCOUNT_LOCK_TIMEOUT`]; [`SettingsError::Write`] when the file
    /// cannot be written.
    pub fn write(root: &AccountsRoot, settings: &Settings) -> Result<(), SettingsError> {
        settings.check().map_err(SettingsError::Invalid)?;
        let _lock = acquire_in_root(root, &root.settings_lock(), ACCOUNT_LOCK_TIMEOUT)
            .map_err(SettingsError::Lock)?;
        write_json_atomic(&root.settings(), settings).map_err(SettingsError::Write)
    }

    /// The first value outside its range, as a message naming its field ([`AutoSwitchSection::check`]); the fleet
    /// section's one value is checked as it is read.
    ///
    /// # Errors
    ///
    /// The problem, such as `autoswitch.threshold is 49, which is outside 50 to 99.9`.
    pub fn check(&self) -> Result<(), String> {
        self.autoswitch.check()
    }
}

/// The field of `value`, a settings document serde refused, that fails on its own, as `<section>.<field>: <why>`, or
/// `<section>: <why>` for a section that is not an object; `None` when no single field fails.
fn failing_field(value: &Value) -> Option<String> {
    for (section, body) in value.as_object()? {
        let failure = match section.as_str() {
            "autoswitch" => field_failure::<AutoSwitchSection>(body),
            "fleet" => field_failure::<FleetSettings>(body),
            _ => {
                return Some(format!(
                    "{section} is not a settings section; the sections are autoswitch and fleet"
                ));
            }
        };
        if let Some((field, why)) = failure {
            return Some(match field {
                Some(field) => format!("{section}.{field}: {why}"),
                None => format!("{section}: {why}"),
            });
        }
    }
    None
}

/// The first field of the section `body` that `T` refuses on its own, with serde's reason; the whole section's
/// reason, with no field, when it is not an object.
fn field_failure<T: DeserializeOwned>(body: &Value) -> Option<(Option<&str>, String)> {
    let Some(fields) = body.as_object() else {
        return T::deserialize(body)
            .err()
            .map(|error| (None, error.to_string()));
    };
    for (field, field_value) in fields {
        let mut alone = Map::new();
        alone.insert(field.clone(), field_value.clone());
        if let Err(error) = T::deserialize(&Value::Object(alone)) {
            return Some((Some(field.as_str()), error.to_string()));
        }
    }
    None
}

/// The auto-switch engine's settings, written in the file in camelCase, each absent one its default; a read or a write
/// refuses a value outside its range ([`AutoSwitchSection::check`]).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AutoSwitchSection {
    /// Whether the engine may switch at all; while false it keeps reading usage and its records but moves nothing.
    /// Off when the file or the key is missing: auto-switch is opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// The binding window's utilization, in percent, at which to switch ([`AutoSwitchSection::THRESHOLD`]).
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    /// How the engine picks the account to switch to.
    #[serde(default)]
    pub strategy: StrategyName,
    /// The model names whose weekly limits also count, matched case-insensitively; `all` counts every scoped window,
    /// and an empty list counts the account-wide windows only.
    #[serde(default)]
    pub models: Vec<String>,
    /// Seconds between engine ticks ([`AutoSwitchSection::INTERVAL_SECONDS`]).
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: f64,
    /// Least seconds between proactive switches ([`AutoSwitchSection::COOLDOWN_SECONDS`]).
    #[serde(default = "default_cooldown_seconds")]
    pub cooldown_seconds: f64,
    /// Percentage points a target must beat the active account by ([`AutoSwitchSection::HYSTERESIS_PCT`]).
    #[serde(default = "default_hysteresis_pct")]
    pub hysteresis_pct: f64,
    /// Failed polls in a row before an account counts as unhealthy ([`AutoSwitchSection::UNHEALTHY_TICKS`]).
    #[serde(default = "default_unhealthy_ticks")]
    pub unhealthy_ticks: u32,
}

fn default_threshold() -> f64 {
    AutoSwitchSettings::default().threshold
}

fn default_interval_seconds() -> f64 {
    AutoSwitchSettings::default().interval_seconds
}

fn default_cooldown_seconds() -> f64 {
    AutoSwitchSettings::default().cooldown_seconds
}

fn default_hysteresis_pct() -> f64 {
    AutoSwitchSettings::default().hysteresis_pct
}

fn default_unhealthy_ticks() -> u32 {
    AutoSwitchSettings::default().unhealthy_ticks
}

impl Default for AutoSwitchSection {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_threshold(),
            strategy: StrategyName::default(),
            models: Vec::new(),
            interval_seconds: default_interval_seconds(),
            cooldown_seconds: default_cooldown_seconds(),
            hysteresis_pct: default_hysteresis_pct(),
            unhealthy_ticks: default_unhealthy_ticks(),
        }
    }
}

impl AutoSwitchSection {
    /// The allowed thresholds, in percent.
    pub const THRESHOLD: RangeInclusive<f64> = 50.0..=99.9;
    /// The allowed seconds between engine ticks.
    pub const INTERVAL_SECONDS: RangeInclusive<f64> = 15.0..=3600.0;
    /// The allowed least seconds between proactive switches.
    pub const COOLDOWN_SECONDS: RangeInclusive<f64> = 0.0..=86400.0;
    /// The allowed percentage points a target must beat the active account by.
    pub const HYSTERESIS_PCT: RangeInclusive<f64> = 0.0..=50.0;
    /// The allowed failed polls in a row before an account counts as unhealthy.
    pub const UNHEALTHY_TICKS: RangeInclusive<u32> = 1..=100;
    /// The most model names `models` may hold.
    pub const MAX_MODELS: usize = 16;
    /// The most characters a model name may have.
    pub const MAX_MODEL_CHARS: usize = 64;

    /// The first value outside its range, in field order, as a message naming its field: a number outside its range
    /// (or not finite), more than [`Self::MAX_MODELS`] model names, or a model name that is empty, has spaces around
    /// it, is over [`Self::MAX_MODEL_CHARS`] characters, holds a control, bidi-control or other invisible character (a
    /// line or paragraph separator, a zero-width space, a byte-order mark), or is named twice ignoring case.
    ///
    /// # Errors
    ///
    /// The problem, such as `autoswitch.threshold is 49, which is outside 50 to 99.9`.
    pub fn check(&self) -> Result<(), String> {
        let numbers = [
            ("threshold", self.threshold, &Self::THRESHOLD),
            (
                "intervalSeconds",
                self.interval_seconds,
                &Self::INTERVAL_SECONDS,
            ),
            (
                "cooldownSeconds",
                self.cooldown_seconds,
                &Self::COOLDOWN_SECONDS,
            ),
            ("hysteresisPct", self.hysteresis_pct, &Self::HYSTERESIS_PCT),
        ];
        for (key, value, range) in numbers {
            if !value.is_finite() || !range.contains(&value) {
                return Err(outside(key, value, range));
            }
        }
        if !Self::UNHEALTHY_TICKS.contains(&self.unhealthy_ticks) {
            return Err(outside(
                "unhealthyTicks",
                self.unhealthy_ticks,
                &Self::UNHEALTHY_TICKS,
            ));
        }
        self.check_models()
    }

    /// [`AutoSwitchSection::check`] for `models`.
    fn check_models(&self) -> Result<(), String> {
        if self.models.len() > Self::MAX_MODELS {
            return Err(format!(
                "autoswitch.models holds {} names, at most {} models",
                self.models.len(),
                Self::MAX_MODELS
            ));
        }
        let mut seen = BTreeSet::new();
        for name in &self.models {
            if name.trim().is_empty() {
                return Err("autoswitch.models has an empty name".to_owned());
            }
            if name.chars().count() > Self::MAX_MODEL_CHARS {
                return Err(format!(
                    "autoswitch.models: a model name is at most {} characters",
                    Self::MAX_MODEL_CHARS
                ));
            }
            if name.chars().any(is_control_or_bidi) {
                return Err(
                    "autoswitch.models: a model name may not hold control characters".to_owned(),
                );
            }
            if name.trim() != name {
                return Err(format!(
                    "autoswitch.models has \"{name}\" with spaces around it"
                ));
            }
            if !seen.insert(name.to_lowercase()) {
                return Err(format!("autoswitch.models has \"{name}\" twice"));
            }
        }
        Ok(())
    }

    /// These settings as the engine and the poll take them; `enabled` is the caller's to act on and is not carried.
    #[must_use]
    pub fn to_engine(&self) -> AutoSwitchSettings {
        AutoSwitchSettings {
            threshold: self.threshold,
            interval_seconds: self.interval_seconds,
            cooldown_seconds: self.cooldown_seconds,
            hysteresis_pct: self.hysteresis_pct,
            strategy: self.strategy.into(),
            unhealthy_ticks: self.unhealthy_ticks,
            models: self.models.clone(),
        }
    }
}

/// The invisible characters a model name may not hold besides the control characters: the bidi embeddings, overrides
/// and isolates, the marks and the Arabic letter mark, the line and paragraph separators, the zero-width space and the
/// byte-order mark.
const INVISIBLE_CONTROLS: [char; 16] = [
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}', '\u{200E}', '\u{200F}', '\u{061C}', '\u{2028}', '\u{2029}', '\u{200B}', '\u{FEFF}',
];

/// Whether `character` is a control character or one of [`INVISIBLE_CONTROLS`].
pub(crate) fn is_control_or_bidi(character: char) -> bool {
    character.is_control() || INVISIBLE_CONTROLS.contains(&character)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::Settings;
    use std::path::PathBuf;

    /// A fresh scratch folder for `test`.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-settings-{test}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch folder can be made");
        dir
    }

    /// The settings read from a `settings.json` holding `text`.
    fn read_text(test: &str, text: &str) -> Settings {
        let path = scratch(test).join("settings.json");
        std::fs::write(&path, text).expect("settings.json can be written");
        Settings::read(&path).expect("the settings read")
    }

    #[test]
    fn auto_switch_is_off_without_a_settings_file() {
        let path = scratch("no-file").join("settings.json");

        let settings = Settings::read(&path).expect("a missing file is the defaults");

        assert!(!settings.autoswitch.enabled, "auto-switch defaults to off");
    }

    #[test]
    fn auto_switch_is_off_without_its_section() {
        let settings = read_text("no-section", r#"{"fleet": {"rateWindowMinutes": 30}}"#);

        assert!(
            !settings.autoswitch.enabled,
            "a missing section reads as off"
        );
        let settings = read_text("empty-section", r#"{"autoswitch": {"threshold": 90}}"#);
        assert!(!settings.autoswitch.enabled, "a missing key reads as off");
    }

    #[test]
    fn auto_switch_reads_on_when_the_file_says_so() {
        let settings = read_text("on", r#"{"autoswitch": {"enabled": true}}"#);

        assert!(settings.autoswitch.enabled);
    }
}

/// `autoswitch.<key> is <value>, which is outside <low> to <high>`.
fn outside<T: fmt::Display>(key: &str, value: T, range: &RangeInclusive<T>) -> String {
    format!(
        "autoswitch.{key} is {value}, which is outside {} to {}",
        range.start(),
        range.end()
    )
}

/// How auto-switch picks the account to switch to, written in the file as `best` or `consume-first`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StrategyName {
    /// The account with the most headroom, the default.
    #[default]
    Best,
    /// The account whose weekly window resets soonest.
    ConsumeFirst,
}

impl StrategyName {
    /// Every strategy, the default first.
    pub const ALL: [Self; 2] = [Self::Best, Self::ConsumeFirst];

    /// The name as the file writes it: `best` or `consume-first`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Best => "best",
            Self::ConsumeFirst => "consume-first",
        }
    }
}

impl From<StrategyName> for Strategy {
    fn from(name: StrategyName) -> Self {
        match name {
            StrategyName::Best => Self::Best,
            StrategyName::ConsumeFirst => Self::ConsumeFirst,
        }
    }
}

/// How the fleet verdict reads usage, written in the file as `{"rateWindowMinutes": 60}`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FleetSettings {
    /// The span each pooled account's recent usage rise is read over; [`RateWindow::OneHour`] when absent.
    #[serde(default, rename = "rateWindowMinutes")]
    pub rate_window: RateWindow,
}

/// The span the fleet verdict reads each pooled account's usage rise over, written in the file as its minutes: 30, 60,
/// 120 or 180, any other value refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateWindow {
    /// 30 minutes.
    ThirtyMinutes,
    /// 1 hour, the default.
    #[default]
    OneHour,
    /// 2 hours.
    TwoHours,
    /// 3 hours.
    ThreeHours,
}

impl RateWindow {
    /// Every window, shortest first.
    pub const ALL: [Self; 4] = [
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::TwoHours,
        Self::ThreeHours,
    ];

    /// The window's length in minutes: 30, 60, 120 or 180.
    #[must_use]
    pub const fn minutes(self) -> u32 {
        match self {
            Self::ThirtyMinutes => 30,
            Self::OneHour => 60,
            Self::TwoHours => 120,
            Self::ThreeHours => 180,
        }
    }

    /// The window's length.
    #[must_use]
    pub fn span(self) -> SignedDuration {
        SignedDuration::from_mins(i64::from(self.minutes()))
    }

    /// The window `minutes` long; `None` when no window is.
    #[must_use]
    pub fn from_minutes(minutes: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|window| window.minutes() == minutes)
    }

    /// Every window's minutes as a list, `30, 60, 120, 180`, for a message that names the allowed ones.
    #[must_use]
    pub fn choices_text() -> String {
        Self::ALL
            .map(|window| window.minutes().to_string())
            .join(", ")
    }
}

impl Serialize for RateWindow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.minutes())
    }
}

impl<'de> Deserialize<'de> for RateWindow {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_u64(RateWindowMinutes)
    }
}

/// Reads a [`RateWindow`] from its minutes, refusing any other value with an error that names the allowed ones.
struct RateWindowMinutes;

impl de::Visitor<'_> for RateWindowMinutes {
    type Value = RateWindow;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "one of {}", RateWindow::choices_text())
    }

    fn visit_u64<E: de::Error>(self, minutes: u64) -> Result<RateWindow, E> {
        u32::try_from(minutes)
            .ok()
            .and_then(RateWindow::from_minutes)
            .ok_or_else(|| E::invalid_value(de::Unexpected::Unsigned(minutes), &self))
    }

    fn visit_i64<E: de::Error>(self, minutes: i64) -> Result<RateWindow, E> {
        match u64::try_from(minutes) {
            Ok(minutes) => self.visit_u64(minutes),
            Err(_) => Err(E::invalid_value(de::Unexpected::Signed(minutes), &self)),
        }
    }
}
