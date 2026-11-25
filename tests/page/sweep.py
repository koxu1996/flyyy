# Every width from 900 to 1400 px in 10 px steps, with classic scrollbars: the page must not scroll sideways
# on Nodes and on an app's VMs tab (mock.py's long values). argv: base URL, chromium path.
import sys
from playwright.sync_api import sync_playwright
base, chromium = sys.argv[1], sys.argv[2]
bad = []
with sync_playwright() as p:
    # Playwright hides scrollbars in headless mode by default; keep the classic ones.
    b = p.chromium.launch(executable_path=chromium, ignore_default_args=["--hide-scrollbars"], args=["--disable-dev-shm-usage"])
    pg = b.new_page(viewport={"width": 1400, "height": 800})
    pg.goto(base + "/__scenario/full")
    for name, url, ready in [("Nodes", "/#nodes", ".lrow.wrow:not(.head)"), ("VMs", "/#app/averyveryverylongappname", None)]:
        pg.goto(base + url)
        if ready is None:
            pg.wait_for_selector("[data-tab=machines]")
            pg.click("[data-tab=machines]")
            ready = ".lrow.mtrow:not(.head)"
        pg.wait_for_selector(ready)
        rows = pg.locator(ready).count()
        sb = pg.evaluate("innerWidth - document.documentElement.clientWidth")
        print(f"{name}: {rows} rows; vertical scrollbar {sb} px")
        assert rows >= 10 and sb > 0, f"{name}: want the mock's rows and a classic vertical scrollbar"
        for w in range(900, 1401, 10):
            pg.set_viewport_size({"width": w, "height": 800})
            pg.wait_for_timeout(30)
            sw, cw = pg.evaluate("[document.documentElement.scrollWidth, document.documentElement.clientWidth]")
            if sw != cw:
                # The widest offender, for the report.
                who = pg.evaluate("""() => { const cw = document.documentElement.clientWidth; let best = null;
                  for (const e of document.querySelectorAll('main *')) { const r = e.getBoundingClientRect().right;
                    if (r > cw + 0.5 && (!best || r > best[1])) best = [e.tagName.toLowerCase() + '.' + [...e.classList].join('.'), Math.round(r - cw)]; }
                  return best; }""")
                bad.append((name, w, sw - cw, who))
    b.close()
for x in bad: print("OVERFLOW", *x)
print("ok: no horizontal overflow" if not bad else f"{len(bad)} widths overflow")
sys.exit(1 if bad else 0)
