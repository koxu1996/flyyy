# dashboard: `flyyy dashboard` on the control container's loopback. It serves the page and /config.json, and
# forwards the page's API calls to the control plane with the CLI's token, so the token never reaches the
# browser. Only /v1/apps*, GET /v1/workers and the mesh's list, create, tokens and approve are forwarded.
# A wrong Host (DNS rebinding) is refused, and so is a write without the page's Origin and JSON (CSRF).
from typing import Any
WHOAMI = image("traefik/whoami")
ENV = "FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE=/etc/control.token"
D = "http://127.0.0.1:4040"
ORIGIN = "-H 'Origin: http://127.0.0.1:4040' -H 'content-type: application/json'"


def dash_unit(unit: str, args: str, env: str = ENV) -> None:
    sets = " ".join(f"-p Environment={e}" for e in env.split())
    control.succeed(f"systemd-run --unit={unit} {sets} $(command -v flyyy) dashboard {args}")


def req(method: str, path: str, extra: str = "", base: str = D) -> tuple[str, dict, str]:
    """(status, lowercased headers, body) of one request to the dashboard."""
    out = control.succeed(f"curl -s -i --max-time {SLOW_REQUEST} -X {method} {extra} {base}{path} | tr -d '\\r'")
    head, _, body = out.partition("\n\n")
    lines = head.splitlines()
    headers = dict((k.strip().lower(), v.strip()) for k, _, v in (l.partition(":") for l in lines[1:]))
    return lines[0].split(" ")[1] if lines else "", headers, body


def ok_json(method: str, path: str, extra: str = "") -> Any:
    st, _, body = req(method, path, extra)
    assert st.startswith("2"), f"{method} {path}: {st} {body}"
    return json.loads(body)


TOKEN = control.succeed("cat /etc/control.token").strip()
# The join command's flake by default (dash.rs FLAKE_URL, nix/modules/lib.nix flakeUrl).
FLAKE = "git+https://github.com/koxu1996/flyyy?ref=main"
dash_unit("dash", "")
control.wait_for_open_port(4040)

# 1. The page and its config, with headers that keep it to itself.
st, h, page = req("GET", "/")
check(st == "200" and h.get("content-type", "").startswith("text/html") and "<title>flyyy</title>" in page and 'fetch("/config.json")' in page,
      "page: GET / serves the embedded dashboard", f"page: {st} {h}")
csp = h.get("content-security-policy", "")
check("default-src 'none'" in csp and "connect-src 'self'" in csp and "frame-ancestors 'none'" in csp and h.get("x-content-type-options") == "nosniff"
      and "font-src https://fonts.gstatic.com;" in csp and h.get("cache-control") == "no-store",
      "page: strict CSP (same-origin requests only, no framing; Google's fonts only), nosniff, no-store", f"page: headers {h}")
check(ok_json("GET", "/config.json") == {"apps_url": "http://{app}.flyyy.localhost:8000/", "flake_url": FLAKE, "auth": "local"},
      "config: apps_url defaults to http://{app}.flyyy.localhost:8000/, the join command's flake to this repo, auth local (the CLI's token)",
      f"config: {req('GET', '/config.json')[2]}")
tpl = ok_json("GET", "/templates.json")
items = [t for g in tpl for t in g["items"]]
check(page.count("loadTemplates") >= 2 and items and all("@sha256:" in t["image"] for t in items) and any(t["name"] == "whoami" for t in items),
      f"templates: /templates.json serves the embedded {len(tpl)} groups, {len(items)} templates, each pinned by digest", f"templates: {len(tpl)} groups, {items[:2]}")
# + Add node: one dialog, first Worker or Edge (New app's choices), then one command with the token in it (the
# page builds it; tests/page/ui.py runs it in a browser). Here: the page has every piece, and the old token
# display is gone.
parts = ["+ Add node</button>", 'id="tk-start" class="choices"', 'data-kind="worker"', 'data-kind="edge"', '<span class="em">🖥️</span><b>Worker</b>',
         '<span class="em">🌐</span><b>Edge</b>', 'id="tk-back"', "#join' --", "#edge' --",
         "--apps-domain", "--tls-cert", "--tls-key", '" \\\\\\n  "', "Works once, for 24 hours. The node shows up here for you to approve.",
         '"flyyy.add-edge"', 'EDGE_DEFAULTS = { listen: "[::]:443", cert: "~/flyyy-tls/cert.pem", key: "~/flyyy-tls/key.pem" }',
         'const OLD_LISTEN = "0.0.0.0:8443";', "if (saved.listen === OLD_LISTEN) delete saved.listen;", "Nothing to run apps on yet", 'class="btn primary" id="add-node-empty"']
check(all(p in page for p in parts) and "tk-token" not in page and "Join token" not in page,
      "page: + Add node, Worker or Edge first, the command over several lines, the edge's defaults remembered per browser ([::]:443 over a saved old default); no separate token",
      f"page: missing {[p for p in parts if p not in page]}, or the old token display")
check("join command${m.unused_tokens === 1 ? \"\" : \"s\"} not used yet (expire in 24 h)" in page and "control plane key" not in page,
      "page: the Nodes subtitle counts unused join commands (no control plane key)", "page: the old Nodes subtitle")
check(page.index('data-start="template"') < page.index('data-start="image"') and 'class="choice rec" data-start="template"' in page
      and '<span class="rec-badge">Recommended</span>' in page and 'startPick = "template";' in page and '$("#step-start .choice.on")?.focus();' in page,
      "page: New app starts at From a template (first, Recommended, selected and focused); Your own image second", "page: the New app choices")
# New app's first two steps: a click selects (the accent border), Continue goes on (disabled on the
# template step until a card is selected); a double-click or Enter goes straight on; Back keeps it.
parts = ['<button type="button" class="btn primary" id="deploy-next">Continue</button>', '$("#deploy-next").onclick = goOn;',
         '$("#deploy-next").disabled = step === "template" && !TEMPLATES.some((t) => t.name === chosen);', '$("#deploy-next").hidden = next === "config";',
         'el.addEventListener("dblclick", (e) => { if (e.target.closest(sel)) goOn(); });', 'const b = e.key === "Enter" && e.target.closest(sel);',
         'selectable($("#step-start"), "[data-start]"', 'selectable($("#f-templates"), "[data-i]"', ".choice.on, .choice.alt.on { background: var(--accent-soft); border-color: var(--accent); }"]
check(all(p in page for p in parts) and "showStep(\"config\");\n};" not in page,
      "page: New app's choices and templates select; Continue (or a double-click, or Enter) goes on", f"page: missing {[p for p in parts if p not in page]}")
# The tab bar draws its underline inside the buttons (40 px, no negative margin), so it never scrolls
# vertically; Configure's controls are 40 px like its inputs; Memory is its own row, 7 columns (4 on phones).
parts = [".tabs button { height: 40px; padding: 0 14px;", ".tabs button.on { color: var(--ink); box-shadow: inset 0 -2px 0 var(--accent); }", "overflow-x: auto; overflow-y: hidden; }",
         "#deploy-form .stepper { height: 40px; padding: 3px; }", "#deploy-form .seg button { height: 34px; }",
         "#f-memory { display: grid; grid-template-columns: repeat(7, minmax(0, 1fr)); }", "@media (max-width: 560px) {\n    #f-memory { grid-template-columns: repeat(4, minmax(0, 1fr)); }",
         ".frow.name-port { grid-template-columns: minmax(0, 1fr) 120px; }"]
check(all(p in page for p in parts) and "margin-bottom: -1px" not in page and "#f-memory { flex-wrap: wrap; }" not in page,
      "page: the tab bar 40 px with its underline inside (no scrolling); Configure's 40 px controls; Memory's own 7-column row",
      f"page: missing {[p for p in parts if p not in page]}, or the old tab underline / memory wrap")
# The app's VMs are "VMs" in the UI ("1 VM", "2 VMs"); "machine" stays only for a host. Ids, classes and
# data-* attributes keep their names.
parts = ['<svg class="i"><use href="#i-layers"/></svg>VMs</label>', '<div class="t">VMs</div>', 'aria-label="VMs"', '<b>${n}</b> VM${n === 1 ? "" : "s"}',
         'for ${count} VM${count === 1 ? "" : "s"}', "'<div class=\"quiet\">No VMs.</div>'", '<div class="lrow mtrow head"><span>VM</span>', 'chart("CPU per VM (average)"',
         "chart(`Memory per VM (now, of", '${w.machines} VM${w.machines === 1 ? "" : "s"}', '<span class="ds">VMs</span>', '`Its ${a.started} running VM${a.started === 1 ? "" : "s"} will be',
         'toast(`Scaling ${name} to ${n} VM${n === 1 ? "" : "s"}`)', "{id} becomes the VM's id."]
gone = [">Machines<", ">Machine<", 'cell("Machines"', "`Machines (", "No machines.", '"CPU per machine', "`Memory per machine", "running machine", "machine${", "which machine answered", 'aria-label="Machines"']
check(all(p in page for p in parts) and not any(g in page for g in gone), "page: the app's VMs are called VMs (1 VM, 2 VMs) everywhere in the UI",
      f"page: missing {[p for p in parts if p not in page]}, or left {[g for g in gone if g in page]}")
# No edge (none joined, approved or local; pending doesn't count): a note on Nodes with + Add edge (Add node
# on Edge), the amber line on the operator's Apps list and app page, and the Traffic tab says why it's empty.
parts = ['const noEdge = () => !!(mesh?.nodes && !mesh.none && !mesh.error) && !mesh.nodes.some((n) => n.role === "edge" && n.state !== "pending");',
         "No edge: apps aren't reachable from outside.<button type=\"button\" class=\"linkbtn\" id=\"add-edge-note\">+ Add edge</button>",
         'if (e.target.closest("#add-edge-note")) return addNode("edge");', 'tkStep(kind ? "form" : "start");',
         "No edge: apps aren't reachable.<a href=\"#nodes\" data-act=\"add-edge\">Add an edge →</a>", '+ (noEdge() ? NO_EDGE : "") + appList(shown)',
         '${noEdge() ? NO_EDGE : ""}\n    <div class="stats strip" id="strip">', 'case "add-edge": location.hash = "#nodes"; return addNode("edge");',
         "const noEdges = d.asked?.edges === 0;", "No edge is serving this app, so there's no traffic to show."]
check(all(p in page for p in parts), "page: no edge: a note on Nodes (+ Add edge), a line on Apps and the app page, and the Traffic tab says so",
      f"page: missing {[p for p in parts if p not in page]}")
# A link to the repo on GitHub, from /config.json's flake_url (githubUrl; its cases, GitHub and not, are in
# tests/dashboard-js.mjs): the sidebar's Source row above the profile button, and the sign-in form. Hidden
# when the flake isn't on GitHub; nothing is fetched from GitHub.
parts = ["function githubUrl(flake) {", 'const gh = githubUrl(flakeUrl);', 'for (const a of [$("#src-link"), $("#si-src")]) { a.hidden = !gh; if (gh) a.href = gh; }',
         '<a class="foot src" id="src-link" target="_blank" rel="noopener" title="Source on GitHub" aria-label="Source on GitHub" hidden><svg class="i"><use href="#i-github"/></svg><span class="label">Source</span></a>\n    <button class="foot prof" id="who"',
         '<a class="si-src" id="si-src" target="_blank" rel="noopener" hidden><svg class="i"><use href="#i-github"/></svg>Source on GitHub</a>', '<symbol id="i-github" viewBox="0 0 16 16">']
check(all(p in page for p in parts) and "api.github.com" not in page, "page: a Source on GitHub link from the flake URL (sidebar, sign in), hidden when it isn't GitHub",
      f"page: missing {[p for p in parts if p not in page]}")
# The Nodes page, top to bottom: pending joins, a Control plane card (facts: state, mesh IP, the endpoint
# nodes dial, peers connected in the last 3 minutes, the API), Edges (N) (or the no-edge note), any
# Dashboards (N), Workers (N).
parts = ['<div class="section-title">Control plane</div><div class="cpcard">', 'fact("Endpoint", m.control?.endpoint', 'fact("Peers", peers.length ? `${connected} of ${peers.length} connected`',
         "n.handshake_age_s != null && n.handshake_age_s < 180", 'fact("API", m.control?.api', 'nodeTable("Edges", "Edge", edges, true)', '<div class="section-title">Edges (0)</div><div class="nowork">No edge',
         'nodeTable("Dashboards", "Dashboard", dashes)', '<div class="section-title">${title} (${rows.length})</div>', '<div class="section-title">Workers (${wrows.length})</div>',
         'host: n.state === "local" ? "the control host" : n.hostname', "$(\"#nodes\").innerHTML = (asks ? `<div class=\"asks\">${asks}</div>` : \"\") + cpTable + wTable;",
         "@media (max-width: 600px) { .cpcard { grid-template-columns: repeat(2, minmax(0, 1fr)); } }"]
check(all(p in page for p in parts) and "Control plane and edges" not in page and "API and mesh hub" not in page,
      "page: Nodes: pending joins, the Control plane card, Edges (N) or the no-edge note, Workers (N)", f"page: missing {[p for p in parts if p not in page]}")
# Node names: any 1-32 of [a-z0-9-] starting with a letter (no workerN). Add node has one optional Name for
# a worker and an edge alike (the node's hostname otherwise), in the command only when filled; no nextFree.
# "on <host>" is left out when the name is the one the hostname gives (defaultNodeName, as wg::default_node_name).
parts = ['<input type="text" id="tk-name" placeholder="the machine\'s hostname"', "const NODE_NAME = /^[a-z][a-z0-9-]{0,31}$/;", "function defaultNodeName(host) {",
         'const name = $("#tk-name").value.trim(), named = name ? [`--name ${sh(name)}`] : [];', "[`nix run '${flakeUrl}#join' --`, ...named]", "[`nix run '${flakeUrl}#edge' --`, ...named,",
         '$("#tk-name").classList.toggle("invalid", !!name && (!NODE_NAME.test(name) || seenNames().includes(name)));',
         'const onHost = (name, host) => (host && defaultNodeName(host) !== name ? `on ${esc(host)}` : "");']
check(all(p in page for p in parts) and "nextFree" not in page and "workerN" not in page, "page: Add node's one optional Name (the hostname otherwise); no \"nuc on nuc\"",
      f"page: missing {[p for p in parts if p not in page]}")
# Under every flyyy command (deploy, orgs and users, confirm), once: how to get the CLI, from the page's flake.
check(page.count('<div class="cli-hint"') == 3 and "No flyyy here? <span class=\"mono\">nix profile install '${esc(flakeUrl)}#flyyy'</span>" in page
      and '$("#confirm-cli-row").hidden = $("#confirm-cli-hint").hidden = !cli;' in page,
      "page: \"No flyyy here? nix profile install '<flake>#flyyy'\" under each flyyy command", "page: the CLI hints")
# Orgs & users: the operator's page, its dialogs' commands (the deploy dialog's layout, a copy button), and
# who's signed in; Nodes and Orgs & users are hidden until /v1/whoami says operator.
parts = ['data-page="nodes" hidden', 'data-page="orgs" hidden', "Orgs &amp; users</span>", 'id="who"', '"/v1/whoami"', "if (me?.operator) {",
         "`flyyy orgs create ${n}`", "`flyyy users create ${n} --org ${", "`flyyy orgs destroy ${o.name}`", 'data-copy="od-cli"', 'data-copy="confirm-cli"',
         "You won't see it again. Send it to them now.", 'data-copy="od-token"', "+ New org</button>", "+ New user</button>"]
check(all(p in page for p in parts), "page: Orgs & users (+ New org, + New user with its token once, destroy), each dialog's flyyy command; who's signed in",
      f"page: missing {[p for p in parts if p not in page]}")
# The app page's events (polled) and each machine's logs (a dialog); the Configure row that must fit.
parts = ['id="events-panel"', "`/v1/apps/${app}/events`", "`/v1/apps/${app}/machines/${id}/logs`", 'data-logs="${esc(m.id)}"', 'id="logs-text"',
         ".frow.svc-row { grid-template-columns: auto minmax(0, 1fr); }", "f.image.setSelectionRange(0, 0)"]
check(all(p in page for p in parts) and '["events", "Events"]]' in page and 'id="events-panel"></div></div>' not in page, "page: Events as the app page's tab, Logs per machine; the Configure row fits; the image shows its start",
      f"page: missing {[p for p in parts if p not in page]}")
# The app page: a live strip, then the Configuration, Traffic, VMs and Events tabs (the tab kept across refreshes);
# no Load panel, metrics summary line or stat tiles any more.
parts = ['id="strip"', 'cell("VMs"', 'cell("Health"', 'cell("Traffic"', 'cell("Latency p95"', 'cell("Errors"', '["config", "Configuration"]', '["traffic", "Traffic"]',
         "`VMs (${ms.length})`", 'let detailTab = "config"', "values hidden", "function renderTraffic(", 'class="grid"', "now</span>", "function renderMachinesTab(",
         'class="mt-err"', "repeat(5, minmax(0, 1fr))", "repeat(3, minmax(0, 1fr))"]
check(all(p in page for p in parts) and not any(p in page for p in ['id="load-panel"', 'id="metrics-panel"', 'class="lsum"', 'class="ldrow', "on each worker's disk</div>"]),
      "page: the app page's strip and tabs (Configuration, Traffic, VMs, Events); the old Load panel, metrics line and stat tiles gone",
      f"page: missing {[p for p in parts if p not in page]}")
# Sign in: a full page (the shell hidden), the brand panel and the form side by side; where a token comes from.
parts = ['<div id="signin" hidden>', "body:has(#signin:not([hidden])) .shell { display: none; }", 'class="brandside"', 'class="net"', "users</text>", "edges</text>",
         "VMs</text>", "<h2>Turn your machines into a cloud.</h2>", "<p>Containers run as Cloud Hypervisor microVMs across all of them, scale in seconds, and the edges balance every request.</p>", "#e2531a 0%, var(--accent) 50%, #ff9a55 100%", 'class="formside"', '<form id="signin-form" novalidate>', 'id="si-token"', 'id="si-err"',
         "With your API token. It stays in this tab.", '<details class="si-help">', "Where do I get a token?", "flyyy users create alice --org acme",
         "/var/lib/flyyy-operator/token", "@media (prefers-reduced-motion: reduce)", 'if ($("#signin").hidden) showSignIn();']
check(all(p in page for p in parts) and 'class="scrim" id="signin"' not in page,
      "page: sign in is a full page (brand panel, the form, where to get a token); no longer a dialog over the shell",
      f"page: missing {[p for p in parts if p not in page]}, or the old dialog")
# The shell commands are highlighted (checks.dashboard-js tests shHighlight itself); copy still reads the text.
parts = ["function shHighlight(text)", '$("#tk-cmd").innerHTML = shHighlight(', '$("#od-cli").innerHTML = shHighlight(', '$("#confirm-cli").innerHTML = shHighlight(',
         '<span class="sh-op sh-prompt">$ </span>${shHighlight(cli)}', 'querySelectorAll(".si-help .blk")', ".sh-prompt { user-select: none;", "--sh-cmd:",
         "navigator.clipboard.writeText($(`#${c.dataset.copy}`).textContent)"]
check(all(p in page for p in parts), "page: the dialogs' shell commands highlighted (Add node, New app, orgs/users, confirm, sign in); copy reads the text",
      f"page: missing {[p for p in parts if p not in page]}")
# The Workbench look (IBM Plex from Google Fonts, warm tokens, square controls) and the empty states: the
# emoji (as before), a line on what to do, and that step as a button.
parts = ["family=IBM+Plex+Sans:wght@400;500;600;700&family=IBM+Plex+Mono", '--sans: "IBM Plex Sans"', "--bg: #f6f3ee", "--bg: #161412", "--radius: 6px;",
         "box-shadow: inset 0 -2px 0 rgba(0,0,0,.18)", '<div class="empty"><div class="big">🛰️</div>', '<div class="empty"><div class="big">🖥️</div>\n      <h2>No workers yet</h2>', '<div class="big">🧩</div>',
         "Nothing running yet", "Nothing running right now", "Start with whoami: it's up in about 10 seconds and tells you which VM answered.",
         "Add a worker: you get one command to paste on it.", "Orgs keep apps apart: each user sees only their own org's.", 'class="btn primary" id="new-org-empty"',
         "There's no app called ${esc(detailOf)}", '<a href="#apps">Back to apps</a>', '<div class="big">🪁</div>', '<div class="big">🏢</div><h2>No orgs yet</h2>',
         '<div class="big">🔍</div><h2>There\'s no app called', "`✓ means we ran it on flyyy (${", 'title="We ran it on flyyy"',
         "toast(`Deploying ${s.name}…`)", ">not 80 or 443<", "(values hidden)"]
check(all(p in page for p in parts) and not any(p in page for p in ["motif", "SF Pro Text", "docs/APP_COVERAGE.md", "newest first, kept by the control plane"]),
      "page: the Workbench look (Plex, warm tokens), empty states with a next step (🪁, 🏢, 🔍; 🛰️ and 🖥️ on Nodes, 🧩 on Apps before the cluster is ready), the shorter copy",
      f"page: missing {[p for p in parts if p not in page]}, or old copy left")
# No workers: the Nodes page's Workers section says so (the control plane and edges listed above it), and
# the operator's Apps page too: its "two things before your first app" checklist, or an amber line above
# the apps. Each offers + Add worker, which opens Add node at the worker form. Org users get workers = null
# and no mesh, so neither.
parts = ['id="add-worker-empty">+ Add worker</button>', "Apps run on workers: add one and paste its command on that machine.",
         'if (e.target.closest("#add-worker-empty")) return addNode("worker");',
         ": noWorkers || noEdge() ? notReady() : `", "<h2>Two things before your first app</h2>",
         'const noWorkers = me?.operator === true && Array.isArray(workers) && !workers.some((w) => w.alive);',
         '${step(1, "A worker", "Runs the VMs.", live.length > 0, live, pending("worker"), "add-worker", "+ Add worker")}',
         '${step(2, "An edge", "Serves them to the internet.", !noEdge(), edges, pending("edge"), "add-edge", "+ Add edge")}',
         "No live workers: VMs can't start.<a href=\"#nodes\" data-act=\"add-worker\">Add a worker →</a>",
         'case "add-worker": location.hash = "#nodes"; return addNode("worker");', "function showToken(t, kind) {"]
check(all(p in page for p in parts), "page: no workers: a Workers empty state on Nodes, and on Apps (the checklist, or an amber line), each with + Add worker",
      f"page: missing {[p for p in parts if p not in page]}")
# Each node's host ("on nuc", from /v1/mesh's hostname; nothing for nodes from before it) under its name in
# both tables and in a pending join's banner; the control plane's from /v1/mesh's control.
parts = ['host: m.control?.hostname', 'host: n.state === "local" ? "the control host" : n.hostname, node: n, state: state(n) }', 'host: n.hostname, node: n, w, state: state(n, w) }',
         'const parts = [onHost(r.name, r.host), ...rest].filter(Boolean);', '${name(cp)}${under(cp)}', '<div>${name({ ...r, role: "" })}${under(r)}</div>', "under(r, what)",
         '<b>${esc(n.name)}</b>${onHost(n.name, n.hostname) ? ` ${onHost(n.name, n.hostname)}` : ""} asks to join']
check(all(p in page for p in parts), "page: Nodes show each node's host (\"on nuc\") in both tables and the join banners",
      f"page: missing {[p for p in parts if p not in page]}")
# One profile button: the avatar and who (the org, or operator), opening a menu with Sign out (#sign-out,
# hidden for `flyyy dashboard`); Esc or a click outside closes it. The old who/Sign out rows are gone.
parts = ['<button class="foot prof" id="who" aria-haspopup="menu"', 'id="who-av"', 'id="who-sub"', '<div class="who-menu" id="who-menu" role="menu" hidden>',
         '<button id="sign-out" role="menuitem" hidden>Sign out</button>', '$("#sign-out").hidden = auth !== "token";', "Signed in as <b>the operator</b>",
         "Signed in as <b>${esc(me.user)}</b>, org <b>${esc(me.org)}</b>", 'if (!e.target.closest("#who, #who-menu")) closeWhoMenu();',
         'if (!$("#who-menu").hidden) { closeWhoMenu(); $("#who").focus(); }', ".av { width: 30px; height: 30px; border-radius: 50%;"]
check(all(p in page for p in parts) and 'class="foot who"' not in page and 'title="Sign out"' not in page,
      "page: one profile button (avatar, name, org) with a menu: who, then Sign out (token auth only); Esc and outside clicks close it",
      f"page: missing {[p for p in parts if p not in page]}, or the old rows")
dash_unit("dash2", "--listen 127.0.0.1:4041 --apps-url 'https://{app}.flyyy.example.com/' --flake-url github:example/flyyy")
control.wait_for_open_port(4041)
c2 = json.loads(req("GET", "/config.json", "-H 'Host: 127.0.0.1:4041'", "http://127.0.0.1:4041")[2])
check(c2 == {"apps_url": "https://{app}.flyyy.example.com/", "flake_url": "github:example/flyyy", "auth": "local"}, "config: --apps-url and --flake-url are what /config.json says", f"config: {c2}")

# 2. Forwarded reads, answered with the CLI's token (the request carries none, or a wrong one).
check([w["name"] for w in ok_json("GET", "/v1/workers")] == ["worker1", "worker2", "worker3"], "forward: GET /v1/workers lists the 3 workers", "forward: workers")
mesh = ok_json("GET", "/v1/mesh")
check(sorted(n["name"] for n in mesh["nodes"]) == ["edges", "worker1", "worker2", "worker3"], "forward: GET /v1/mesh lists the nodes", f"forward: mesh {mesh}")
check(ok_json("GET", "/v1/apps", "-H 'Authorization: Bearer nope'") == [], "forward: GET /v1/apps with the CLI's token, not the browser's (a wrong one is ignored)",
      "forward: apps with a bad browser token")

# 3. Forwarded writes from the page: deploy, scale, a join token, approve (of a node that isn't there), destroy.
spec = json.dumps({"image": WHOAMI, "port": 80, "count": 1})
st, _, body = req("PUT", "/v1/apps/dash-web", f"{ORIGIN} -d {shlex.quote(spec)}")
check(st == "202", "forward: PUT /v1/apps/dash-web deploys (202)", f"forward: deploy {st} {body}")
check(until(lambda: status("dash-web")["converged"], DEPLOY), "forward: dash-web converged", "forward: dash-web never converged")
check(until(lambda: code("dash-web.flyyy.localhost") == "200", PROPAGATE), "forward: dash-web serves through the edges", "forward: dash-web doesn't serve")
st, _, _ = req("PATCH", "/v1/apps/dash-web", f"{ORIGIN} -d '{{\"count\": 2}}'")
check(st == "202" and until(lambda: status("dash-web")["count"] == 2, PROPAGATE), "forward: PATCH scales dash-web to 2", f"forward: scale {st}")
detail = ok_json("GET", "/v1/apps/dash-web")
check(detail["name"] == "dash-web" and len(detail["machines"]) == 2, "forward: GET /v1/apps/dash-web has its machines", f"forward: detail {detail}")
before = mesh["unused_tokens"]
t = ok_json("POST", "/v1/mesh/tokens", ORIGIN)
check(t["token"].startswith("flyyy-join:") and ok_json("GET", "/v1/mesh")["unused_tokens"] == before + 1, "forward: POST /v1/mesh/tokens makes a join token",
      f"forward: token {t}")
st, _, body = req("POST", "/v1/mesh/nodes/nope/approve", ORIGIN)
check(st == "404" and "nope" in body, "forward: approve reaches the control plane (404: no node nope)", f"forward: approve {st} {body}")
st, _, body = req("POST", "/v1/mesh", ORIGIN)
check(st == "409", "forward: POST /v1/mesh (create) reaches the control plane (409: it exists)", f"forward: init {st} {body}")
check(ok_json("GET", "/v1/whoami") == {"operator": True}, "forward: GET /v1/whoami says operator (the CLI's token)", f"forward: whoami {req('GET', '/v1/whoami')[2]}")
st, _, body = req("POST", "/v1/orgs", f"{ORIGIN} -d '{{\"name\": \"teal\"}}'")
st2, _, body2 = req("POST", "/v1/users", f"{ORIGIN} -d '{{\"name\": \"tom\", \"org\": \"teal\"}}'")
tom = json.loads(body2).get("token", "") if st2 == "201" else ""
teal = next((o for o in ok_json("GET", "/v1/orgs") if o["name"] == "teal"), {})
check(st == "201" and len(tom) >= 32 and teal.get("users") == 1 and {"name": "tom", "org": "teal", "role": "member"} in ok_json("GET", "/v1/users"),
      "forward: + New org teal, + New user tom (its token in the answer, once)", f"forward: org {st} {body}, user {st2} {body2}, teal {teal}")
whoami_tom = control.succeed(f"curl -s -H 'authorization: Bearer {tom}' http://127.0.0.1:4000/v1/whoami")
check(json.loads(whoami_tom) == {"user": "tom", "org": "teal"}, "whoami: tom's token is tom, of teal", f"whoami: {whoami_tom}")
st, _, body = req("DELETE", "/v1/orgs/teal", ORIGIN)
gone = control.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' -H 'authorization: Bearer {tom}' http://127.0.0.1:4000/v1/whoami")
check(st == "202" and gone == "401", "forward: destroying teal works, and tom's token stops working", f"forward: destroy {st} {body}, tom then {gone}")

# 4. The token never reaches the browser: not in any answer above, nor in a fresh look at all of them.
seen = "".join(req("GET", p)[2] for p in ["/", "/config.json", "/v1/apps", "/v1/apps/dash-web", "/v1/workers", "/v1/mesh"])
check(TOKEN not in seen and TOKEN not in page, "token: in no page or answer the dashboard serves", "token: leaked to the browser")

# 5. DNS rebinding: any other Host is refused, for the page and for the API alike.
for host in ["evil.example:4040", "127.0.0.1.nip.io:4040", "localhost:80"]:
    codes = [req(m, p, f"-H 'Host: {host}' {ORIGIN if m != 'GET' else ''}")[0] for m, p in [("GET", "/"), ("GET", "/v1/apps"), ("POST", "/v1/mesh/tokens")]]
    check(codes == ["421", "421", "421"], f"rebinding: Host {host} -> 421 for the page, reads and writes", f"rebinding: Host {host}: {codes}")
check(req("GET", "/", "-H 'Host: localhost:4040'")[0] == "200", "rebinding: Host localhost:4040 is fine", "rebinding: localhost refused")

# 6. CSRF: a write without the page's Origin, from another origin, or not JSON (a form), doesn't happen.
n0 = ok_json("GET", "/v1/mesh")["unused_tokens"]
for what, extra in [("no Origin", "-H 'content-type: application/json'"), ("Origin evil.example", "-H 'Origin: http://evil.example' -H 'content-type: application/json'"),
                    ("a form (text/plain)", "-H 'Origin: http://127.0.0.1:4040' -H 'content-type: text/plain'")]:
    codes = [req(m, p, extra)[0] for m, p in [("POST", "/v1/mesh/tokens"), ("DELETE", "/v1/apps/dash-web"), ("POST", "/v1/mesh/nodes/worker1/approve"),
                                              ("POST", "/v1/orgs"), ("DELETE", "/v1/orgs/red")]]
    check(codes == ["403"] * 5, f"csrf: {what} -> 403 for tokens, destroy, approve and org writes", f"csrf: {what}: {codes}")
check(ok_json("GET", "/v1/mesh")["unused_tokens"] == n0 and status("dash-web")["state"] == "active", "csrf: no token made, dash-web still active", "csrf: a refused write happened")

# 7. Only what the page needs is forwarded: not the mesh's own routes, no other org or user routes, no path tricks.
for m, p in [("GET", "/v1/orgs/red"), ("DELETE", "/v1/users"), ("DELETE", "/v1/orgs/..%2fapps"), ("GET", "/v1/mesh/peers"), ("POST", "/v1/mesh/join"), ("POST", "/v1/workers"),
             ("GET", "/v1/apps/dash-web/x"), ("GET", "/v1/apps/..%2forgs"), ("GET", "/v1/mesh/tokens")]:
    st, _, body = req(m, p, ORIGIN if m != "GET" else "")
    check(st == "404" and "not forwarded" in body, f"routes: {m} {p} -> 404, not forwarded", f"routes: {m} {p}: {st} {body}")
check("path=/v1/orgs/red " not in control.succeed("journalctl -u control --no-pager"),
      "routes: the control plane never saw GET /v1/orgs/red", "routes: /v1/orgs/red reached the control plane")

# 8. A user's token: forwarded the same way, and the control plane scopes it (workers are the operator's).
fly("orgs create red")
control.succeed(f"{FLY} users create alice --org red > /etc/alice.token")
dash_unit("dash3", "--listen 127.0.0.1:4043", "FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE=/etc/alice.token")
control.wait_for_open_port(4043)
st, _, body = req("GET", "/v1/workers", "-H 'Host: 127.0.0.1:4043'", "http://127.0.0.1:4043")
apps = json.loads(req("GET", "/v1/apps", "-H 'Host: 127.0.0.1:4043'", "http://127.0.0.1:4043")[2])
check(st == "403" and apps == [], "user: alice's dashboard gets 403 for workers and sees none of the operator's apps", f"user: workers {st}, apps {apps}")
a_who = json.loads(req("GET", "/v1/whoami", "-H 'Host: 127.0.0.1:4043'", "http://127.0.0.1:4043")[2])
a_codes = [req("GET", p, "-H 'Host: 127.0.0.1:4043'", "http://127.0.0.1:4043")[0] for p in ["/v1/orgs", "/v1/users", "/v1/mesh"]]
# The page shows Nodes and Orgs & users only for {operator: true}: alice gets neither (and so none of these 403s).
check(a_who == {"user": "alice", "org": "red"} and a_codes == ["403", "403", "403"],
      "user: whoami says alice of red, so her page hides Nodes and Orgs & users (whose calls are 403 for her)", f"user: whoami {a_who}, orgs/users/mesh {a_codes}")

# 10. The public dashboard (`proxy dashboard` on pubdash, a mesh node of role dashboard): HTTPS, no token of
# its own; the page signs in, and each call carries its caller's token to the control plane over the mesh.
PD = "https://dash.flyyy.localhost:8443"
PD_CURL = "curl -s -i --max-time 15 --cacert /var/lib/dash-tls/cert.pem --resolve dash.flyyy.localhost:8443:127.0.0.1"
PD_ORIGIN = "-H 'Origin: https://dash.flyyy.localhost' -H 'content-type: application/json'"


def pd(method: str, path: str, extra: str = "", host: str = "dash.flyyy.localhost") -> tuple[str, dict, str]:
    """(status, lowercased headers, body) of one request to the public dashboard, from its own container
    (as a browser behind the TLS passthrough would send it: Host without the port)."""
    out = pubdash.succeed(f"{PD_CURL} -H 'Host: {host}' -X {method} {extra} {PD}{path} | tr -d '\\r'")
    head, _, body = out.partition("\n\n")
    lines = head.splitlines()
    headers = dict((k.strip().lower(), v.strip()) for k, _, v in (l.partition(":") for l in lines[1:]))
    return lines[0].split(" ")[1] if lines else "", headers, body


def bearer(token: str) -> str:
    return f"-H 'Authorization: Bearer {token}'"


pubdash.start()
underlay_up(pubdash)
mesh_join(pubdash, "pubdash")
node = mesh_nodes()["pubdash"]
check(node["role"] == "dashboard" and node["state"] == "joined", "public: pubdash joined the mesh as a dashboard", f"public: {node}")
pubdash.wait_for_open_port(8443)
st, h, page = pd("GET", "/")
csp = h.get("content-security-policy", "")
check(st == "200" and "<title>flyyy</title>" in page and "script-src 'sha256-" in csp and "unsafe-inline" not in csp.split("style-src-attr")[0]
      and "connect-src 'self'" in csp and h.get("strict-transport-security", "").startswith("max-age="),
      "public: the page over HTTPS, with a strict CSP (inline script and style by hash) and HSTS", f"public: {st} {h}")
check(json.loads(pd("GET", "/config.json")[2]) == {"apps_url": "https://{app}.flyyy.localhost/", "flake_url": FLAKE, "auth": "token"}, "public: config says sign in with a token (auth token)",
      "public: config")
check(pd("GET", "/", host="evil.example")[0] == "421" and pd("GET", "/v1/apps", bearer(TOKEN), host="evil.example")[0] == "421",
      "public: another Host -> 421, for the page and the API", "public: another Host served")
# No token, a wrong one: 401, from the control plane or here; nothing forwarded acts as anyone.
codes = [pd("GET", "/v1/apps", a)[0] for a in ["", bearer("wrong-token-" + "0" * 40), "-H 'Authorization: Basic eA=='"]]
check(codes == ["401", "401", "401"], "public: no token, a wrong one or not a bearer -> 401", f"public: {codes}")
# Org scoping, by the caller's own token: alice (red) sees red's app only; the operator sees every app.
rc, out = control.execute(f"FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE=/etc/alice.token flyyy deploy red-web --image {WHOAMI} --port 80 --count 0 --detach 2>&1")
check(rc == 0, "public: alice deploys red-web (count 0) into red", f"public: alice deploy {rc} {out}")
alice = control.succeed("cat /etc/alice.token").strip()
st_a, _, body_a = pd("GET", "/v1/apps", bearer(alice))
st_o, _, body_o = pd("GET", "/v1/apps", bearer(TOKEN))
mine, everyone = sorted(a["name"] for a in json.loads(body_a)), sorted(a["name"] for a in json.loads(body_o))
check(st_a == "200" and mine == ["red-web"], "public: alice's token sees only red's apps (red-web)", f"public: alice {st_a} {mine}")
check(st_o == "200" and {"dash-web", "red-web"} <= set(everyone), f"public: the operator's token sees all ({', '.join(everyone)})", f"public: operator {st_o} {everyone}")
check(pd("GET", "/v1/workers", bearer(alice))[0] == "403" and pd("GET", "/v1/workers", bearer(TOKEN))[0] == "200",
      "public: workers are the operator's (alice 403, operator 200)", "public: workers scoping")
st, _, body = pd("GET", "/v1/apps/red-web/metrics?window=1h", bearer(alice))
check(st == "200" and json.loads(body)["window_s"] == 3600, "public: alice's red-web metrics, with ?window=1h passed on", f"public: metrics {st} {body[:200]}")
# Writes: the page's https Origin and JSON, with a token; another origin is refused before forwarding.
st, _, _ = pd("PATCH", "/v1/apps/red-web", f"{bearer(alice)} -H 'Origin: https://evil.example' -H 'content-type: application/json' -d '{{\"count\": 0}}'")
st2, _, _ = pd("PATCH", "/v1/apps/red-web", f"{bearer(alice)} {PD_ORIGIN} -d '{{\"count\": 0}}'")
check((st, st2) == ("403", "202"), "public: a write from another origin -> 403; from the page with alice's token -> 202", f"public: writes {st} {st2}")
check(pd("GET", "/v1/orgs", bearer(TOKEN))[0] == "200" and pd("GET", "/v1/orgs", bearer(alice))[0] == "403" and pd("GET", "/v1/orgs/red", bearer(TOKEN))[0] == "404",
      "public: orgs are the operator's (200; alice 403), and the allowlist still holds (GET /v1/orgs/red not forwarded)", "public: orgs")
check(json.loads(pd("GET", "/v1/whoami", bearer(alice))[2]) == {"user": "alice", "org": "red"} and json.loads(pd("GET", "/v1/whoami", bearer(TOKEN))[2]) == {"operator": True},
      "public: whoami by each caller's own token (alice of red; the operator)", "public: whoami")
# It holds no token: none in its unit, and the CA gave it no certificate.
unit = pubdash.succeed("systemctl cat flyyy-dashboard")
check(TOKEN not in unit and alice not in unit and pubdash.execute("test -e /var/lib/flyyy-tls/cert.pem")[0] != 0,
      "public: no token in its unit, no mesh certificate on it", "public: it holds a token or a certificate")
# Failed sign-ins are limited per address: past 10 in a minute (the 3 above count), 429 even with a good token.
codes = [pd("GET", "/v1/apps", bearer("guess-" + str(i) * 40))[0] for i in range(10)]
st = pd("GET", "/v1/apps", bearer(TOKEN))[0]
check(codes == ["401"] * 7 + ["429"] * 3 and st == "429", "public: after 10 failed sign-ins from one address, 429 (even with the operator's token)", f"public: {codes} then {st}")

# 9. Loopback only: a wildcard or LAN --listen is refused before binding.
rc, out = control.execute(f"{ENV} flyyy dashboard --listen 0.0.0.0:4042 2>&1")
check(rc != 0 and "loopback" in out, "listen: --listen 0.0.0.0 refused (loopback only)", f"listen: {rc} {out}")
st, _, _ = req("DELETE", "/v1/apps/dash-web", ORIGIN)
check(st == "202" and until(lambda: status("dash-web")["state"] == "destroyed", PROPAGATE), "forward: DELETE destroys dash-web", f"forward: destroy {st}")
