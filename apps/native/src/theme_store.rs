//! The themes the client can draw in: the built-ins compiled in, and the
//! user's, one JSON file per theme in `<config dir>/themes/`, named by its
//! slug. A user theme file is stored resolved: every `ui` token and every
//! `terminal` colour is present, colours as hex strings.

use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::TerminalPalette;
use crate::ui_theme::UiTheme;

/// The folder under the config dir that holds the user's themes.
pub const THEMES_DIR: &str = "themes";

pub use crate::builtin_themes::BUILT_INS;

/// The id of the theme drawn when none is saved or the saved one does not
/// load.
pub const DEFAULT_ID: &str = "nord";

/// The theme [`DEFAULT_ID`] names.
pub const DEFAULT_THEME: UiTheme = crate::builtin_themes::NORD;

/// One theme the store can load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEntry {
    /// A built-in's id or a user theme's file slug.
    pub id: String,
    /// The name it is shown under; a broken file's is its id.
    pub name: String,
    pub built_in: bool,
    /// Why a user theme's file does not load.
    pub error: Option<String>,
    /// The theme it was made from; `None` for a built-in and for an entry
    /// whose file cannot be read.
    pub base: Option<String>,
    /// The theme its terminal colours came from; `None` likewise.
    pub terminal_source: Option<String>,
}

/// Why a theme could not be loaded, saved or deleted.
#[derive(Debug)]
pub enum ThemeError {
    /// No built-in has the id and no file holds it.
    NotFound {
        id: String,
    },
    /// The id holds a character a slug never does.
    InvalidId {
        id: String,
    },
    /// A built-in can be neither saved over nor deleted.
    BuiltIn {
        id: String,
    },
    /// A user theme's file is named after a built-in's id, which the built-in
    /// keeps: the file is listed, never read.
    ShadowsBuiltIn {
        id: String,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not a whole theme: a field missing, a malformed colour.
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    Serialize(serde_json::Error),
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { id } => write!(f, "no theme has the id {id:?}"),
            Self::InvalidId { id } => write!(
                f,
                "{id:?} is not a theme id: only a-z, 0-9 and - are allowed"
            ),
            Self::BuiltIn { id } => write!(f, "{id:?} is a built-in theme and cannot be changed"),
            Self::ShadowsBuiltIn { id } => write!(
                f,
                "themes/{id}.json has the id of a built-in theme; rename the file to use it"
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "{} is not a whole theme: {source}", path.display())
            }
            Self::Serialize(source) => write!(f, "serializing the theme: {source}"),
        }
    }
}

impl std::error::Error for ThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } | Self::Serialize(source) => Some(source),
            Self::NotFound { .. }
            | Self::InvalidId { .. }
            | Self::BuiltIn { .. }
            | Self::ShadowsBuiltIn { .. } => None,
        }
    }
}

/// A user theme file's shape.
#[derive(Debug, Serialize, Deserialize)]
struct ThemeFile {
    name: String,
    /// The theme it was made from; provenance only, never resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    /// The theme its terminal colours came from; provenance only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_source: Option<String>,
    ui: UiTheme,
    terminal: TerminalPalette,
}

impl ThemeFile {
    fn theme(&self) -> UiTheme {
        UiTheme {
            terminal: self.terminal,
            ..self.ui
        }
    }
}

/// The themes in one folder, with the built-ins before them.
#[derive(Debug, Clone)]
pub struct ThemeStore {
    dir: PathBuf,
}

impl ThemeStore {
    /// The store whose user themes live in `dir`, normally
    /// `<config dir>/themes`; the folder is made on the first save.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The built-ins in their fixed order, then the user themes by name,
    /// ignoring case; a file that does not load is listed with its error.
    #[must_use]
    pub fn list(&self) -> Vec<ThemeEntry> {
        let mut entries: Vec<ThemeEntry> = BUILT_INS
            .iter()
            .map(|(id, name, _)| ThemeEntry {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                built_in: true,
                error: None,
                base: None,
                terminal_source: None,
            })
            .collect();
        let mut users = self.user_entries();
        users.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        entries.extend(users);
        entries
    }

    fn user_entries(&self) -> Vec<ThemeEntry> {
        let read = match std::fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    "listing themes in {}: {err}; only the built-ins are listed",
                    self.dir.display()
                );
                return Vec::new();
            }
        };
        read.filter_map(|entry| {
            let path = entry
                .inspect_err(|err| {
                    tracing::warn!("listing themes in {}: {err}", self.dir.display());
                })
                .ok()?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_owned();
            if !valid_id(&id) {
                let error = ThemeError::InvalidId { id: id.clone() }.to_string();
                return Some(broken_entry(id, error));
            }
            if built_in(&id).is_some() {
                let error = ThemeError::ShadowsBuiltIn { id: id.clone() }.to_string();
                return Some(broken_entry(id, error));
            }
            Some(match read_file(&path) {
                Ok(file) => ThemeEntry {
                    id,
                    name: file.name,
                    built_in: false,
                    error: None,
                    base: file.base,
                    terminal_source: file.terminal_source,
                },
                Err(err) => broken_entry(id, err.to_string()),
            })
        })
        .collect()
    }

    /// The theme `id` names: a built-in's, else the user theme in
    /// `<id>.json`.
    pub fn load(&self, id: &str) -> Result<UiTheme, ThemeError> {
        if let Some(theme) = built_in(id) {
            return Ok(theme);
        }
        let path = self.path_of(id)?;
        match read_file(&path) {
            Err(ThemeError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                Err(ThemeError::NotFound { id: id.to_owned() })
            }
            other => other.map(|file| file.theme()),
        }
    }

    /// The name in the user theme `id`'s file, reading that one file;
    /// `None` when it does not load.
    #[must_use]
    pub fn name_of(&self, id: &str) -> Option<String> {
        let path = self.path_of(id).ok()?;
        read_file(&path).ok().map(|file| file.name)
    }

    /// Saves `theme` as a new user theme called `name`, returning its id:
    /// the name's slug, with `-2`, `-3`, … when a built-in or a file has it.
    pub fn save(
        &self,
        name: &str,
        theme: &UiTheme,
        base: Option<&str>,
        terminal_source: Option<&str>,
    ) -> Result<String, ThemeError> {
        let id = self.free_slug(&slug(name));
        let file = ThemeFile {
            name: name.to_owned(),
            base: base.map(str::to_owned),
            terminal_source: terminal_source.map(str::to_owned),
            ui: *theme,
            terminal: theme.terminal,
        };
        let json = serde_json::to_vec_pretty(&file).map_err(ThemeError::Serialize)?;
        std::fs::create_dir_all(&self.dir).map_err(|source| ThemeError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let path = self.dir.join(format!("{id}.json"));
        let tmp = self.dir.join(format!(".{id}.json.tmp"));
        write_then_rename(&tmp, &path, &json)?;
        Ok(id)
    }

    /// Deletes the user theme `id`.
    pub fn delete(&self, id: &str) -> Result<(), ThemeError> {
        if built_in(id).is_some() {
            return Err(ThemeError::BuiltIn { id: id.to_owned() });
        }
        let path = self.path_of(id)?;
        std::fs::remove_file(&path).map_err(|source| {
            if source.kind() == ErrorKind::NotFound {
                ThemeError::NotFound { id: id.to_owned() }
            } else {
                ThemeError::Io { path, source }
            }
        })
    }

    /// The theme a saved selection draws: the default built-in,
    /// [`DEFAULT_THEME`], when `id` is `None`; the default, with a warning,
    /// when `id` names nothing or its file does not load.
    #[must_use]
    pub fn resolve(&self, id: Option<&str>) -> UiTheme {
        let Some(id) = id else {
            return DEFAULT_THEME;
        };
        self.load(id).unwrap_or_else(|err| {
            tracing::warn!(
                "the saved theme {id:?} does not load: {err}; drawing the built-in theme {DEFAULT_ID:?}"
            );
            DEFAULT_THEME
        })
    }

    fn path_of(&self, id: &str) -> Result<PathBuf, ThemeError> {
        if valid_id(id) {
            Ok(self.dir.join(format!("{id}.json")))
        } else {
            Err(ThemeError::InvalidId { id: id.to_owned() })
        }
    }

    /// `slug`, or the first of `slug-2`, `slug-3`, … that no built-in and
    /// no file has.
    fn free_slug(&self, slug: &str) -> String {
        let taken =
            |id: &str| built_in(id).is_some() || self.dir.join(format!("{id}.json")).exists();
        if !taken(slug) {
            return slug.to_owned();
        }
        let mut n = 2_u32;
        loop {
            let candidate = format!("{slug}-{n}");
            if !taken(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }
}

/// The entry for a user theme file that cannot be listed as a theme: its
/// error, under the id its name gives it.
fn broken_entry(id: String, error: String) -> ThemeEntry {
    ThemeEntry {
        name: id.clone(),
        id,
        built_in: false,
        error: Some(error),
        base: None,
        terminal_source: None,
    }
}

/// The theme a saved selection draws, from the user themes in
/// `themes_dir` when there is one; see [`ThemeStore::resolve`].
#[must_use]
pub fn resolve_selection(themes_dir: Option<&Path>, id: Option<&str>) -> UiTheme {
    match themes_dir {
        Some(dir) => ThemeStore::new(dir.to_path_buf()).resolve(id),
        None => match id.map(|id| (id, built_in(id))) {
            None => DEFAULT_THEME,
            Some((_, Some(theme))) => theme,
            Some((id, None)) => {
                tracing::warn!(
                    "the saved theme {id:?} does not load: no config dir; drawing the built-in theme {DEFAULT_ID:?}"
                );
                DEFAULT_THEME
            }
        },
    }
}

/// The theme `id` names: [`ThemeStore::load`]'s answer from the user themes
/// in `themes_dir` when there is one, else a built-in's, else no theme.
pub fn load_selection(themes_dir: Option<&Path>, id: &str) -> Result<UiTheme, ThemeError> {
    match themes_dir {
        Some(dir) => ThemeStore::new(dir.to_path_buf()).load(id),
        None => built_in(id).ok_or_else(|| ThemeError::NotFound { id: id.to_owned() }),
    }
}

/// Whether `id` can name a user theme file: only `a-z`, `0-9` and `-`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn built_in(id: &str) -> Option<UiTheme> {
    BUILT_INS
        .iter()
        .find(|(built_in, _, _)| *built_in == id)
        .map(|(_, _, theme)| *theme)
}

/// Writes `bytes` to `tmp`, then renames it to `path`. A failed write or
/// rename removes `tmp`.
fn write_then_rename(tmp: &Path, path: &Path, bytes: &[u8]) -> Result<(), ThemeError> {
    let result = std::fs::write(tmp, bytes)
        .map_err(|source| ThemeError::Io {
            path: tmp.to_path_buf(),
            source,
        })
        .and_then(|()| {
            std::fs::rename(tmp, path).map_err(|source| ThemeError::Io {
                path: path.to_path_buf(),
                source,
            })
        });
    if result.is_err() {
        remove_temp(tmp);
    }
    result
}

/// Removes a failed save's temp file, logging why when it cannot.
fn remove_temp(tmp: &Path) {
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => tracing::warn!("removing {} after a failed save: {err}", tmp.display()),
    }
}

fn read_file(path: &Path) -> Result<ThemeFile, ThemeError> {
    let text = std::fs::read_to_string(path).map_err(|source| ThemeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| ThemeError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// `name` lower-cased, each run of characters outside `a-z0-9` one `-`,
/// no `-` at either end; `theme` when nothing is left.
fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    if out.is_empty() {
        "theme".to_owned()
    } else {
        out
    }
}

/// Serde helpers for colours stored as hex strings: `#rrggbb`, and
/// `#rrggbbaa` for a token that carries alpha.
pub(crate) mod hex {
    use alacritty_terminal::vte::ansi::Rgb;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    /// `raw` as a `u32` when it is `#` and exactly `digits` hex digits.
    fn parse<'de, D: Deserializer<'de>>(raw: &str, digits: usize) -> Result<u32, D::Error> {
        raw.strip_prefix('#')
            .filter(|hex| hex.len() == digits && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .ok_or_else(|| {
                D::Error::custom(format!(
                    "{raw:?} is not a colour: expected # and {digits} hex digits"
                ))
            })
    }

    /// An opaque `0xRRGGBB` token as `#rrggbb`.
    pub mod rgb {
        use super::{Deserialize, Deserializer, Serializer, parse};

        #[expect(
            clippy::trivially_copy_pass_by_ref,
            reason = "serde's `with` signature takes the field by reference"
        )]
        pub fn serialize<S: Serializer>(value: &u32, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&format!("#{:06x}", value & 0x00ff_ffff))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
            parse::<D>(&String::deserialize(d)?, 6)
        }
    }

    /// A `0xRRGGBBAA` token as `#rrggbbaa`.
    pub mod rgba {
        use super::{Deserialize, Deserializer, Serializer, parse};

        #[expect(
            clippy::trivially_copy_pass_by_ref,
            reason = "serde's `with` signature takes the field by reference"
        )]
        pub fn serialize<S: Serializer>(value: &u32, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&format!("#{value:08x}"))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
            parse::<D>(&String::deserialize(d)?, 8)
        }
    }

    fn term_hex(color: Rgb) -> String {
        format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
    }

    fn term_parse<'de, D: Deserializer<'de>>(raw: &str) -> Result<Rgb, D::Error> {
        let [_, r, g, b] = parse::<D>(raw, 6)?.to_be_bytes();
        Ok(Rgb { r, g, b })
    }

    /// A terminal colour as `#rrggbb`.
    pub mod term {
        use super::{Deserialize, Deserializer, Rgb, Serializer, term_hex, term_parse};

        #[expect(
            clippy::trivially_copy_pass_by_ref,
            reason = "serde's `with` signature takes the field by reference"
        )]
        pub fn serialize<S: Serializer>(value: &Rgb, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&term_hex(*value))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Rgb, D::Error> {
            term_parse::<D>(&String::deserialize(d)?)
        }
    }

    /// The 16 ANSI colours as a list of `#rrggbb`.
    pub mod term_ansi {
        use super::{Deserialize, Deserializer, Rgb, Serializer, term_hex, term_parse};
        use serde::de::Error as _;
        use serde::ser::SerializeSeq as _;

        pub fn serialize<S: Serializer>(value: &[Rgb; 16], s: S) -> Result<S::Ok, S::Error> {
            let mut seq = s.serialize_seq(Some(value.len()))?;
            for color in value {
                seq.serialize_element(&term_hex(*color))?;
            }
            seq.end()
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[Rgb; 16], D::Error> {
            let raw = Vec::<String>::deserialize(d)?;
            let colors = raw
                .iter()
                .map(|c| term_parse::<D>(c))
                .collect::<Result<Vec<_>, _>>()?;
            let count = colors.len();
            colors
                .try_into()
                .map_err(|_| D::Error::custom(format!("expected 16 ANSI colours, got {count}")))
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
#[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rt-native-themes-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        fn store(&self) -> ThemeStore {
            ThemeStore::new(self.0.clone())
        }

        fn file(&self, id: &str) -> PathBuf {
            self.0.join(format!("{id}.json"))
        }

        fn read(&self, id: &str) -> Value {
            let text = std::fs::read_to_string(self.file(id)).expect("the theme file");
            serde_json::from_str(&text).expect("the theme file is JSON")
        }

        fn write(&self, id: &str, value: &Value) {
            std::fs::write(self.file(id), value.to_string()).expect("write the theme file");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ids(entries: &[ThemeEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn a_saved_theme_loads_back_equal() {
        let dir = TestDir::new("round-trip");
        let store = dir.store();
        let id = store
            .save(
                "My theme",
                &UiTheme::CONTRAST,
                Some("contrast"),
                Some("contrast"),
            )
            .expect("save");
        assert_eq!(id, "my-theme");
        assert_eq!(store.load(&id).expect("load"), UiTheme::CONTRAST);
        let file = dir.read(&id);
        assert_eq!(file["name"], "My theme");
        assert_eq!(file["base"], "contrast");
        assert_eq!(file["terminal_source"], "contrast");
        assert_eq!(file["ui"]["ground"], "#08070a");
        assert_eq!(file["terminal"]["bg"], "#070609");
        assert_eq!(file["terminal"]["ansi"].as_array().map(Vec::len), Some(16));
        assert!(
            file["ui"].get("terminal").is_none(),
            "the terminal is its own part"
        );
    }

    #[test]
    fn a_name_taken_by_a_built_in_or_a_file_gets_a_suffix() {
        let dir = TestDir::new("suffix");
        let store = dir.store();
        let t = UiTheme::CONTRAST;
        let save = |name: &str| store.save(name, &t, None, None).expect("save");
        assert_eq!(save("Contrast"), "contrast-2", "a built-in's id");
        assert_eq!(save("  Late Night!  Blue "), "late-night-blue");
        assert_eq!(save("late night blue"), "late-night-blue-2", "a file's id");
        assert_eq!(save("Late-Night-Blue"), "late-night-blue-3");
        assert_eq!(save("!!!"), "theme", "an empty slug");
        assert_eq!(save("Ünïcode"), "n-code");
    }

    #[test]
    fn a_file_missing_a_field_is_refused() {
        let dir = TestDir::new("missing");
        let store = dir.store();
        let id = store
            .save("Mine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        let whole = dir.read(&id);
        let mut no_ground = whole.clone();
        no_ground["ui"]
            .as_object_mut()
            .expect("ui")
            .remove("ground");
        dir.write(&id, &no_ground);
        let err = store.load(&id).expect_err("no ui ground");
        assert!(matches!(err, ThemeError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("ground"), "{err}");

        let mut no_fg = whole.clone();
        no_fg["terminal"]
            .as_object_mut()
            .expect("terminal")
            .remove("fg");
        dir.write(&id, &no_fg);
        assert!(matches!(store.load(&id), Err(ThemeError::Parse { .. })));

        let mut extra = whole;
        extra["ui"]["not_a_token"] = Value::from("#000000");
        extra["comment"] = Value::from("unknown keys are ignored");
        dir.write(&id, &extra);
        assert_eq!(store.load(&id).expect("load"), UiTheme::CONTRAST);
    }

    #[test]
    fn a_malformed_colour_is_refused() {
        let dir = TestDir::new("malformed");
        let store = dir.store();
        let id = store
            .save("Mine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        let whole = dir.read(&id);
        let bad = [
            ("ui", "ground", Value::from("#12345")),
            ("ui", "ground", Value::from("red")),
            ("ui", "ground", Value::from("#08070aff")),
            ("ui", "row_card", Value::from("#16151b")),
            ("ui", "text", Value::from(0xede9e4)),
            ("terminal", "fg", Value::from("#zzzzzz")),
        ];
        for (part, field, value) in bad {
            let mut file = whole.clone();
            file[part][field] = value.clone();
            dir.write(&id, &file);
            let err = store.load(&id).expect_err("a malformed colour");
            assert!(
                matches!(err, ThemeError::Parse { .. }),
                "{part}.{field} = {value}: {err}"
            );
        }
        let mut short = whole;
        short["terminal"]["ansi"]
            .as_array_mut()
            .expect("ansi")
            .pop();
        dir.write(&id, &short);
        let err = store.load(&id).expect_err("15 ANSI colours");
        assert!(err.to_string().contains("16"), "{err}");
    }

    /// The eight built-ins' ids, in list order.
    const BUILT_IN_IDS: [&str; 8] = [
        "contrast",
        "nord",
        "catppuccin-mocha",
        "catppuccin-latte",
        "rose-pine",
        "dracula",
        "tokyonight",
        "one-half-dark",
    ];

    /// The user entries of `entries`, after the built-ins.
    fn users(entries: &[ThemeEntry]) -> &[ThemeEntry] {
        &entries[BUILT_INS.len()..]
    }

    #[test]
    fn list_puts_the_eight_built_ins_first_in_order() {
        let dir = TestDir::new("built-ins");
        let store = dir.store();
        store
            .save("Aardvark", &UiTheme::CONTRAST, None, None)
            .expect("save");
        let entries = store.list();
        assert_eq!(ids(&entries[..8]), BUILT_IN_IDS);
        let names: Vec<&str> = entries[..8].iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Contrast",
                "Nord",
                "Catppuccin Mocha",
                "Catppuccin Latte",
                "Rose Pine",
                "Dracula",
                "TokyoNight",
                "One Half Dark",
            ]
        );
        assert!(entries[..8].iter().all(|e| e.built_in && e.error.is_none()));
        assert_eq!(ids(&entries[8..]), ["aardvark"], "user themes come after");
        for id in BUILT_IN_IDS {
            assert!(store.path_of(id).is_ok(), "{id} is a valid id");
            assert!(store.load(id).is_ok(), "{id} loads");
        }
    }

    #[test]
    fn every_built_in_round_trips_through_the_file_format() {
        let dir = TestDir::new("built-in-round-trip");
        let store = dir.store();
        for (id, name, theme) in BUILT_INS {
            let saved = store.save(name, theme, Some(id), Some(id)).expect("save");
            assert_ne!(&saved, id, "a built-in's id is never a file's");
            assert_eq!(&store.load(&saved).expect("load"), theme, "{id}");
        }
    }

    #[test]
    fn list_puts_built_ins_first_then_user_themes_by_name() {
        let dir = TestDir::new("list");
        let store = dir.store();
        assert_eq!(ids(&store.list()), BUILT_IN_IDS, "no folder yet");
        let t = UiTheme::CONTRAST;
        for name in ["beta", "Alpha", "gamma"] {
            store.save(name, &t, None, None).expect("save");
        }
        std::fs::write(dir.0.join("notes.txt"), "not a theme").expect("write");
        let entries = store.list();
        assert_eq!(ids(&entries[..8]), BUILT_IN_IDS);
        assert_eq!(ids(users(&entries)), ["alpha", "beta", "gamma"]);
        assert_eq!(entries[0].name, "Contrast");
        assert!(entries[0].built_in);
        assert_eq!(users(&entries)[0].name, "Alpha");
        assert!(
            users(&entries)
                .iter()
                .all(|e| !e.built_in && e.error.is_none())
        );
    }

    #[test]
    fn list_shows_a_broken_file_with_its_error() {
        let dir = TestDir::new("broken");
        let store = dir.store();
        store
            .save("Fine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        std::fs::write(dir.file("broken"), "{").expect("write");
        let entries = store.list();
        let users = users(&entries);
        assert_eq!(ids(users), ["broken", "fine"]);
        let broken = &users[0];
        assert_eq!(broken.name, "broken");
        assert!(!broken.built_in);
        let error = broken.error.as_deref().expect("its error");
        assert!(error.contains("broken.json"), "{error}");
        assert_eq!(users[1].error, None);
    }

    #[test]
    fn a_file_with_an_invalid_id_is_listed_with_its_error() {
        let dir = TestDir::new("invalid-id");
        let store = dir.store();
        let id = store
            .save("Fine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        std::fs::copy(dir.file(&id), dir.0.join("Dark Mode.json")).expect("copy");
        let entries = store.list();
        let users = users(&entries);
        assert_eq!(ids(users), ["Dark Mode", "fine"]);
        let dark = &users[0];
        assert_eq!(
            dark.name, "Dark Mode",
            "its name is its id, the file unread"
        );
        assert!(!dark.built_in);
        let error = dark.error.as_deref().expect("its error");
        assert!(error.contains("is not a theme id"), "{error}");
        assert_eq!(users[1].error, None);
        assert!(matches!(
            store.load("Dark Mode"),
            Err(ThemeError::InvalidId { .. })
        ));
    }

    #[test]
    fn a_failed_save_leaves_no_temp_file() {
        let dir = TestDir::new("failed-save");
        std::fs::create_dir_all(dir.0.join("mine.json")).expect("a folder where the file goes");
        let tmp = dir.0.join(".mine.json.tmp");
        let err =
            write_then_rename(&tmp, &dir.file("mine"), b"{}").expect_err("a folder is in the way");
        assert!(matches!(err, ThemeError::Io { .. }), "{err}");
        assert!(!tmp.exists(), "the temp file is removed");
        assert!(dir.file("mine").is_dir(), "the folder is left alone");
    }

    #[test]
    fn built_ins_cannot_be_saved_over_or_deleted() {
        let dir = TestDir::new("built-in");
        let store = dir.store();
        let err = store.delete("contrast").expect_err("a built-in");
        assert!(matches!(err, ThemeError::BuiltIn { .. }), "{err}");
        let mut other = UiTheme::CONTRAST;
        other.ground = 0x123456;
        let id = store.save("contrast", &other, None, None).expect("save");
        assert_ne!(id, "contrast");
        assert!(!dir.file("contrast").exists());
        assert_eq!(store.load("contrast").expect("load"), UiTheme::CONTRAST);

        store.delete(&id).expect("a user theme deletes");
        assert!(matches!(store.load(&id), Err(ThemeError::NotFound { .. })));
        assert!(matches!(
            store.delete(&id),
            Err(ThemeError::NotFound { .. })
        ));
        assert!(matches!(
            store.load("../native-ui"),
            Err(ThemeError::InvalidId { .. })
        ));
    }

    #[test]
    fn alpha_tokens_round_trip_with_their_alpha() {
        let dir = TestDir::new("alpha");
        let store = dir.store();
        let mut t = UiTheme::CONTRAST;
        t.row_card = 0x1234_5678;
        t.scrim = 0xabcd_ef80;
        t.transparent = 0;
        t.diff_word_wash_alpha = 0.42;
        let id = store.save("Alpha", &t, None, None).expect("save");
        let file = dir.read(&id);
        assert_eq!(file["ui"]["row_card"], "#12345678");
        assert_eq!(file["ui"]["scrim"], "#abcdef80");
        assert_eq!(file["ui"]["transparent"], "#00000000");
        assert_eq!(file["ui"]["ground"], "#08070a");
        assert_eq!(store.load(&id).expect("load"), t);
    }

    #[test]
    fn a_file_named_after_a_built_in_lists_with_an_error() {
        let dir = TestDir::new("shadows-built-in");
        let store = dir.store();
        let id = store
            .save("Mine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        std::fs::copy(dir.file(&id), dir.0.join("nord.json")).expect("copy");
        let entries = store.list();
        assert_eq!(entries[1].id, "nord");
        assert!(entries[1].built_in);
        assert_eq!(
            entries[1].error, None,
            "the built-in lists as it always did"
        );
        let shadow = users(&entries)
            .iter()
            .find(|entry| entry.id == "nord")
            .expect("the file's row");
        assert!(!shadow.built_in);
        assert_eq!(shadow.name, "nord", "its name is its id, the file unread");
        let error = shadow.error.as_deref().expect("its error");
        assert!(error.contains("nord.json"), "{error}");
        assert!(error.contains("built-in"), "{error}");
        assert_eq!(
            store.load("nord").expect("load"),
            crate::builtin_themes::NORD,
            "the built-in is the theme the id loads"
        );
    }

    #[test]
    fn a_listed_user_theme_carries_its_base_and_terminal_source() {
        let dir = TestDir::new("provenance");
        let store = dir.store();
        let id = store
            .save(
                "Mine",
                &UiTheme::CONTRAST,
                Some("contrast"),
                Some("dracula"),
            )
            .expect("save");
        let entries = store.list();
        let mine = users(&entries)
            .iter()
            .find(|entry| entry.id == id)
            .expect("the saved theme");
        assert_eq!(mine.base.as_deref(), Some("contrast"));
        assert_eq!(mine.terminal_source.as_deref(), Some("dracula"));
    }

    #[test]
    fn a_built_in_lists_no_base_or_terminal_source() {
        let dir = TestDir::new("built-in-provenance");
        let store = dir.store();
        let entries = store.list();
        for entry in &entries[..BUILT_INS.len()] {
            assert_eq!(entry.base, None, "{}", entry.id);
            assert_eq!(entry.terminal_source, None, "{}", entry.id);
        }
    }

    #[test]
    fn name_of_reads_one_file() {
        let dir = TestDir::new("name-of");
        let store = dir.store();
        let id = store
            .save("Harbour", &UiTheme::CONTRAST, None, None)
            .expect("save");
        std::fs::write(dir.file("broken"), "{").expect("write");

        assert_eq!(store.name_of(&id).as_deref(), Some("Harbour"));
        assert_eq!(store.name_of("broken"), None, "a file that does not load");
        assert_eq!(store.name_of("missing"), None, "no file");
        assert_eq!(store.name_of("../escape"), None, "not an id");
    }
}
