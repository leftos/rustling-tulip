//! The store's settings file, `settings.json`: a missing file is the defaults, a read is strict and names the field it
//! refuses, and a write is checked first, then made under its lock through a temp file.

use std::fs;

use claude_accounts::{
    AutoSwitchSection, AutoSwitchSettings, FleetSettings, RateWindow, Settings, Strategy,
    StrategyName,
};

use crate::support::Scratch;

/// Writes `text` as the settings file of `scratch`'s store and reads it back, the error's text on failure.
fn read_text(scratch: &Scratch, text: &str) -> Result<Settings, String> {
    fs::create_dir_all(scratch.backup()).expect("the store's root can be made");
    fs::write(scratch.root().settings(), text).expect("the settings file can be written");
    Settings::read(&scratch.root().settings()).map_err(|error| error.to_string())
}

/// Settings unlike the defaults in every field.
fn changed() -> Settings {
    Settings {
        autoswitch: AutoSwitchSection {
            enabled: false,
            threshold: 80.5,
            strategy: StrategyName::ConsumeFirst,
            models: vec!["opus".to_owned(), "all".to_owned()],
            interval_seconds: 120.0,
            cooldown_seconds: 0.0,
            hysteresis_pct: 25.0,
            unhealthy_ticks: 7,
        },
        fleet: FleetSettings {
            rate_window: RateWindow::TwoHours,
        },
    }
}

#[test]
fn settings_missing_file_reads_defaults() {
    let scratch = Scratch::empty("settings_missing_file_reads_defaults");

    let settings = Settings::read(&scratch.root().settings()).expect("a missing file reads");

    assert_eq!(settings, Settings::default());
    assert!(settings.autoswitch.enabled, "auto-switch defaults to on");
    assert_eq!(settings.autoswitch.strategy, StrategyName::Best);
    assert_eq!(settings.autoswitch.unhealthy_ticks, 3);
    assert_eq!(settings.fleet.rate_window, RateWindow::OneHour);
    assert_eq!(
        settings.autoswitch.to_engine(),
        AutoSwitchSettings::default()
    );
}

#[test]
fn settings_round_trip() {
    let scratch = Scratch::empty("settings_round_trip");
    let root = scratch.root();

    Settings::write(&root, &changed()).expect("the settings are written");
    assert_eq!(
        Settings::read(&root.settings()).expect("the settings read back"),
        changed()
    );
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(root.settings()).expect("the file reads"))
            .expect("the file is JSON");
    assert_eq!(written["autoswitch"]["strategy"], "consume-first");
    assert_eq!(written["autoswitch"]["intervalSeconds"], 120.0);
    assert_eq!(written["fleet"]["rateWindowMinutes"], 120);

    Settings::write(&root, &Settings::default()).expect("the defaults are written");
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(root.settings()).expect("the file reads"))
            .expect("the file is JSON");
    let sections: Vec<&str> = written
        .as_object()
        .expect("the file is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        sections,
        ["autoswitch", "fleet"],
        "both sections are always written"
    );
    assert_eq!(
        Settings::read(&root.settings()).expect("the defaults read back"),
        Settings::default()
    );
}

#[test]
fn settings_unknown_field_is_refused() {
    let scratch = Scratch::empty("settings_unknown_field_is_refused");

    let error = read_text(&scratch, r#"{"autoswitch":{"treshold":80}}"#)
        .expect_err("an unknown autoswitch field is refused");
    assert!(error.contains("autoswitch.treshold"), "{error}");

    let error = read_text(&scratch, r#"{"fleet":{"rateWindowMinutes":60,"extra":1}}"#)
        .expect_err("an unknown fleet field is refused");
    assert!(error.contains("fleet.extra"), "{error}");

    let error = read_text(&scratch, r#"{"daemon":{"enabled":true}}"#)
        .expect_err("an unknown section is refused");
    assert!(error.contains("daemon"), "{error}");

    let error = read_text(&scratch, r#"{"autoswitch":{"threshold":"high"}}"#)
        .expect_err("a bad type is refused");
    assert!(error.contains("autoswitch.threshold"), "{error}");

    let error = read_text(&scratch, r#"{"fleet":{"rateWindowMinutes":45}}"#)
        .expect_err("a window outside the choices is refused");
    assert!(error.contains("fleet.rateWindowMinutes"), "{error}");
}

#[test]
fn settings_out_of_range_threshold_is_refused() {
    let scratch = Scratch::empty("settings_out_of_range_threshold_is_refused");

    let error = read_text(&scratch, r#"{"autoswitch":{"threshold":49}}"#)
        .expect_err("a threshold under 50 is refused");
    assert!(error.contains("autoswitch.threshold"), "{error}");
    assert!(error.contains("outside 50 to 99.9"), "{error}");

    let error = read_text(&scratch, r#"{"autoswitch":{"threshold":100}}"#)
        .expect_err("a threshold over 99.9 is refused");
    assert!(error.contains("autoswitch.threshold"), "{error}");

    let read = read_text(&scratch, r#"{"autoswitch":{"threshold":99.9}}"#)
        .expect("the top of the range reads");
    let mut expected = AutoSwitchSettings::default();
    expected.threshold = 99.9;
    assert_eq!(read.autoswitch.to_engine(), expected);
}

#[test]
fn settings_duplicate_models_ignoring_case_are_refused() {
    let scratch = Scratch::empty("settings_duplicate_models_ignoring_case_are_refused");

    let error = read_text(&scratch, r#"{"autoswitch":{"models":["opus","Opus"]}}"#)
        .expect_err("a model named twice ignoring case is refused");
    assert!(error.contains("autoswitch.models"), "{error}");
    assert!(error.contains("twice"), "{error}");

    read_text(&scratch, r#"{"autoswitch":{"models":["opus","sonnet"]}}"#)
        .expect("two different models read");
}

#[test]
fn settings_write_refuses_invalid_without_touching_file() {
    let scratch = Scratch::empty("settings_write_refuses_invalid_without_touching_file");
    let root = scratch.root();
    Settings::write(&root, &changed()).expect("valid settings are written");
    let before = fs::read(root.settings()).expect("the file reads");
    let listing = || {
        let mut names: Vec<String> = fs::read_dir(root.root())
            .expect("the root lists")
            .map(|entry| {
                entry
                    .expect("an entry reads")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    };
    let files = listing();

    let mut invalid = changed();
    invalid.autoswitch.threshold = 120.0;
    let error = Settings::write(&root, &invalid)
        .expect_err("an out-of-range threshold is refused")
        .to_string();
    assert!(error.contains("autoswitch.threshold"), "{error}");

    let mut invalid = changed();
    invalid.autoswitch.models = vec!["Opus".to_owned(), "opus".to_owned()];
    Settings::write(&root, &invalid).expect_err("duplicate models are refused");

    assert_eq!(fs::read(root.settings()).expect("the file reads"), before);
    assert_eq!(listing(), files, "no temp file is left");
}

#[test]
fn autoswitch_section_maps_to_engine_settings() {
    let engine = changed().autoswitch.to_engine();

    let mut expected = AutoSwitchSettings::default();
    expected.threshold = 80.5;
    expected.strategy = Strategy::ConsumeFirst;
    expected.models = vec!["opus".to_owned(), "all".to_owned()];
    expected.interval_seconds = 120.0;
    expected.cooldown_seconds = 0.0;
    expected.hysteresis_pct = 25.0;
    expected.unhealthy_ticks = 7;
    assert_eq!(engine, expected);
    assert_eq!(
        AutoSwitchSection::default().to_engine(),
        AutoSwitchSettings::default()
    );
}
