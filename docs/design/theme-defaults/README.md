# Theme defaults mock (RT-583)

`index.html` draws our native client's main window once per candidate built-in theme, so the owner can choose which themes ship and which one replaces today's near-black Contrast look as the default. Each lane shows the activity rail, the sessions sidebar (two repo groups; a Working, a Needs you, an idle and an exited session), a session ⋯ menu open over the sidebar, two tabs, two terminal panes using the scheme's ANSI colours, and the footer. Under each lane is a list of the token pairs that fall below our contrast floors.

The page needs no media: its only outside requests are Inter and Geist Mono from Google Fonts.

## Sources

- Our tokens and today's look: `apps/native/src/palette.rs` (the Contrast values) and `apps/native/src/ui_theme.rs` (the `UiTheme` fields and the Contrast terminal palette).
- The layout copies a `rt.ps1 native-shot main` capture of the real client.
- The schemes: herdr-gpui (`X:\dev\_ext\penso\herdr-gpui`). Default, Nord, Dracula, Catppuccin Mocha and Catppuccin Latte are its hand-written built-ins (`crates/herdr-gpui/src/config/theme.rs:331-362` and `:549-601`). TokyoNight, Rose Pine, Kanagawa Wave, Everforest Dark Med and One Half Dark are its bundled Ghostty files in `assets/themes/` (MIT, from iTerm2-Color-Schemes; see that repo's `THEMES-NOTICE.md`).

## Derivation

Every derived lane uses herdr-gpui's own chrome rules, built from the scheme's background, foreground and 16-colour palette. `mix(a, b, p)` blends `p`% of `b` onto `a` per channel (`theme.rs:366-373`). `ink(c)` moves a colour's OKLab lightness, keeping its hue, until it reaches 3:1 against the background, surface and active fills (`theme.rs:509-515`, `contrast.rs:154-181`).

| Our token | Rule | herdr-gpui source |
|---|---|---|
| GROUND, TERMINAL_GROUND | background | the scheme |
| SURFACE | `mix(bg, fg, 5)` | `surface`, theme.rs:544 |
| CHIP, HOVER | `mix(bg, fg, 12)` | `active`, theme.rs:545; a hover is `active`, menu/chrome.rs:810 |
| SUBTLE | `mix(bg, fg, 55)` | `muted`, theme.rs:546 |
| TEXT | foreground | the scheme |
| TEXT_2 | `ink(mix(bg, fg, 78))` | `subtext`, theme.rs:473-475 |
| LINE | `mix(bg, fg, 14)` | `border`, browser/kit.rs:46 |
| LINE_STRONG, POPOVER_EDGE | `mix(bg, fg, 24)` | card edge, chat/view/layout/bubbles.rs:33 |
| MENU_RULE | `mix(bg, fg, 22)` | rule line, chat/view/layout/timeline.rs:29 |
| POPOVER | dark: `active`; light: `mix(bg, #fff, 70)` | `raised`, browser/kit.rs:34-39 |
| POPOVER_HOVER | `mix(POPOVER, fg, 12)` | button hover, chat/view/settings.rs:114 |
| ACCENT | `ink(palette[5])`, the magenta | `primary`, theme.rs:452-456 |
| SELECTION | `mix(SURFACE, ACCENT, 22)` | `primary_wash`, theme.rs:485-487 |
| ON_ACCENT, ON_ASKING | bg or fg, whichever contrasts more with the fill | `text_on`, theme.rs:492-503 |
| WORKING / ASKING / WAITING / DANGER | `ink` of palette[4] / [3] / [2] / [1] | Running / Waiting / Ok / Failed, automations/model.rs:36-39 |
| Terminal ANSI 0-15 | the palette, never inked | theme.rs:505-508 |

herdr-gpui has no rule for these, so the mock uses its own:

| Our token | Rule |
|---|---|
| SUNKEN (rail, footer, unfocused pane header) | bg pushed 4% further from fg |
| RAISED, ROW_CARD | `mix(bg, fg, 8)`, between herdr's surface and active |
| FAINT | `mix(bg, fg, 35)` |
| PRIMARY button | `mix(bg, fg, 92)`, label in bg |
| DANGER_TEXT | DANGER blended toward white (black on a light theme) in 5% steps until 4.5:1 on POPOVER and POPOVER_HOVER |

herdr-gpui's Default skips its derivation: its surface, active and muted are fixed (`theme.rs:348-362`), its palette is the xterm VGA set, and its accent is that palette's `#800080`, inked.

## Flags

The list under each lane checks our floors from `palette.rs`'s contrast test (TEXT 7:1 on GROUND; TEXT_2 and SUBTLE 4.5:1 on every surface they sit on), ANSI colours under 3:1 on the terminal ground, colour pairs under 0.12 apart in OKLab, and status pairs whose hues are under 35° apart.

## Outcome

The owner chose Nord as the default theme and the clean set as the built-ins (Contrast, Nord, Catppuccin Mocha and Latte, Rose Pine, Dracula, TokyoNight, One Half Dark). The ruling is recorded on RT-583. `gen_theme_mock.py` regenerates `index.html` from herdr-gpui's scheme files (with `theme_mock.css` and `theme_mock_notes.html`).
