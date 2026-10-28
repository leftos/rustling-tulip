"""Render each lane, surface and state of the accounts mock headlessly and save PNGs beside the page."""

import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

page_path = Path(sys.argv[1]).resolve()
out_dir = page_path.parent

SHOTS = [
    ("a", "popover", "comfy"),
    ("a", "popover", "near"),
    ("a", "popover", "switched"),
    ("a", "popover", "dry"),
    ("a", "settings", "comfy"),
    ("a", "settings", "dry"),
    ("a", "add", "comfy"),
    ("b", "dialog", "comfy"),
    ("b", "dialog", "near"),
    ("b", "dialog", "switched"),
    ("b", "dialog", "dry"),
    ("b", "add", "comfy"),
]

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    try:
        page = browser.new_page(viewport={"width": 1640, "height": 1120}, device_scale_factor=1)
        page.goto(page_path.as_uri() + "#a")
        page.wait_for_timeout(400)
        for lane, surface, state in SHOTS:
            page.evaluate("([l, s, x]) => window.go(l, s, x)", [lane, surface, state])
            page.wait_for_timeout(150)
            out = out_dir / f"{lane}-{surface}-{state}.png"
            page.locator("#app").screenshot(path=str(out))
            print(out)
        # Settings → Accounts whole: the modal grown to its page's scroll height, so Forecast and Store show too.
        page.evaluate("() => window.go('a', 'settings', 'comfy')")
        page.evaluate(
            "() => { const m = document.querySelector('.modal'); const p = document.querySelector('.spage');"
            " const h = p.scrollHeight + 54 + 58 + 4; m.style.height = h + 'px';"
            " document.getElementById('app').style.height = (h + 70) + 'px'; }"
        )
        page.set_viewport_size({"width": 1640, "height": 1600})
        page.wait_for_timeout(150)
        out = out_dir / "a-settings-comfy-full.png"
        page.locator("#app").screenshot(path=str(out))
        print(out)
    finally:
        browser.close()
