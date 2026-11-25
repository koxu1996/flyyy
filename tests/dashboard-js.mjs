// The dashboard's shHighlight, outside a browser: node dashboard-js.mjs <dashboard.html>. The text must
// come back unchanged (copy reads it), every < > & escaped, and each piece in its span.
import fs from "fs";
const s = fs.readFileSync(process.argv[2], "utf8");
const a = s.indexOf("const esc = "), b = s.indexOf("\n}\n", s.indexOf("function shHighlight")) + 3;
// esc, the SH_ sets and shHighlight, as the page declares them.
const { shHighlight } = new Function(s.slice(a, b) + "; return { shHighlight };")();
const un = (h) => h.replace(/<[^>]+>/g, "").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, "&");
const spans = (h) => [...h.matchAll(/<span class="sh-(\w+)">([^<]*)<\/span>/g)].map((m) => `${m[1]}:${un(m[2])}`);
let fail = 0;
const eq = (name, got, want) => { const ok = JSON.stringify(got) === JSON.stringify(want); if (!ok) fail++; console.log(ok ? "ok  " : "FAIL", name, ok ? "" : `\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`); };
const edge = ["nix run 'github:x/flyyy#edge' --", "--name edge1", "--listen 0.0.0.0:8443", "--apps-domain flyyy.example.com",
  "--tls-cert ~/'my certs'/cert.pem", "--tls-key ~/flyyy-tls/key.pem", "'flyyy-join:kkk=:10.250.255.3:ccc=@203.0.113.9:51820'"].join(" \\\n  ");
const h1 = shHighlight(edge);
eq("edge: round trip", un(h1), edge);
eq("edge: spans", spans(h1), ["cmd:nix", "sub:run", "str:'github:x/flyyy#edge'", "op:--", "op:\\", "flag:--name", "op:\\", "flag:--listen", "op:\\",
  "flag:--apps-domain", "op:\\", "flag:--tls-cert", "str:'my certs'", "op:\\", "flag:--tls-key", "op:\\", "str:'flyyy-join:kkk=:10.250.255.3:ccc=@203.0.113.9:51820'"]);
eq("edge: 7 lines, continuation lines aren't commands", h1.split("\n").length, 7);
const dep = "flyyy deploy web --image traefik/whoami@sha256:ab --service 'http=http:80,path=/healthz' --count 2 --env 'A=it'\\''s' -- '-text={id}'";
const h2 = shHighlight(dep);
eq("deploy: round trip", un(h2), dep);
eq("deploy: spans", spans(h2), ["cmd:flyyy", "sub:deploy", "flag:--image", "flag:--service", "str:'http=http:80,path=/healthz'", "flag:--count", "flag:--env",
  "str:'A=it'", "str:'s'", "op:--", "str:'-text={id}'"]);
const h3 = shHighlight("flyyy users create <user> --org <org>");
eq("users: two subs, placeholders", spans(h3), ["cmd:flyyy", "sub:users", "sub:create", "ph:<user>", "flag:--org", "ph:<org>"]);
eq("orgs destroy status: an app named like a sub is an argument", spans(shHighlight("flyyy destroy status")), ["cmd:flyyy", "sub:destroy"]);
const nasty = `echo a<b & "c>d" --x=<y> # 1 < 2 && <script>`;
const h4 = shHighlight(nasty);
eq("escaped: round trip", un(h4), nasty);
eq("escaped: no raw < > & outside tags", /[<>]/.test(h4.replace(/<\/?span[^>]*>/g, "")) || /&(?!amp;|lt;|gt;|quot;|#39;)/.test(h4), false);
eq("escaped: spans", spans(h4), ["cmd:echo", 'str:"c>d"', "flag:--x", "ph:<y>", "com:# 1 < 2 && <script>"]);
eq("sudo/env/prompt", spans(shHighlight("$ sudo env FOO=1 nix run x")), ["op:$ ", "cmd:sudo", "cmd:env", "cmd:nix", "sub:run"]);
// githubUrl: the Source link's href from the page's flake URL; "" when it isn't GitHub.
const g = s.indexOf("function githubUrl"), { githubUrl } = new Function(s.slice(g, s.indexOf("\n}\n", g) + 3) + "; return { githubUrl };")();
const gh = (f) => githubUrl(f);
eq("github: git+https with a ref (repo)", gh("git+https://github.com/koxu1996/flyyy?ref=main"), "https://github.com/koxu1996/flyyy/tree/main");
eq("github: git+ssh, .git, a ref with a slash", gh("git+ssh://git@github.com/o/r.git?ref=feat/x"), "https://github.com/o/r/tree/feat/x");
eq("github: github:o/r", gh("github:example/flyyy"), "https://github.com/example/flyyy");
eq("github: github:o/r/branch", gh("github:example/flyyy/main"), "https://github.com/example/flyyy/tree/main");
eq("github: https", gh("https://github.com/o/r"), "https://github.com/o/r");
eq("github: git+https with a ref", gh("git+https://github.com/o/r?ref=dev"), "https://github.com/o/r/tree/dev");
eq("github: not GitHub", ["git+ssh://git@gitlab.com/o/r?ref=x", "path:/srv/flyyy", "gitlab:o/r", "https://github.com.evil.example/o/r", "https://example.com/github.com/o/r", ""].map(gh),
  ["", "", "", "", "", ""]);
// defaultNodeName: the name a node takes from its hostname, as wg::default_node_name (its Rust tests' cases).
const d = s.indexOf("function defaultNodeName"), { defaultNodeName } = new Function(s.slice(d, s.indexOf("\n}\n", d) + 3) + "; return { defaultNodeName };")();
eq("node name: as wg::default_node_name", ["Nuc\n", "My_Host.local", "1box", "nixAir", "x".repeat(40), "9" + "y".repeat(40)].map(defaultNodeName),
  ["nuc", "my-host-local", "n1box", "nixair", "x".repeat(32), "n9" + "y".repeat(30)]);
process.exit(fail ? 1 : 0);
