"""RT-1152: classify apps/native/src by what a phone build could keep. Counts non-blank, non-comment lines before the
first `#[cfg(test)]` module line. Run from the worktree root."""
import pathlib
import re

ROOT = pathlib.Path("apps/native/src")
DESKTOP = re.compile(
    r"^(toast_window|toast_activation|taskbar_badge|update|update_view|whats_new_view|driver/|driver_gate|driver_id|"
    r"offscreen|folder_picker|presence|sound|window_state|popout|popouts|popout_view|main|open|open_view|shell_dialog|"
    r"shell_view|vscode_suggestion|vscode_suggestion_view|remote_access|connection_picker|connection_picker_view|ssh)"
)
VIEW = re.compile(r"\bdiv\(\)|impl Render for|\.child\(|canvas\(|impl Element for")
GPUI = re.compile(r"\bgpui::|use gpui\b")
buckets = {}
files = {}
for path in sorted(ROOT.rglob("*.rs")):
    rel = path.relative_to(ROOT).as_posix()
    text = path.read_text(encoding="utf8")
    match = re.search(r"^#\[cfg\(test\)\]\s*\n\s*(#\[[^\n]*\n\s*)*mod ", text, re.M)
    if match:
        text = text[: match.start()]
    code = [
        line
        for line in text.split("\n")
        if line.strip() and not line.strip().startswith("//")
    ]
    body = "\n".join(code)
    stem = rel[:-3]
    if DESKTOP.match(stem):
        kind = "4 desktop-shell (OS integration, updater, driver, process spawning)"
    elif VIEW.search(body):
        kind = "3 view (renders elements)"
    elif GPUI.search(body):
        kind = "2 logic that names gpui types, renders nothing"
    else:
        kind = "1 plain Rust, no gpui"
    buckets[kind] = buckets.get(kind, 0) + len(code)
    files.setdefault(kind, []).append((len(code), stem))
total = sum(buckets.values())
print("total code lines", total)
for kind in sorted(buckets):
    print(f"{kind}: {buckets[kind]} lines, {buckets[kind] * 100 / total:.1f}%, {len(files[kind])} files")
    print("   largest:", ", ".join(f"{s} {n}" for n, s in sorted(files[kind], reverse=True)[:9]))
named = ["chat", "chat_tail", "chat_cards", "term", "term_input", "term_view", "chat_view", "sidebar", "sidebar_view",
         "connection", "net", "tabs", "md_blocks", "appearance", "fonts", "text_input", "mouse", "keys"]
for name in named:
    for kind, entries in files.items():
        for n, stem in entries:
            if stem == name:
                print(f"{name}: {n} ({kind[0]})")
