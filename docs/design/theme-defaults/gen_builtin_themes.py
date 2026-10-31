"""Generate apps/native/src/builtin_themes.rs: the built-in themes beside Contrast, each derived from its terminal scheme.

Run from anywhere, naming herdr-gpui's Ghostty theme folder:

    uv run --no-project python docs/design/theme-defaults/gen_builtin_themes.py X:/dev/_ext/penso/herdr-gpui/assets/themes

The derivation is the mock's (`gen_theme_mock.py` `derive()`, its helpers copied here because that script renders its
page on import), plus the rules for the fields the mock does not draw, and a solver that holds every quiet-text pair at
4.5:1. It prints each theme's solved shares and exits non-zero when a theme misses a floor it is meant to hold.
"""

import math
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / "apps" / "native" / "src" / "builtin_themes.rs"
COMMAND = "uv run --no-project python docs/design/theme-defaults/gen_builtin_themes.py <herdr-gpui>/assets/themes"

TEXT_FLOOR = 4.5
GLYPH_FLOOR = 3.0


# ---------- colour maths (copied from gen_theme_mock.py) ----------

def ch(c):
    return [((c >> s) & 255) / 255 for s in (16, 8, 0)]


def pack(rgb):
    out = 0
    for v in rgb:
        out = (out << 8) | int(round(min(1, max(0, v)) * 255))
    return out


def to_lin(v):
    return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4


def from_lin(v):
    return v * 12.92 if v <= 0.0031308 else 1.055 * v ** (1 / 2.4) - 0.055


def lum(c):
    r, g, b = (to_lin(v) for v in ch(c))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def ratio(a, b):
    la, lb = lum(a), lum(b)
    return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)


def worst(c, bgs):
    return min(ratio(c, b) for b in bgs)


def cbrt(x):
    return math.copysign(abs(x) ** (1 / 3), x)


def to_oklab(c):
    r, g, b = (to_lin(v) for v in ch(c))
    l = cbrt(0.41222146 * r + 0.53633255 * g + 0.051445995 * b)
    m = cbrt(0.2119035 * r + 0.6806995 * g + 0.10739696 * b)
    s = cbrt(0.08830246 * r + 0.28171885 * g + 0.6299787 * b)
    return [
        0.21045426 * l + 0.7936178 * m - 0.004072047 * s,
        1.9779985 * l - 2.4285922 * m + 0.4505937 * s,
        0.025904037 * l + 0.78277177 * m - 0.80867577 * s,
    ]


def from_oklab(lab):
    L, a, b = lab
    l = (L + 0.39633778 * a + 0.21580376 * b) ** 3
    m = (L - 0.105561346 * a - 0.06385417 * b) ** 3
    s = (L - 0.08948418 * a - 1.2914855 * b) ** 3
    rgb = [
        4.0767417 * l - 3.3077116 * m + 0.23096994 * s,
        -1.268438 * l + 2.6097574 * m - 0.34131938 * s,
        -0.0041960863 * l - 0.7034186 * m + 1.7076147 * s,
    ]
    return pack([from_lin(min(1, max(0, v))) for v in rgb])


def ink(c, bgs, target=GLYPH_FLOOR):
    """OKLab lightness bisection keeping hue, chroma gain 1.5, until `c` reaches `target` on every one of `bgs`."""
    if worst(c, bgs) >= target:
        return c
    darken = worst(0x000000, bgs) >= worst(0xFFFFFF, bgs)
    pole = 0x000000 if darken else 0xFFFFFF
    L, a, b = to_oklab(c)
    span = L if darken else 1 - L
    if span <= 1e-7:
        return pole
    near, far = L, (0.0 if darken else 1.0)
    found = pole
    for _ in range(16):
        mid = (near + far) / 2
        gain = 1 + 1.5 * abs(mid - L) / span
        cand = from_oklab([mid, a * gain, b * gain])
        if worst(cand, bgs) >= target:
            found, far = cand, mid
        else:
            near = mid
    return found


def mix(base, over, pct):
    """Integer per-channel blend of `pct`% of `over` onto `base`."""
    out = 0
    for s in (16, 8, 0):
        b, o = (base >> s) & 255, (over >> s) & 255
        out |= ((b * (100 - pct) + o * pct) // 100) << s
    return out


def away(base, other, pct):
    """`base` pushed `pct`% further from `other` (extrapolated, clipped)."""
    out = 0
    for s in (16, 8, 0):
        b, o = (base >> s) & 255, (other >> s) & 255
        v = round(b + (b - o) * pct / 100)
        out |= max(0, min(255, v)) << s
    return out


def toward_floor(c, bgs, light, floor=TEXT_FLOOR):
    """`c` blended toward white (black on a light theme) in 5% steps until it reaches `floor` on every one of `bgs`."""
    pole = 0x000000 if light else 0xFFFFFF
    for pct in range(0, 101, 5):
        cand = mix(c, pole, pct)
        if worst(cand, bgs) >= floor:
            return cand
    return pole


def text_on(theme_bg, theme_fg, fill):
    """The theme's bg or fg, whichever is further from `fill` in gamma-encoded luminance."""
    def l(c):
        r, g, b = ch(c)
        return 0.2126 * r + 0.7152 * g + 0.0722 * b
    f = l(fill)
    return theme_bg if abs(l(theme_bg) - f) >= abs(l(theme_fg) - f) else theme_fg


# ---------- theme sources ----------

# herdr-gpui crates/herdr-gpui/src/config/theme.rs:553-584, the hand-built schemes.
HAND_BUILT = {
    "Nord": (0x2E3440, 0xD8DEE9, [0x3B4252, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x88C0D0, 0xE5E9F0,
                                  0x4C566A, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x8FBCBB, 0xECEFF4]),
    "Dracula": (0x282A36, 0xF8F8F2, [0x21222C, 0xFF5555, 0x50FA7B, 0xF1FA8C, 0xBD93F9, 0xFF79C6, 0x8BE9FD, 0xF8F8F2,
                                     0x6272A4, 0xFF6E6E, 0x69FF94, 0xFFFFA5, 0xD6ACFF, 0xFF92DF, 0xA4FFFF, 0xFFFFFF]),
    "Catppuccin Mocha": (0x1E1E2E, 0xCDD6F4, [0x45475A, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5,
                                              0xBAC2DE, 0x585B70, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7,
                                              0x94E2D5, 0xA6ADC8]),
    "Catppuccin Latte": (0xEFF1F5, 0x4C4F69, [0x5C5F77, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB, 0x179299,
                                              0xACB0BE, 0x6C6F85, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB,
                                              0x179299, 0xBCC0CC]),
}

# The built-ins after Contrast, in list order: id, display name, where the scheme comes from.
THEMES = [
    ("nord", "Nord", "hand"),
    ("catppuccin-mocha", "Catppuccin Mocha", "hand"),
    ("catppuccin-latte", "Catppuccin Latte", "hand"),
    ("rose-pine", "Rose Pine", "ghostty"),
    ("dracula", "Dracula", "hand"),
    ("tokyonight", "TokyoNight", "ghostty"),
    ("one-half-dark", "One Half Dark", "ghostty"),
]

# Per-theme overrides the owner ruled on RT-583.
NORD_ACCENT = 0xB48EAD
NORD_WORKING = 0x70A8FF
NORD_BRIGHT_BLACK = 0x7A808C


def load_ghostty(path):
    """A Ghostty theme file's background, foreground, palette 0-15, cursor colour and selection background."""
    keys, pal = {}, [None] * 16
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = (p.strip() for p in line.split("=", 1))
        if k == "palette":
            i, c = (p.strip() for p in v.split("=", 1))
            if int(i) < 16:
                pal[int(i)] = int(c.lstrip("#"), 16)
        else:
            keys[k] = v
    missing = [k for k in ("background", "foreground", "cursor-color", "selection-background") if k not in keys]
    if missing or None in pal:
        sys.exit(f"{path}: missing {missing or 'palette entries'}")
    colour = lambda k: int(keys[k].lstrip("#"), 16)
    return colour("background"), colour("foreground"), pal, colour("cursor-color"), colour("selection-background")


def terminal_of(name, kind, herdr_dir):
    """The scheme's ground, foreground, ANSI 0-15 and terminal palette (caret, selection, its alpha)."""
    if kind == "hand":
        bg, fg, pal = HAND_BUILT[name]
        pal = list(pal)
        # Contrast's recipe: the scheme's blue at 0.3 over the cell, the caret the foreground.
        term = {"caret": fg, "selection": pal[4], "selection_alpha": 0.3}
    else:
        bg, fg, pal, cursor, selection = load_ghostty(herdr_dir / name)
        # Ghostty's selection background is a solid colour; its selection-foreground is not used.
        term = {"caret": cursor, "selection": selection, "selection_alpha": 1.0}
    if name == "Nord":
        pal[8] = NORD_BRIGHT_BLACK
    term.update({"fg": fg, "bg": bg, "selection_fg": fg, "ansi": pal})
    return bg, fg, pal, term


# ---------- the solvers ----------

def first_share(base, over, start, stop, ok):
    """The first share in `start`..`stop` (inclusive, either direction) whose mix passes `ok`, or None."""
    step = 1 if stop >= start else -1
    for p in range(start, stop + step, step):
        if ok(mix(base, over, p)):
            return p
    return None


def label_on(label, fills, floor=TEXT_FLOOR):
    """`label` blended in 10% steps toward the pole away from the first fill until it holds `floor` on every fill."""
    pole = 0x000000 if lum(label) < lum(fills[0]) else 0xFFFFFF
    for pct in range(0, 101, 10):
        cand = mix(label, pole, pct)
        if worst(cand, fills) >= floor:
            return cand
    return None


def fill_and_label(seed, chrome, bg, fg, label_fills):
    """`seed` inked to 3:1 on the chrome, and the label (bg or fg, nudged) holding 4.5:1 on `label_fills(fill)`.

    When no label reaches 4.5:1, the fill is inked further from the chrome in 0.25 steps until one does.
    """
    target = GLYPH_FLOOR
    while target <= 21:
        fill = ink(seed, chrome, target)
        label = label_on(text_on(bg, fg, fill), label_fills(fill))
        if label is not None:
            return fill, label
        target += 0.25
    return ink(seed, chrome), None


# How many points of the mix toward text separate text_2 from subtle, so the two steps stay apart.
QUIET_GAP = 6
QUIET_SURFACES = ("ground", "surface", "raised", "row_hover", "popover", "modal")


def solve_quiet_text(t, bg, fg, accent):
    """The shares toward text of text_2 and subtle, and the selected-row wash's share of the accent.

    text_2 starts at 88% and rises until it holds 4.5:1 on every quiet surface; the wash is the strongest share (at
    most 22% of the accent into the surface) on which text and text_2 hold 4.5:1; subtle is the quietest share that
    holds 4.5:1 on every quiet surface and the wash. When no subtle stays QUIET_GAP points below text_2, the wash
    lightens further, and when even no wash leaves room, text_2 rises again.
    """
    surfaces = [t[k] for k in QUIET_SURFACES]
    for t2 in range(88, 101):
        text_2 = mix(bg, fg, t2)
        if worst(text_2, surfaces) < TEXT_FLOOR:
            continue
        strongest = first_share(t["surface"], accent, 22, 0,
                                lambda c: ratio(text_2, c) >= TEXT_FLOOR and ratio(fg, c) >= TEXT_FLOOR)
        if strongest is None:
            continue
        for wash in range(strongest, -1, -1):
            selection = mix(t["surface"], accent, wash)
            sub = first_share(bg, fg, 55, t2 - QUIET_GAP,
                              lambda c: worst(c, surfaces + [selection]) >= TEXT_FLOOR)
            if sub is not None:
                return t2, wash, sub
    return None, None, None


def derive(name, bg, fg, pal, problems):
    light = sum(x * w for x, w in zip(ch(bg), (0.2126, 0.7152, 0.0722))) > 0.5
    surface, active = mix(bg, fg, 5), mix(bg, fg, 12)
    chrome = [bg, surface, active]
    popover = mix(bg, 0xFFFFFF, 70) if light else active
    t = {
        "ground": bg,
        "sunken": away(bg, fg, 4),
        "surface": surface,
        "raised": mix(bg, fg, 8),
        "chip": active,
        "hover": active,
        "pressed": mix(bg, fg, 16),
        "popover": popover,
        "popover_hover": mix(popover, fg, 12),
        "popover_pressed": mix(popover, fg, 8),
        "row_card": (mix(bg, fg, 8) << 8) | 0xFF,
        "row_hover": mix(bg, fg, 10),
        "popover_edge": mix(bg, fg, 24),
        "modal": mix(bg, fg, 10),
        "modal_edge": mix(bg, fg, 24),
        "modal_inset": mix(bg, fg, 14),
        "menu_rule": mix(bg, fg, 22),
        "card_edge": mix(bg, fg, 14),
        "line": mix(bg, fg, 14),
        "line_strong": mix(bg, fg, 24),
        "text": fg,
        "primary": mix(bg, fg, 92),
    }
    if light:
        t["primary_hover"], t["primary_pressed"] = mix(t["primary"], 0x000000, 25), mix(t["primary"], bg, 12)
    else:
        t["primary_hover"], t["primary_pressed"] = mix(t["primary"], 0xFFFFFF, 55), mix(t["primary"], 0x000000, 12)

    # The accent, and the label on it and on the primary buttons.
    accent, on_accent = fill_and_label(
        pal[5], chrome, bg, fg, lambda a: [a, t["primary"], t["primary_hover"], t["primary_pressed"]])
    if name == "Nord" and accent != NORD_ACCENT:
        problems.append(f"{name}: the accent derives to {accent:06x}, not the ruled {NORD_ACCENT:06x}")
    if on_accent is None:
        problems.append(f"{name}: no on_accent reaches {TEXT_FLOOR}:1")
        on_accent = text_on(bg, fg, accent)
    t["accent"], t["on_accent"] = accent, on_accent

    t2, wash, sub = solve_quiet_text(t, bg, fg, accent)
    if t2 is None:
        problems.append(f"{name}: no text_2 share holds {TEXT_FLOOR}:1 with subtle {QUIET_GAP} points below it")
        t2, wash, sub = 100, 0, 100
    t["text_2"] = mix(bg, fg, t2)
    t["selection"] = mix(surface, accent, wash)
    t["subtle"] = mix(bg, fg, sub)
    faint = first_share(bg, fg, 35, 100, lambda c: worst(c, [bg, surface]) >= GLYPH_FLOOR)
    t["faint"] = mix(bg, fg, faint if faint is not None else 100)
    t["launched_tint"] = mix(surface, accent, min(100, 2 * wash))

    working = NORD_WORKING if name == "Nord" else pal[4]
    t["working"] = ink(working, chrome)
    t["waiting"] = ink(pal[2], chrome)
    t["danger"] = ink(pal[1], chrome)
    t["status_ok"] = ink(pal[2], chrome)
    asking, on_asking = fill_and_label(pal[3], chrome, bg, fg, lambda a: [a])
    if on_asking is None:
        problems.append(f"{name}: no on_asking reaches {TEXT_FLOOR}:1")
        on_asking = text_on(bg, fg, asking)
    t["asking"], t["on_asking"] = asking, on_asking
    t["danger_text"] = toward_floor(t["danger"], [t[k] for k in (
        "ground", "popover", "popover_hover", "popover_pressed", "modal", "modal_inset")], light)

    syntax_on = [bg, t["hover"], t["pressed"]]

    def syntax(c):
        # ink can clip a saturated colour all the way to the pole; a plain blend toward it keeps some hue.
        inked = ink(c, syntax_on, TEXT_FLOOR)
        return toward_floor(c, syntax_on, light) if inked in (0x000000, 0xFFFFFF) else inked

    t["lilac"] = syntax(pal[13])
    t["rose"] = syntax(pal[9])
    t["periwinkle"] = syntax(pal[12])
    t["ochre"] = syntax(pal[11])
    t["change_deleted"] = syntax(pal[9])
    t["diff_delete"] = t["danger"]
    t["diff_filler"] = 0x000000 if light else 0xFFFFFF
    t["diff_delete_wash_alpha"] = 0.13
    t["diff_insert_wash_alpha"] = 0.12
    t["diff_filler_alpha"] = 0.04 if light else 0.025
    t["diff_word_wash_alpha"] = 0.30
    t["danger_bg"] = mix(surface, t["danger"], 18)
    t["status_idle"] = ink(mix(bg, fg, 45), [t["sunken"], bg])
    t["status_err"] = t["danger"]
    t["spawn_badge_ok"] = t["waiting"]
    shadows = (0x40, 0x59, 0x33, 0x59, 0x40) if light else (0xB3, 0xCC, 0x73, 0xA6, 0x59)
    for key, alpha in zip(("popover_shadow", "dialog_shadow", "toast_shadow", "scrim", "shell_mark_border"), shadows):
        t[key] = alpha

    t.update({
        "panel_bg": t["surface"],
        "bar_bg": t["sunken"],
        "hover_bg": t["hover"],
        "border": t["line"],
        "muted": t["text_2"],
        "warning": t["asking"],
        "selected_bg": t["selection"],
        "input_text": t["text"],
        "input_placeholder": t["subtle"],
        "input_cursor": t["accent"],
        "input_selection": (t["accent"] << 8) | 0x4D,
        "ask_wash": (t["asking"] << 8) | 0x12,
        "ask_border": (t["asking"] << 8) | 0x59,
        "danger_wash": (t["danger"] << 8) | 0x1A,
        "context_hot": t["danger"],
        "ok_wash": (t["status_ok"] << 8) | 0x29,
        "transparent": 0x00000000,
    })
    shares = {"text_2": t2, "subtle": sub, "selection": wash, "faint": faint}
    return t, shares


# ---------- checks the app's contrast table also makes ----------

def check(name, t, term, problems):
    pairs = [(fg, bg, TEXT_FLOOR) for fg in ("text", "text_2", "subtle")
             for bg in ("ground", "surface", "raised", "row_hover", "popover", "modal", "selection")]
    pairs += [("on_accent", "accent", TEXT_FLOOR), ("on_accent", "primary", TEXT_FLOOR),
              ("on_asking", "asking", TEXT_FLOOR), ("danger_text", "ground", TEXT_FLOOR),
              ("faint", "ground", GLYPH_FLOOR), ("faint", "surface", GLYPH_FLOOR)]
    for fg, bg, floor in pairs:
        r = ratio(t[fg], t[bg])
        if r < floor:
            problems.append(f"{name}: {fg} on {bg} is {r:.2f}:1, below {floor}")
    r = ratio(term["fg"], term["bg"])
    if r < TEXT_FLOOR:
        problems.append(f"{name}: terminal_fg on terminal_bg is {r:.2f}:1")


# ---------- Rust ----------

# UiTheme's fields in declaration order, with how each is written.
FIELDS = [
    ("ground", "rgb"), ("sunken", "rgb"), ("surface", "rgb"), ("raised", "rgb"), ("chip", "rgb"), ("hover", "rgb"),
    ("pressed", "rgb"), ("popover_hover", "rgb"), ("popover_pressed", "rgb"), ("launched_tint", "rgb"),
    ("row_card", "rgba"), ("row_hover", "rgb"), ("popover", "rgb"), ("popover_edge", "rgb"),
    ("popover_shadow", "rgba"), ("modal", "rgb"), ("modal_edge", "rgb"), ("modal_inset", "rgb"),
    ("menu_rule", "rgb"), ("card_edge", "rgb"), ("line", "rgb"), ("line_strong", "rgb"), ("text", "rgb"),
    ("text_2", "rgb"), ("subtle", "rgb"), ("faint", "rgb"), ("accent", "rgb"), ("on_accent", "rgb"),
    ("primary", "rgb"), ("primary_hover", "rgb"), ("primary_pressed", "rgb"), ("selection", "rgb"),
    ("scrim", "rgba"), ("dialog_shadow", "rgba"), ("toast_shadow", "rgba"), ("working", "rgb"), ("asking", "rgb"),
    ("on_asking", "rgb"), ("waiting", "rgb"), ("danger", "rgb"), ("danger_text", "rgb"), ("lilac", "rgb"),
    ("ochre", "rgb"), ("rose", "rgb"), ("periwinkle", "rgb"), ("change_deleted", "rgb"), ("diff_delete", "rgb"),
    ("diff_filler", "rgb"), ("diff_delete_wash_alpha", "f32"), ("diff_insert_wash_alpha", "f32"),
    ("diff_filler_alpha", "f32"), ("diff_word_wash_alpha", "f32"), ("panel_bg", "rgb"), ("bar_bg", "rgb"),
    ("hover_bg", "rgb"), ("border", "rgb"), ("muted", "rgb"), ("warning", "rgb"), ("selected_bg", "rgb"),
    ("input_text", "rgb"), ("input_placeholder", "rgb"), ("input_cursor", "rgb"), ("input_selection", "rgba"),
    ("ask_wash", "rgba"), ("ask_border", "rgba"), ("danger_wash", "rgba"), ("context_hot", "rgb"),
    ("danger_bg", "rgb"), ("status_ok", "rgb"), ("ok_wash", "rgba"), ("status_idle", "rgb"), ("status_err", "rgb"),
    ("spawn_badge_ok", "rgb"), ("shell_mark_border", "rgba"), ("transparent", "rgba"),
]


def literal(value, kind):
    if kind == "rgb":
        return f"0x{value:06x}"
    if kind == "rgba":
        return f"0x{value:08x}"
    return repr(float(value))


def const_name(theme_id):
    return theme_id.upper().replace("-", "_")


def rust_theme(theme_id, name, source, t, term, shares):
    share_text = ", ".join(f"`{k}` {v}%" for k, v in shares.items())
    lines = [
        f"/// The `{theme_id}` built-in, derived from {source}.",
        "///",
        f"/// Solved shares: {share_text}.",
        f"pub const {const_name(theme_id)}: UiTheme = UiTheme {{",
    ]
    lines += [f"    {key}: {literal(t[key], kind)}," for key, kind in FIELDS]
    lines += [
        "    terminal: TerminalPalette {",
        f"        fg: rgb(0x{term['fg']:06x}),",
        f"        bg: rgb(0x{term['bg']:06x}),",
        f"        caret: rgb(0x{term['caret']:06x}),",
        f"        selection: rgb(0x{term['selection']:06x}),",
        f"        selection_alpha: {term['selection_alpha']!r},",
        f"        selection_fg: rgb(0x{term['selection_fg']:06x}),",
        "        ansi: [",
    ]
    lines += [f"            rgb(0x{c:06x})," for c in term["ansi"]]
    lines += ["        ],", "    },", "};"]
    return "\n".join(lines)


HEADER = f"""//! The built-in themes, in the order they are listed. Contrast is
//! [`UiTheme::CONTRAST`]; every other one is derived from its terminal
//! scheme.
//!
//! Generated: do not edit. Regenerate with
//! `{COMMAND}`
//! (`docs/design/theme-defaults/gen_builtin_themes.py`). The schemes' sources and
//! licences are in `apps/native/assets/themes/NOTICE.md`.

#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

use crate::theme::{{TerminalPalette, rgb}};
use crate::ui_theme::UiTheme;

#[cfg(test)]
mod tests;
"""


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {COMMAND}")
    herdr_dir = Path(sys.argv[1])
    problems, blocks, entries = [], [], [("contrast", "Contrast", "UiTheme::CONTRAST")]
    for theme_id, name, kind in THEMES:
        bg, fg, pal, term = terminal_of(name, kind, herdr_dir)
        t, shares = derive(name, bg, fg, pal, problems)
        check(name, t, term, problems)
        source = ("the scheme herdr-gpui transcribes" if kind == "hand"
                  else f"the `{name}` Ghostty file of `iTerm2-Color-Schemes`")
        blocks.append(rust_theme(theme_id, name, source, t, term, shares))
        entries.append((theme_id, name, const_name(theme_id)))
        print(f"{name:18} " + "  ".join(f"{k}={v}" for k, v in shares.items())
              + f"  accent={t['accent']:06x} on_accent={t['on_accent']:06x} selection={t['selection']:06x}"
              + "  syntax=" + ",".join(f"{t[k]:06x}" for k in ("lilac", "rose", "periwinkle", "ochre", "danger_text")))
    listing = "\n".join(f'    ("{i}", "{n}", {c}),' for i, n, c in entries)
    text = (HEADER + "\n/// The built-ins in list order: id, name, theme.\n"
            + f"pub const BUILT_INS: &[(&str, &str, UiTheme)] = &[\n{listing}\n];\n\n"
            + "\n\n".join(blocks) + "\n")
    with open(OUT, "w", newline="\n", encoding="utf-8") as fh:
        fh.write(text)
    print(f"wrote {OUT}")
    if problems:
        print("\n".join(problems))
        sys.exit(1)


main()
