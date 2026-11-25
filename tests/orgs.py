# orgs: two orgs (red, blue), a user in each, and the operator. The operator's token (the control plane's
# own) alone creates orgs and users and uses the cluster's routes (workers, mesh, orgs, users). A user's token
# sees and changes only its org's apps: another org's app is a 404, as if it weren't there. App names stay
# global (each is a vhost). Tokens are never stored, only their SHA-256, in rows every node gets a copy of.
import hashlib
WHOAMI = image("traefik/whoami")


def as_user(user: str, args: str) -> tuple[int, str]:
    return control.execute(f"FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE=/etc/{user}.token flyyy {args} 2>&1")


def apps_of(user: str) -> list[str]:
    rc, out = as_user(user, "ls --json")
    assert rc == 0, out
    return sorted(a["name"] for a in json.loads(out))


# 1. The operator makes the orgs and a user in each; each token is printed once and stored only hashed.
fly("orgs create red")
fly("orgs create blue")
tokens = {}
for user, org, role in [("alice", "red", "admin"), ("bob", "blue", "member")]:
    tokens[user] = fly(f"users create {user} --org {org} --role {role}").strip()
    control.succeed(f"printf %s {shlex.quote(tokens[user])} > /etc/{user}.token && chmod 600 /etc/{user}.token")
check(all(re.fullmatch(r"flyyy_[0-9a-f]{64}", t) for t in tokens.values()), "users: alice (red, admin) and bob (blue, member) got 256-bit tokens", f"users: tokens {tokens}")
listed = json.loads(fly("users --json"))
check([(u["name"], u["org"], u["role"]) for u in listed] == [("alice", "red", "admin"), ("bob", "blue", "member")] and all(set(u) == {"name", "org", "role"} for u in listed),
      "users: the list has names, orgs and roles, no token or hash", f"users: {listed}")
# Users stay with the control plane: no users table in gossip, and its store holds each token's SHA-256,
# never the token (nor does its log).
rc, out = worker1.execute(f"curl -s {CORRO_AUTH} -H 'content-type: application/json' -d {shlex.quote(json.dumps(['SELECT name FROM users', []]))} http://127.0.0.1:8080/v1/queries")
check("no such table" in out, "users: not in Corrosion (worker1 has no users table)", f"users: worker1's Corrosion: {out[:200]}")
store = control.succeed("cat /var/lib/flyyy-control/store.json")
rows = sorted([u, v["token_sha256"]] for u, v in json.loads(store)["users"].items())
dump = store + control.succeed("journalctl -u control --no-pager")
check(rows == [[u, hashlib.sha256(tokens[u].encode()).hexdigest()] for u in ["alice", "bob"]] and not any(t in dump for t in tokens.values()),
      "users: the control plane keeps each token's SHA-256, never the token (nor does its log)", f"users: stored {rows}")

# 2. Each user deploys into its own org; both serve.
for user, app in [("alice", "red-web"), ("bob", "blue-web")]:
    rc, out = as_user(user, f"deploy {app} --image {WHOAMI} --port 80 --count 1 --timeout {DEPLOY}")
    check(rc == 0, f"deploy: {user} deployed {app}", f"deploy: {user} {app}: {rc} {out}")
check(until(lambda: code("red-web.flyyy.localhost") == "200" and code("blue-web.flyyy.localhost") == "200", PROPAGATE),
      "edges: red-web and blue-web both serve", "edges: an org's app doesn't serve")
check(status("red-web")["org"] == "red" and status("blue-web")["org"] == "blue", "deploy: each app belongs to its user's org", "deploy: wrong orgs")

# 3. Each sees only its own; the other's app is a 404 to status, scale and destroy, and stays as it was.
for user, mine, theirs in [("alice", "red-web", "blue-web"), ("bob", "blue-web", "red-web")]:
    check(apps_of(user) == [mine], f"isolation: {user} lists only {mine}", f"isolation: {user} lists {apps_of(user)}")
    for args in [f"status {theirs}", f"scale {theirs} --count 3 --detach", f"destroy {theirs} --detach"]:
        rc, out = as_user(user, args)
        check(rc != 0 and "404" in out, f"isolation: {user}: `{args.split()[0]} {theirs}` -> 404", f"isolation: {user} {args}: {rc} {out}")
    st = status(theirs)
    check((st["state"], st["count"]) == ("active", 1), f"isolation: {theirs} untouched (active, count 1)", f"isolation: {theirs} {st['state']} {st['count']}")
    # Names are global: the other's name is taken, and a user can't deploy into another org.
    rc, out = as_user(user, f"deploy {theirs} --image {WHOAMI} --port 80 --count 1 --detach")
    check(rc != 0 and "409" in out and "taken" in out, f"names: {user} can't take {theirs} (409)", f"names: {user} deploy {theirs}: {rc} {out}")
    other = "blue" if user == "alice" else "red"
    rc, out = as_user(user, f"deploy {user}-x --image {WHOAMI} --port 80 --count 1 --org {other} --detach")
    check(rc != 0 and "400" in out, f"names: {user} can't deploy into org {other} (400)", f"names: {user} --org {other}: {rc} {out}")
    own_status = json.loads(as_user(user, f"status {mine} --json")[1])
    check(own_status["name"] == mine and own_status["machines"], f"isolation: {user} sees {mine} and its machines", f"isolation: {user} status {mine}: {own_status}")

# 4. The cluster's routes are the operator's; a user gets 403 there, a bad token 401 everywhere.
for args in ["workers", "mesh", "orgs", "users", "orgs create green", "users create carol --org red", "token"]:
    rc, out = as_user("alice", args)
    check(rc != 0 and "403" in out, f"operator only: alice `{args}` -> 403", f"operator only: alice {args}: {rc} {out}")
control.succeed("printf %s flyyy_nope > /etc/mallory.token")
rc, out = as_user("mallory", "ls")
check(rc != 0 and "401" in out, "auth: an unknown token -> 401", f"auth: unknown token: {rc} {out}")
check(fly_rc("users create carol --org green")[1].count("404") == 1, "users: a user in a missing org -> 404", "users: missing org not refused")
check("409" in fly_rc("users create alice --org blue")[1], "users: a taken user name -> 409", "users: taken name not refused")
check("409" in fly_rc("orgs create red")[1], "orgs: an existing org -> 409", "orgs: existing org not refused")

# 5. The operator sees both, with their orgs, and can manage either.
mine = {a["name"]: a["org"] for a in json.loads(fly("ls --json"))}
check(mine == {"red-web": "red", "blue-web": "blue"}, "operator: lists both apps with their orgs", f"operator: {mine}")
check(re.search(r"(?m)^red-web +red ", fly("ls")) is not None, "operator: flyyy ls shows the ORG column", f"operator: {fly('ls')}")
fly(f"scale blue-web --count 2 --timeout {DEPLOY}")
check(json.loads(as_user("bob", "status blue-web --json")[1])["count"] == 2, "operator: scaled bob's blue-web to 2 (bob sees it)", "operator: scale not seen by bob")
orgs = {o["name"]: (o["state"], o["apps"], o["users"]) for o in json.loads(fly("orgs --json"))}
check(orgs == {"red": ("active", 1, 1), "blue": ("active", 1, 1)}, "operator: flyyy orgs counts each org's apps and users", f"operator: orgs {orgs}")

# 6. An org with active apps can't be destroyed; once they're gone it can, and its users' tokens stop working.
rc, out = fly_rc("orgs destroy red")
check(rc != 0 and "409" in out and "red-web" in out, "orgs: destroying red refused while red-web is active (409)", f"orgs: destroy red: {rc} {out}")
rc, out = as_user("alice", f"destroy red-web --timeout {DEPLOY}")
check(rc == 0, "orgs: alice destroyed red-web", f"orgs: alice destroy: {rc} {out}")
fly("orgs destroy red")
check(json.loads(fly("orgs --json"))[1]["state"] == "destroyed", "orgs: red destroyed", f"orgs: {fly('orgs --json')}")
rc, out = as_user("alice", "ls")
check(rc != 0 and "401" in out, "orgs: alice's token stops working with red destroyed (401)", f"orgs: alice after red destroyed: {rc} {out}")
check(apps_of("bob") == ["blue-web"] and code("blue-web.flyyy.localhost") == "200", "orgs: bob and blue-web unaffected", "orgs: blue affected")
