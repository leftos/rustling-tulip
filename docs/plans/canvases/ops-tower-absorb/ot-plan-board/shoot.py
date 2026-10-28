"""Render each lane of the Plan board mock headlessly and save PNGs beside the page.

One PNG per lane of the app frame at a 1640x1120 viewport, lane C again grown to its
full height (its Plan section sits below the fold), and the states strip.
"""

import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

page_path = Path(sys.argv[1]).resolve()
out_dir = page_path.parent
lanes = sys.argv[2:] or ["a", "b", "c"]

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    try:
        page = browser.new_page(viewport={"width": 1640, "height": 1120}, device_scale_factor=1)
        page.goto(page_path.as_uri() + "#b")
        page.wait_for_timeout(400)
        for lane in lanes:
            page.click(f'.lane-tab[data-lane="{lane}"]')
            page.wait_for_timeout(200)
            out = out_dir / f"lane-{lane}.png"
            page.locator("#app").screenshot(path=str(out))
            print(out)
        # Lane C in full: the frame grown to the Dashboard's scroll height.
        page.click('.lane-tab[data-lane="c"]')
        page.evaluate(
            "() => { const v = document.getElementById('dashc');"
            " v.style.height = 'auto'; v.style.overflow = 'visible';"
            " document.getElementById('app').style.height = (v.scrollHeight + 64) + 'px'; }"
        )
        page.set_viewport_size({"width": 1640, "height": 2000})
        page.wait_for_timeout(150)
        out = out_dir / "lane-c-full.png"
        page.locator("#app").screenshot(path=str(out))
        print(out)
        out = out_dir / "states.png"
        page.locator("#states").screenshot(path=str(out))
        print(out)
    finally:
        browser.close()
