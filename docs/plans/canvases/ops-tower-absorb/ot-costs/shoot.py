"""Render each lane and state of the Costs mock headlessly and save PNGs of the app frame beside the page."""

import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

page_path = Path(sys.argv[1]).resolve()
out_dir = page_path.parent

# (lane, state, tab, file name, full height)
SHOTS = [
    ("a", "normal", "costs", "lane-a.png", False),
    ("a", "normal", "costs", "lane-a-full.png", True),
    ("a", "normal", "dash", "lane-a-dashboard.png", False),
    ("b", "normal", "costs", "lane-b.png", False),
    ("b", "normal", "costs", "lane-b-full.png", True),
    ("b", "normal", "dash", "lane-b-dashboard.png", False),
    ("c", "normal", "dash", "lane-c.png", False),
    ("a", "empty", "costs", "state-a-empty.png", False),
    ("a", "partial", "costs", "state-a-partial.png", False),
    ("a", "error", "costs", "state-a-error.png", False),
    ("b", "partial", "costs", "state-b-partial.png", False),
    ("c", "empty", "dash", "state-c-empty.png", False),
]

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    try:
        page = browser.new_page(viewport={"width": 1640, "height": 1200}, device_scale_factor=1)
        page.goto(page_path.as_uri())
        page.wait_for_timeout(400)
        for lane, state, tab, name, full in SHOTS:
            page.evaluate(
                "([l, s, t]) => { lane = l; st = s; tab = t; render(); document.getElementById('app').style.height = ''; }",
                [lane, state, tab],
            )
            if full:
                page.evaluate(
                    "() => { const v = document.getElementById('view');"
                    " document.getElementById('app').style.height = (v.scrollHeight + 62) + 'px'; }"
                )
                page.set_viewport_size({"width": 1640, "height": 1800})
            else:
                page.set_viewport_size({"width": 1640, "height": 1200})
            page.wait_for_timeout(150)
            out = out_dir / name
            page.locator("#app").screenshot(path=str(out))
            print(out)
    finally:
        browser.close()
