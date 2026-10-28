"""Render each lane and state of the ops-tower additions mock headlessly and save PNGs beside the page."""

import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

page_path = Path(sys.argv[1]).resolve()
out_dir = page_path.parent
LANES = ("a", "b")
STATES = ("normal", "quiet", "trouble")

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    try:
        page = browser.new_page(viewport={"width": 1640, "height": 1120}, device_scale_factor=1)
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(page_path.as_uri() + "#a-normal")
        page.wait_for_timeout(400)
        for lane in LANES:
            for state in STATES:
                page.click(f'.lane-tab[data-lane="{lane}"]')
                page.click(f'.st-tab[data-state="{state}"]')
                page.wait_for_timeout(200)
                out = out_dir / f"lane-{lane}-{state}.png"
                page.locator("#app").screenshot(path=str(out))
                print(out)
        # Both lanes in full, normal state: the frame grown to the view's scroll height so rows below the fold show.
        for lane in LANES:
            page.click(f'.lane-tab[data-lane="{lane}"]')
            page.click('.st-tab[data-state="normal"]')
            page.evaluate(
                "(l) => { const v = document.getElementById('view-' + l);"
                " document.getElementById('app').style.height = (v.scrollHeight + 62) + 'px'; }",
                lane,
            )
            page.set_viewport_size({"width": 1640, "height": 2400})
            page.wait_for_timeout(150)
            out = out_dir / f"lane-{lane}-full.png"
            page.locator("#app").screenshot(path=str(out))
            print(out)
            page.evaluate("() => { document.getElementById('app').style.height = ''; }")
        if errors:
            print("page errors:", *errors, sep="\n  ")
            sys.exit(1)
    finally:
        browser.close()
