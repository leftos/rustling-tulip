"""Render each lane of the dashboard mock headlessly at 1600x1000 and save PNGs beside the page."""

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
        page.goto(page_path.as_uri() + "#a")
        page.wait_for_timeout(400)
        for lane in lanes:
            page.click(f'.lane-tab[data-lane="{lane}"]')
            page.wait_for_timeout(200)
            out = out_dir / f"lane-{lane}.png"
            page.locator("#app").screenshot(path=str(out))
            print(out)
        # Lanes A and B in full: the frame grown to the view's scroll height, so the rows below the fold show too.
        for lane in ("a", "b"):
            page.click(f'.lane-tab[data-lane="{lane}"]')
            page.evaluate(
                "(l) => { const v = document.getElementById('view-' + l);"
                " document.getElementById('app').style.height = (v.scrollHeight + 62) + 'px'; }",
                lane,
            )
            page.set_viewport_size({"width": 1640, "height": 1800})
            page.wait_for_timeout(150)
            out = out_dir / f"lane-{lane}-full.png"
            page.locator("#app").screenshot(path=str(out))
            print(out)
            page.evaluate("() => { document.getElementById('app').style.height = ''; }")
        # Lane C with a needs-you session picked, to show the ask in the detail pane.
        page.click('.lane-tab[data-lane="c"]')
        page.click('#view-c [data-sel="s3"]')
        page.wait_for_timeout(150)
        out = out_dir / "lane-c-permission.png"
        page.locator("#app").screenshot(path=str(out))
        print(out)
    finally:
        browser.close()
