"""Render each lane and state of the Gates mock headlessly at 1640x1120 and save PNGs beside the page."""

import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

page_path = Path(sys.argv[1]).resolve()
out_dir = page_path.parent
lanes = ["a", "b"]
states = ["idle", "busy", "wait", "fail"]

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    try:
        page = browser.new_page(viewport={"width": 1640, "height": 1120}, device_scale_factor=1)
        page.goto(page_path.as_uri() + "#a-busy")
        page.wait_for_timeout(400)
        for lane in lanes:
            page.click(f'.lane-tab[data-lane="{lane}"]')
            for state in states:
                page.click(f'.st-tab[data-state="{state}"]')
                page.wait_for_timeout(150)
                out = out_dir / f"lane-{lane}-{state}.png"
                page.locator("#app").screenshot(path=str(out))
                print(out)
        # The Stop gate confirm, opened from the busy row's menu (the same in both lanes).
        page.click('.lane-tab[data-lane="a"]')
        page.click('.st-tab[data-state="busy"]')
        page.click("[data-stop]")
        page.wait_for_timeout(150)
        out = out_dir / "lane-a-busy-confirm.png"
        page.locator("#app").screenshot(path=str(out))
        print(out)
    finally:
        browser.close()
