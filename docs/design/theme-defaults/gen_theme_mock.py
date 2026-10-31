"""Generate the RT-583 theme-defaults mock page and print each lane's tokens and flags."""

import json
import math
import sys
from pathlib import Path

HERDR = Path("X:/dev/_ext/penso/herdr-gpui/assets/themes")
OUT = Path(sys.argv[1])
SUMMARY = Path(sys.argv[2])


# ---------- colour maths (ported from herdr-gpui contrast.rs / theme.rs) ----------

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


def ink(c, bgs, target=3.0):
    """herdr-gpui contrast.rs:154 `ink`: OKLab lightness bisection keeping hue, chroma gain 1.5."""
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
    """herdr-gpui theme.rs:366 `mix`: integer per-channel blend of `pct`% of `over` onto `base`."""
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


def toward_floor(c, bgs, light, floor=4.5):
    """`c` blended toward white (black on a light theme) in 5% steps until it reaches `floor` on every one of `bgs`."""
    pole = 0x000000 if light else 0xFFFFFF
    for pct in range(0, 101, 5):
        cand = mix(c, pole, pct)
        if worst(cand, bgs) >= floor:
            return cand
    return pole


def text_on(theme_bg, theme_fg, fill):
    """herdr-gpui theme.rs:492 `text_on` (gamma-encoded luminance, as herdr computes it)."""
    def l(c):
        r, g, b = ch(c)
        return 0.2126 * r + 0.7152 * g + 0.0722 * b
    f = l(fill)
    return theme_bg if abs(l(theme_bg) - f) >= abs(l(theme_fg) - f) else theme_fg


def dist(a, b):
    la, lb = to_oklab(a), to_oklab(b)
    return math.dist(la, lb)


def hue_gap(a, b):
    """Angle between two colours' OKLab hues, in degrees."""
    ha = math.degrees(math.atan2(to_oklab(a)[2], to_oklab(a)[1]))
    hb = math.degrees(math.atan2(to_oklab(b)[2], to_oklab(b)[1]))
    d = abs(ha - hb) % 360
    return min(d, 360 - d)


def hx(c):
    return f"#{c & 0xFFFFFF:06x}"


# ---------- theme sources ----------

XTERM16 = [0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xC0C0C0,
           0x808080, 0xFF0000, 0x00FF00, 0xFFFF00, 0x0000FF, 0xFF00FF, 0x00FFFF, 0xFFFFFF]

BUILTIN = {
    "Nord": (0x2E3440, 0xD8DEE9, [0x3B4252, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x88C0D0, 0xE5E9F0,
                                  0x4C566A, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x8FBCBB, 0xECEFF4]),
    "Dracula": (0x282A36, 0xF8F8F2, [0x21222C, 0xFF5555, 0x50FA7B, 0xF1FA8C, 0xBD93F9, 0xFF79C6, 0x8BE9FD, 0xF8F8F2,
                                     0x6272A4, 0xFF6E6E, 0x69FF94, 0xFFFFA5, 0xD6ACFF, 0xFF92DF, 0xA4FFFF, 0xFFFFFF]),
    "Catppuccin Mocha": (0x1E1E2E, 0xCDD6F4, [0x45475A, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xBAC2DE,
                                              0x585B70, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xA6ADC8]),
    "Catppuccin Latte": (0xEFF1F5, 0x4C4F69, [0x5C5F77, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB, 0x179299, 0xACB0BE,
                                              0x6C6F85, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB, 0x179299, 0xBCC0CC]),
}


def load_ghostty(name):
    bg, fg, pal = None, None, [0] * 16
    for line in (HERDR / name).read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = (p.strip() for p in line.split("=", 1))
        if k == "background":
            bg = int(v.lstrip("#"), 16)
        elif k == "foreground":
            fg = int(v.lstrip("#"), 16)
        elif k == "palette":
            i, c = (p.strip() for p in v.split("=", 1))
            if int(i) < 16:
                pal[int(i)] = int(c.lstrip("#"), 16)
    return bg, fg, pal


# ---------- token derivation ----------

def derive(name, source, bg, fg, pal, herdr_chrome=None):
    # browser/kit.rs:26-34: gamma-encoded luminance above 0.5 is a light theme.
    light = sum(x * w for x, w in zip(ch(bg), (0.2126, 0.7152, 0.0722))) > 0.5
    if herdr_chrome:  # herdr's Default is hand-authored and skips derive_chrome (theme.rs:348-362)
        surface, active, muted = herdr_chrome
    else:  # theme.rs:542-547 derive_chrome
        surface, active, muted = mix(bg, fg, 5), mix(bg, fg, 12), mix(bg, fg, 55)
    chrome = [bg, surface, active]
    raised_herdr = mix(bg, 0xFFFFFF, 70) if light else active  # browser/kit.rs:34-39
    accent = ink(pal[5], chrome)  # theme.rs:452-456 primary = palette[5]; :509-515 ink at Standard 3:1
    t = {
        "ground": bg,
        "terminal_ground": bg,
        "sunken": away(bg, fg, 4),
        "surface": surface,
        "raised": mix(bg, fg, 8),
        "row_card": mix(bg, fg, 8),
        "chip": active,
        "hover": active,
        "line": mix(bg, fg, 14),
        "line_strong": mix(bg, fg, 24),
        "popover": raised_herdr,
        "popover_edge": mix(bg, fg, 24),
        "popover_hover": mix(raised_herdr, fg, 12),
        "menu_rule": mix(bg, fg, 22),
        "text": fg,
        "text_2": ink(mix(bg, fg, 78), chrome),
        "subtle": muted,
        "faint": mix(bg, fg, 35),
        "accent": accent,
        "on_accent": text_on(bg, fg, accent),
        "selection": mix(surface, accent, 22),
        "primary": mix(bg, fg, 92),
        "on_primary": bg,
        "working": ink(pal[4], chrome),
        "asking": ink(pal[3], chrome),
        "waiting": ink(pal[2], chrome),
        "danger": ink(pal[1], chrome),
        "status_ok": ink(pal[2], chrome),
        "term_fg": fg,
        "ansi": pal,
    }
    t["on_asking"] = text_on(bg, fg, t["asking"])
    t["danger_text"] = toward_floor(t["danger"], [t["popover"], t["popover_hover"]], light)
    return {"name": name, "source": source, "light": light, "t": t}


def contrast_today():
    t = {
        "ground": 0x08070A, "terminal_ground": 0x070609, "sunken": 0x050407, "surface": 0x0D0C10,
        "raised": 0x16151A, "row_card": 0x16151B, "chip": 0x1E1C23, "hover": 0x2C2A34,
        "line": 0x24222A, "line_strong": 0x454250, "popover": 0x3D3A47, "popover_edge": 0x504D5B,
        "popover_hover": 0x4A4756, "menu_rule": 0x5E5A6B, "text": 0xEDE9E4, "text_2": 0xBDB8C4,
        "subtle": 0xAEAAB8, "faint": 0x6E6A77, "accent": 0xF07A62, "on_accent": 0x1C100D,
        "selection": 0x241718, "primary": 0xE4E0DA, "on_primary": 0x1C100D, "working": 0x6CA6FF,
        "asking": 0xF6BC4E, "on_asking": 0x2A1B00, "waiting": 0x4FC89D, "danger": 0xF26D6D,
        "danger_text": 0xFF9C9C, "status_ok": 0x3FB96A, "term_fg": 0xE5E6E8,
        "ansi": [0x5A5C5F, 0xEF5C5C, 0x3FB96A, 0xE8A531, 0x5B9BFF, 0xB787F0, 0x5DD5E3, 0xE5E6E8,
                 0x656872, 0xFF8585, 0x62D18A, 0xF5C267, 0x7EB4FF, 0xD4ABFF, 0x8FEAF3, 0xF5F6F8],
    }
    return {"name": "Contrast (today)", "source": "apps/native/src/palette.rs + ui_theme.rs:329-354", "light": False, "t": t}


def lanes():
    out = [contrast_today()]
    d = 0x101419
    out.append(derive("Default", "herdr-gpui built-in (theme.rs:331-362), xterm palette", d, 0xD8DEE9, XTERM16,
                      herdr_chrome=(0x1C1C22, 0x2B2933, 0x827E91)))
    for n in ["Catppuccin Mocha", "Nord", "Dracula"]:
        bg, fg, pal = BUILTIN[n]
        out.append(derive(n, "herdr-gpui built-in (theme.rs:553-584)", bg, fg, pal))
    for n in ["TokyoNight", "Rose Pine", "Kanagawa Wave", "Everforest Dark Med", "One Half Dark"]:
        bg, fg, pal = load_ghostty(n)
        out.append(derive(n, f"herdr-gpui assets/themes/{n}", bg, fg, pal))
    bg, fg, pal = BUILTIN["Catppuccin Latte"]
    out.append(derive("Catppuccin Latte", "herdr-gpui built-in (theme.rs:577-584)", bg, fg, pal))
    return out


# ---------- checks ----------

ANSI_NAMES = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
              "br.black", "br.red", "br.green", "br.yellow", "br.blue", "br.magenta", "br.cyan", "br.white"]


def flags(lane):
    t = lane["t"]
    f = []
    r = ratio(t["text"], t["ground"])
    if r < 7:
        f.append(f"TEXT on GROUND {r:.1f}:1 (floor 7)")
    for tok, floor in (("text_2", 4.5), ("subtle", 4.5)):
        unders = ["ground", "surface", "row_card", "selection"] + (["popover"] if tok == "text_2" else [])
        bad = [(u, ratio(t[tok], t[u])) for u in unders if ratio(t[tok], t[u]) < floor]
        if bad:
            w = min(bad, key=lambda x: x[1])
            f.append(f"{tok.upper()} {w[1]:.1f}:1 on {w[0].upper()} (floor {floor})")
    r = ratio(t["accent"], t["selection"])
    if r < 4.5:
        f.append(f"ACCENT label on SELECTION {r:.1f}:1")
    r = ratio(t["on_accent"], t["accent"])
    if r < 4.5:
        f.append(f"ON_ACCENT on ACCENT {r:.1f}:1")
    for i in (1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14):
        r = ratio(t["ansi"][i], t["terminal_ground"])
        if r < 3:
            f.append(f"ANSI {ANSI_NAMES[i]} {hx(t['ansi'][i])} only {r:.1f}:1 on the terminal ground")
    r = ratio(t["ansi"][8], t["terminal_ground"])
    if r < 2.5:
        f.append(f"ANSI br.black (dim text) {r:.1f}:1")
    for s in ("danger", "asking", "working", "waiting"):
        dd = dist(t["accent"], t[s])
        if dd < 0.12:
            f.append(f"ACCENT close to {s.upper()} (OKLab ΔE {dd:.2f})")
    for a, b in (("working", "waiting"), ("asking", "danger"), ("waiting", "asking")):
        dd = dist(t[a], t[b])
        dh = hue_gap(t[a], t[b])
        if dd < 0.12:
            f.append(f"{a.upper()} close to {b.upper()} (OKLab ΔE {dd:.2f})")
        elif dh < 35:
            f.append(f"{a.upper()} and {b.upper()} share a hue ({dh:.0f}° apart)")
    return f


# ---------- HTML ----------

ICON = {
    "menu": '<path d="M3 4.5h10M3 8h10M3 11.5h10"/>',
    "inbox": '<path d="M2.5 9.5 4 3.5h8l1.5 6v3h-11z"/><path d="M2.5 9.5h3l1 1.5h3l1-1.5h3"/>',
    "branch": '<circle cx="4.5" cy="3.5" r="1.5"/><circle cx="4.5" cy="12.5" r="1.5"/><circle cx="11.5" cy="6" r="1.5"/>'
              '<path d="M4.5 5v6M11.5 7.5c0 2.5-3 2.5-6 3.5"/>',
    "pieces": '<rect x="2.5" y="2.5" width="4.5" height="4.5" rx=".8"/><rect x="9" y="2.5" width="4.5" height="4.5" rx=".8"/>'
              '<rect x="2.5" y="9" width="4.5" height="4.5" rx=".8"/><rect x="9" y="9" width="4.5" height="4.5" rx=".8"/>',
    "history": '<path d="M2.8 8a5.2 5.2 0 1 0 1.5-3.7"/><path d="M2.5 2.5v2.5H5"/><path d="M8 5v3l2 1.5"/>',
    "sliders": '<path d="M2.5 5h11M2.5 11h11"/><circle cx="10" cy="5" r="1.6" class="fillbg"/><circle cx="6" cy="11" r="1.6" class="fillbg"/>',
    "chev": '<path d="M4.5 6 8 9.5 11.5 6"/>',
    "repo": '<circle cx="5" cy="4" r="1.4"/><circle cx="5" cy="12" r="1.4"/><circle cx="11" cy="6.5" r="1.4"/>'
            '<path d="M5 5.4v5.2M11 8c0 2-3 2-5.2 3"/>',
    "plus": '<path d="M8 3.5v9M3.5 8h9"/>',
    "term": '<path d="M3 5l3 3-3 3M8 11.5h5"/>',
    "dots": '<circle cx="4" cy="8" r=".9" class="fillfg"/><circle cx="8" cy="8" r=".9" class="fillfg"/><circle cx="12" cy="8" r=".9" class="fillfg"/>',
}


def icon(n, size=16):
    return (f'<svg class="ic" width="{size}" height="{size}" viewBox="0 0 16 16" fill="none" stroke="currentColor" '
            f'stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round">{ICON[n]}</svg>')


GLYPH = {
    "working": '<svg width="11" height="11" viewBox="0 0 11 11"><circle cx="5.5" cy="5.5" r="4.3" fill="none" stroke="var(--working)" stroke-width="1.4"/><circle cx="5.5" cy="5.5" r="1.9" fill="var(--working)"/></svg>',
    "asking": '<svg width="11" height="11" viewBox="0 0 11 11"><path d="M5.5 .6 10.4 5.5 5.5 10.4 .6 5.5z" fill="var(--asking)"/><text x="5.5" y="7.9" text-anchor="middle" font-size="7" font-weight="800" fill="var(--on-asking)" font-family="Inter">?</text></svg>',
    "idle": '<svg width="11" height="11" viewBox="0 0 11 11"><circle cx="5.5" cy="5.5" r="4.1" fill="none" stroke="var(--faint)" stroke-width="1.4"/></svg>',
    "exited": '<svg width="11" height="11" viewBox="0 0 11 11"><path d="M2.2 2.2l6.6 6.6M8.8 2.2l-6.6 6.6" stroke="var(--danger)" stroke-width="1.5" stroke-linecap="round"/></svg>',
    "waiting": '<svg width="11" height="11" viewBox="0 0 11 11"><circle cx="5.5" cy="5.5" r="4.1" fill="none" stroke="var(--waiting)" stroke-width="1.4"/></svg>',
    "bg": '<svg width="11" height="11" viewBox="0 0 11 11"><circle cx="5.5" cy="5.5" r="4.1" fill="none" stroke="var(--working)" stroke-width="1.4"/></svg>',
}


def card(glyph, title, agent, sub, chip, cls="", badge="", extra=""):
    return f'''<div class="card {cls}"><div class="r1"><span class="gl">{GLYPH[glyph]}</span><span class="ti">{title}</span>{badge}<span class="ag">{agent}</span></div>
<div class="r2"><span class="br">{sub}</span><span class="tchip">{chip}</span></div>{extra}</div>'''


SESSION_MENU = f'''<span class="cdots">{icon("dots", 14)}</span><div class="menu">
<div class="mi">Resume</div>
<div class="mi">Restart</div>
<div class="mi">Rename…<kbd>F2</kbd></div>
<div class="mi hov">Duplicate<kbd>▸</kbd></div>
<div class="mi">Pop out</div>
<div class="mr"></div>
<div class="mi dng">Remove from sidebar</div>
</div>'''


def term_bottom():
    a = lambda i, s, b=False: f'<span style="color:var(--a{i}){";font-weight:700" if b else ""}">{s}</span>'
    prompt = a(4, "PS") + " " + a(12, "D:\\src\\rustling-tulip") + a(5, "&gt;")
    lines = [
        prompt + " git log --oneline -2",
        a(3, "cd2135f") + " " + a(3, "(") + a(6, "HEAD -&gt; ", True) + a(2, "main", True) + a(3, ", ") + a(1, "origin/main", True)
        + a(3, ")") + " docs: put the doc comment back",
        a(3, "9082d06") + " ref: read every view's colours from the theme",
        prompt + " cargo test -p protocol",
        a(10, "   Compiling", True) + " protocol v0.5.2 " + a(8, "(D:\\src\\rustling-tulip\\crates\\protocol)"),
        a(11, "warning", True) + "<b>: unused variable: `scale`</b>  " + a(12, "--&gt;", True) + " src/lib.rs:412:9",
        a(9, "error[E0308]", True) + "<b>: mismatched types</b>  " + a(12, "--&gt;", True) + " src/wire.rs:88:17",
        "test tests::hello_decodes_with_range_field ... " + a(2, "ok"),
        "test result: " + a(2, "ok") + ". 2 passed; 0 failed; " + a(13, "0 ignored") + "; " + a(14, "119 filtered out"),
        prompt + ' <span class="cur">&nbsp;</span>',
    ]
    return "".join(f"<div>{l}</div>" for l in lines)


def term_top():
    c = lambda col, s, extra="": f'<span style="color:{col}{extra}">{s}</span>'
    return "".join(f"<div>{l}</div>" for l in [
        c("var(--a2)", "●") + " " + c("var(--tfg)", "Update", ";font-weight:700") + "(apps/native/src/footer.rs)",
        "&nbsp;&nbsp;" + c("var(--a8)", "└ Updated footer.rs with 6 additions and 3 removals"),
        c("var(--a2)", "●") + " " + c("var(--tfg)", "Bash", ";font-weight:700") + "(cargo nextest run -p rustling-tulip-native)",
        "&nbsp;&nbsp;" + c("var(--a8)", "└ Running…"),
        c("#d77757", "✻ Brewing…") + " " + c("var(--a8)", "(38s · ↑ 2.1k tokens · esc to interrupt)"),
        c("var(--a8)", "─" * 52),
        "&gt; ",
        c("var(--a8)", "─" * 52),
        "&nbsp;&nbsp;" + c("#af87ff", "⏵⏵ accept edits on") + " " + c("var(--a8)", "(shift+tab to cycle)"),
    ])


def window(lane):
    rail = "".join([
        f'<div class="ri sel">{icon("menu")}</div>',
        f'<div class="ri">{icon("inbox")}<b class="bdg">2</b></div>',
        f'<div class="ri">{icon("branch")}<b class="bdg">5</b></div>',
        f'<div class="ri">{icon("pieces")}<b class="bdg">2</b></div>',
        '<div class="sp"></div>',
        f'<div class="ri">{icon("history")}</div>',
        f'<div class="ri">{icon("sliders")}</div>',
    ])
    ask_badge = '<span class="abdg">!</span>'
    sidebar = f'''
<div class="sbh"><span class="sbt">Sessions</span><span class="seg"><i class="on">All</i><i>Active</i></span><span class="seg"><i class="on">Repos</i><i>Tabs</i></span></div>
<div class="sbb"><span class="pbtn">{icon("plus", 13)} Session <kbd>Ctrl N</kbd></span><span class="obtn">{icon("term", 13)} Shell</span><span class="dbtn">{icon("dots", 16)}</span></div>
<div class="grp">{icon("chev", 12)}{icon("repo", 13)}<span class="gn">rustling-tulip</span>{ask_badge}<span class="gk">REPO</span><span class="gc">2</span></div>
{card("working", "Petal footer polish", "claude", "feat/petal-footer", "T:tulip", "selc")}
{card("asking", "Recover dialog keys", "claude", "feat/recover-keys", "unbound", "", ask_badge)}
<div class="grp">{icon("chev", 12)}{icon("repo", 13)}<span class="gn">yaat</span><span class="gk">REPO</span><span class="gc">2</span></div>
{card("idle", "Flight strip layout", "claude", "feat/strip-layout", "T:yaat")}
{card("exited", "METAR parser fix", "pwsh", "exited · code 1", "T:yaat", "exit hov", "", SESSION_MENU)}
'''
    main = f'''
<div class="tabs"><div class="tab on">tulip<span class="x">×</span></div><div class="tab"><span class="abdg n">1</span>yaat<span class="x">×</span></div><div class="tplus">{icon("plus", 13)}</div></div>
<div class="panes">
 <div class="pane foc"><div class="ph"><span class="gl">{GLYPH["working"]}</span><b>Petal footer polish</b><span class="pchip ac">claude</span><span class="pchip mono">rustling-t…</span><span class="sp"></span><span class="pa">Pop out</span><span class="pa">Stop</span><span class="pa ic2">–&nbsp;&nbsp;↗&nbsp;&nbsp;↑&nbsp;&nbsp;↓&nbsp;&nbsp;×</span></div>
  <div class="tb">{term_top()}</div></div>
 <div class="pane"><div class="ph"><span class="gl">{GLYPH["idle"]}</span><b>rustling-tulip</b><span class="pchip mono">pwsh</span><span class="pchip mono">rustling-tulip:main</span><span class="sp"></span><span class="pa">Pop out</span><span class="pa">Stop</span><span class="pa ic2">–&nbsp;&nbsp;↗&nbsp;&nbsp;↑&nbsp;&nbsp;↓&nbsp;&nbsp;×</span></div>
  <div class="tb">{term_bottom()}</div></div>
</div>'''
    footer = f'''<span class="fbtn">Connections</span><span class="fpill"><i class="dot"></i>daemon · 8 sessions · :4242</span>
<span class="fi">{GLYPH["asking"]} 2 need you</span><span class="fi">{GLYPH["working"]} 1 working</span><span class="fi">{GLYPH["bg"]} 1 background</span><span class="fi">{GLYPH["waiting"]} 1 waiting</span>
<span class="sp"></span><span class="fi">Petal footer polish · s-petal</span>'''
    return (f'<div class="win" style="{style_vars(lane["t"])}"><div class="rail">{rail}</div><div class="side">{sidebar}</div>'
            f'<div class="main">{main}</div><div class="foot">{footer}</div></div>')


def style_vars(t):
    parts = []
    for k, v in t.items():
        if k == "ansi":
            parts += [f"--a{i}:{hx(c)}" for i, c in enumerate(v)]
        else:
            parts.append(f"--{k.replace('_', '-')}:{hx(v)}")
    parts.append(f"--tfg:{hx(t['term_fg'])}")
    parts.append(f"--sel-ink:{hx(t['accent'])}")
    return ";".join(parts)


def swatches(t):
    keys = ["ground", "sunken", "surface", "row_card", "popover", "line", "text", "text_2", "subtle", "accent", "selection",
            "working", "asking", "waiting", "danger"]
    cells = "".join(f'<span class="sw"><i style="background:{hx(t[k])}"></i>{k.upper()}<br><code>{hx(t[k])}</code></span>' for k in keys)
    ansi = "".join(f'<i title="{ANSI_NAMES[i]} {hx(c)}" style="background:{hx(c)}"></i>' for i, c in enumerate(t["ansi"]))
    return f'<div class="sws">{cells}</div><div class="ansi"><span>ANSI 0–15 on the terminal ground</span><span class="strip" style="background:{hx(t["terminal_ground"])}">{ansi}</span></div>'


CSS = (Path(__file__).parent / "theme_mock.css").read_text(encoding="utf-8")


def page(ls):
    rows = "".join(
        f'<tr><td>{i + 1}</td><td>{l["name"]}</td>' + "".join(
            f'<td><i class="chip" style="background:{hx(l["t"][k])}"></i><code>{hx(l["t"][k])}</code></td>'
            for k in ("ground", "surface", "text", "accent", "working", "asking", "waiting", "danger"))
        + f'<td>{len(l["flags"])}</td></tr>' for i, l in enumerate(ls))
    lanes_html = []
    for i, l in enumerate(ls):
        fl = "".join(f"<li>{x}</li>" for x in l["flags"]) or "<li class=ok>No pair below its floor.</li>"
        lanes_html.append(f'''<section class="lane" id="lane-{i + 1}">
<header><h2><span class="num">{i + 1}</span>{l["name"]}</h2><span class="gh">ground <code>{hx(l["t"]["ground"])}</code></span><span class="src">{l["source"]}</span></header>
{swatches(l["t"])}
{window(l)}
<ul class="flags">{fl}</ul>
</section>''')
    notes = (Path(__file__).parent / "theme_mock_notes.html").read_text(encoding="utf-8")
    return f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Theme defaults</title>
<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700;800&family=Geist+Mono:wght@400;500;700&display=swap" rel="stylesheet">
<style>
{CSS}
</style></head><body>
<div class="intro">
<h1>RT-583 · built-in themes and the new default</h1>
<p class="lede">Our main window drawn in eleven themes: today's Contrast for reference, then herdr-gpui's built-ins and bundled Ghostty schemes, each turned into our UI tokens by herdr-gpui's own chrome derivation. Pick the set to ship and the default. Every hex below is computed, not hand-tuned.</p>
{notes}
<table class="sum"><thead><tr><th>#</th><th>Theme</th><th>Ground</th><th>Surface</th><th>Text</th><th>Accent</th><th>Working</th><th>Needs you</th><th>Waiting</th><th>Danger</th><th>Flags</th></tr></thead><tbody>{rows}</tbody></table>
</div>
{"".join(lanes_html)}
</body></html>
'''


def main():
    ls = lanes()
    for l in ls:
        l["flags"] = flags(l)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    with open(OUT, "w", newline="\n", encoding="utf-8") as fh:
        fh.write(page(ls))
    summary = [{"name": l["name"], **{k: hx(l["t"][k]) for k in
                ("ground", "sunken", "surface", "row_card", "popover", "text", "text_2", "subtle", "accent", "selection",
                 "working", "asking", "waiting", "danger", "danger_text", "on_accent")}, "flags": l["flags"]} for l in ls]
    with open(SUMMARY, "w", newline="\n", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=1, ensure_ascii=False)
    for s in summary:
        print(f'{s["name"]:22} g={s["ground"]} s={s["surface"]} t={s["text"]} a={s["accent"]} | ' + " / ".join(s["flags"]))


main()
