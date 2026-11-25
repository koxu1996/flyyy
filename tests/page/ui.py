# The Nodes page and Apps' empty states in a browser, against mock.py: Add node's two steps, the edge's listen
# default, the Edges table's Listen column, and the "two things before your first app" checklist. argv: base
# URL, chromium path.
import sys
from playwright.sync_api import sync_playwright
base, chromium = sys.argv[1], sys.argv[2]
failed = []


def check(cond, good, bad):
    print(("ok   " if cond else "FAIL ") + (good if cond else bad))
    if not cond:
        failed.append(bad)


with sync_playwright() as p:
    b = p.chromium.launch(executable_path=chromium, args=["--disable-dev-shm-usage"])
    pg = b.new_page(viewport={"width": 1300, "height": 900})
    vis = lambda sel: pg.locator(sel).is_visible()

    def scenario(name, hash):
        pg.goto(f"{base}/__scenario/{name}")
        pg.goto(f"{base}/#{hash}")
        pg.reload()

    # Edges table: no role tag, a Listen column in the edge's words.
    scenario("full", "nodes")
    pg.wait_for_selector(".lrow.cprow.erow:not(.head)")
    rows = pg.locator(".lrow.cprow.erow:not(.head)")
    listen = [rows.nth(i).locator(".lsn").inner_text() for i in range(rows.count())]
    check(listen == ["[::]:443 · IPv4 + IPv6", "0.0.0.0:8443"], f"Edges: Listen {listen}", f"Edges: Listen {listen}")
    check(pg.locator(".lrow.cprow.erow .role").count() == 0, "Edges: no role tag by the names", "Edges: a role tag by a name")

    # Add node: the kind step first, then the form, with Back.
    pg.click("#mesh-token")
    pg.wait_for_selector("#tk-start", state="visible")
    check(vis("#tk-start") and not vis("#tk-form") and pg.locator("#tk-start .choice").count() == 2 and pg.inner_text("#tk-title") == "Add node",
          "Add node: two choices first (Worker, Edge)", "Add node: no kind step")
    check("KVM" in pg.inner_text("#tk-start [data-kind=worker]") and "public IP" in pg.inner_text("#tk-start [data-kind=edge]"), "Add node: each choice says what it's for", "Add node: choice texts")
    pg.click("#tk-start [data-kind=edge]")
    pg.click("#tk-next")
    check(vis("#tk-form") and vis("#tk-edge") and vis("#tk-back") and pg.inner_text("#tk-title") == "Add edge", "Add node: Edge, Continue: the edge form, with Back", "Add node: no edge form")
    check(pg.input_value("#tk-listen") == "[::]:443" and "IPv4 + IPv6" in pg.inner_text("label[for=tk-listen]") and "--listen '[::]:443'" in pg.inner_text("#tk-cmd"),
          "Add edge: Listen [::]:443 (HTTPS, IPv4 + IPv6), in the command", f"Add edge: listen {pg.input_value('#tk-listen')!r}, command {pg.inner_text('#tk-cmd')!r}")
    pg.click("#tk-back")
    check(vis("#tk-start") and not vis("#tk-form"), "Add node: Back to the choices", "Add node: Back didn't go back")
    pg.dblclick("#tk-start [data-kind=worker]")
    check(vis("#tk-form") and not vis("#tk-edge") and "#join" in pg.inner_text("#tk-cmd"), "Add node: double-click Worker: the worker form, #join's command", "Add node: no worker form")
    pg.click("#tk-done")

    # A browser that saved the old default listen gets the new one; its other fields stay.
    pg.evaluate("""localStorage.setItem("flyyy.add-edge", JSON.stringify({ listen: "0.0.0.0:8443", cert: "/etc/x/cert.pem", key: "" }))""")
    pg.click("#mesh-token")
    pg.wait_for_selector("#tk-start", state="visible")
    pg.click("#tk-start [data-kind=edge]")
    pg.click("#tk-next")
    check(pg.input_value("#tk-listen") == "[::]:443" and pg.input_value("#tk-cert") == "/etc/x/cert.pem", "Add edge: a saved 0.0.0.0:8443 gives way to [::]:443; a saved certificate stays",
          f"Add edge: listen {pg.input_value('#tk-listen')!r}, cert {pg.input_value('#tk-cert')!r}")
    pg.click("#tk-done")

    # Apps, nothing there yet: the checklist.
    scenario("bare", "apps")
    pg.wait_for_selector(".ready")
    t = pg.inner_text("#apps")
    check("Two things before your first app" in t and "+ Add worker" in t and "+ Add edge" in t and pg.locator("#apps [data-act=new]").is_disabled(),
          "Apps, bare: the checklist, both Add buttons, New app disabled", f"Apps, bare: {t!r}")
    pg.click("#apps [data-act=add-edge]")
    pg.wait_for_selector("#tk-form", state="visible")
    check(vis("#tk-form") and vis("#tk-edge") and pg.inner_text("#tk-title") == "Add edge", "Apps checklist: + Add edge opens Add node at the edge form", "Apps checklist: Add edge")
    pg.click("#tk-done")
    scenario("bare", "apps")
    pg.wait_for_selector(".ready")
    pg.click("#apps [data-act=add-worker]")
    pg.wait_for_selector("#tk-form", state="visible")
    check(vis("#tk-form") and not vis("#tk-edge") and pg.inner_text("#tk-title") == "Add worker", "Apps checklist: + Add worker opens Add node at the worker form", "Apps checklist: Add worker")
    pg.click("#tk-done")

    scenario("pending", "apps")
    pg.wait_for_selector(".ready")
    t = pg.inner_text("#apps")
    check("nuc: waiting for approval" in t and "✓ edgevps1" in t and pg.locator("#apps .ready a[href='#nodes']").count() == 1 and pg.locator("#apps [data-act=new]").is_disabled(),
          "Apps, a worker pending: it's waiting for approval (a link to Nodes), ✓ edgevps1", f"Apps, pending: {t!r}")

    scenario("ready", "apps")
    pg.wait_for_selector("#apps .empty")
    t = pg.inner_text("#apps")
    check("Nothing running yet" in t and pg.locator(".ready").count() == 0 and not pg.locator("#apps [data-act=new]").is_disabled(),
          "Apps, a worker and an edge: the usual empty state, New app enabled", f"Apps, ready: {t!r}")

    # An org user never sees it: no workers, no mesh.
    pg.goto(f"{base}/__scenario/bare")
    pg.route("**/v1/whoami", lambda r: r.fulfill(json={"user": "ann", "org": "acme"}))
    pg.route("**/v1/workers", lambda r: r.fulfill(status=403, json={"error": "operator only"}))
    pg.route("**/v1/mesh", lambda r: r.fulfill(status=403, json={"error": "operator only"}))
    pg.goto(f"{base}/#apps")
    pg.reload()
    pg.wait_for_selector("#apps .empty")
    check(pg.locator(".ready").count() == 0 and "Nothing running yet" in pg.inner_text("#apps"), "Apps, an org user: the usual empty state", f"Apps, org user: {pg.inner_text('#apps')!r}")
    b.close()
print(f"{len(failed)} failed" if failed else "ok: all passed")
sys.exit(1 if failed else 0)
