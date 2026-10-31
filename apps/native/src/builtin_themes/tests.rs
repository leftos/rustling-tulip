use super::*;
use crate::contrast;

#[test]
fn every_built_in_passes_the_contrast_check() {
    let mut short = Vec::new();
    for (id, _, theme) in BUILT_INS {
        let results = contrast::check(theme);
        assert_eq!(
            results.len(),
            contrast::PAIRS.len(),
            "{id}: every pair is measured"
        );
        short.extend(results.iter().filter(|r| !r.passes).map(|r| {
            format!(
                "{id}: {} on {} is {:.2}:1, below {}:1",
                r.fg, r.bg, r.ratio, r.minimum
            )
        }));
    }
    assert!(short.is_empty(), "{short:#?}");
}

#[test]
fn contrast_is_unchanged() {
    let (id, name, theme) = BUILT_INS[0];
    assert_eq!((id, name), ("contrast", "Contrast"));
    assert_eq!(theme, UiTheme::CONTRAST);
}
