//! The apps: deploy, scale, destroy (each waits until converged, unless --detach), ls, status, ips, metrics.

use std::{
    collections::HashMap,
    process::ExitCode,
    time::{Duration, Instant},
};

use hyper::Method;
use proxy::{client::Client, services};
use serde_json::Value;

use crate::{
    out::{s, short, show, table},
    Json, Wait,
};

#[derive(clap::Args, Debug)]
pub(crate) struct Deploy {
    app: String,
    /// `<repo>@sha256:<digest>`.
    #[arg(long)]
    image: String,
    /// The shortcut for one http service, `--service http=http:N`.
    #[arg(long)]
    port: Option<u64>,
    /// NAME=http:PORT or NAME=tcp:PORT[@EDGE_PORT], then ,path=/p ,interval=S ,timeout=S for its check
    /// (repeatable). tcp is published on the app's own IP, on EDGE_PORT or else PORT; at most one http
    /// service, routed at <app>.<apps-domain>.
    #[arg(long, value_parser = services::parse_flag)]
    service: Vec<services::Service>,
    #[arg(long)]
    count: u64,
    /// vCPUs per machine (the API's default: 1).
    #[arg(long)]
    cpus: Option<u64>,
    /// MiB per machine (the API's default: 128).
    #[arg(long)]
    memory: Option<u64>,
    /// K=V (repeatable; a later K wins).
    #[arg(long, value_parser = env_var)]
    env: Vec<(String, String)>,
    /// The operator's: a user's token deploys into its own org.
    #[arg(long)]
    org: Option<String>,
    /// Gone: refused with where the command goes now.
    #[arg(long = "cmd", hide = true, value_parser = cmd_gone)]
    old_cmd: Option<String>,
    #[command(flatten)]
    wait: Wait,
    /// Replaces the image's Cmd, exactly as your shell split it; {id} becomes the machine id.
    #[arg(last = true)]
    cmd: Vec<String>,
}

fn cmd_gone(_: &str) -> Result<String, String> {
    Err("--cmd is gone: put the command after `--`, e.g. `flyyy deploy app ... -- -text={id}`".into())
}

fn env_var(kv: &str) -> Result<(String, String), String> {
    let (k, v) = kv.split_once('=').ok_or_else(|| format!("{kv:?}: want K=V"))?;
    Ok((k.to_owned(), v.to_owned()))
}

pub(crate) async fn ls(client: &Client, json: &Json) -> Result<ExitCode, String> {
    let apps = client.ok(Method::GET, "/v1/apps", None).await?;
    show(json, &apps, &|apps| {
        let mut rows = vec![["NAME", "ORG", "IMAGE", "PORT", "STARTED", "STATE"].map(String::from).to_vec()];
        for a in apps.as_array().into_iter().flatten() {
            rows.push(vec![
                s(&a["name"]),
                s(&a["org"]),
                short(&s(&a["image"])),
                a["port"].to_string(),
                format!("{}/{}", a["started"], a["count"]),
                s(&a["state"]),
            ]);
        }
        table(rows);
    });
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn status(client: &Client, app: &str, json: &Json) -> Result<ExitCode, String> {
    let app = client.ok(Method::GET, &format!("/v1/apps/{app}"), None).await?;
    show(json, &app, &print_status);
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn ips(client: &Client, app: &str, json: &Json) -> Result<ExitCode, String> {
    // Fly's `ips list`: the app's dedicated address (its tcp services), then the shared ones (HTTP).
    let app = client.ok(Method::GET, &format!("/v1/apps/{app}"), None).await?;
    if json.json {
        println!("{}", app["ips"]);
    } else {
        let mut rows = vec![["ADDRESS", "TYPE", "SERVES"].map(String::from).to_vec()];
        let tcp: Vec<String> = app["services"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v["protocol"] == "tcp")
            .map(|v| format!("{} :{}", s(&v["name"]), v["edge_port"].as_u64().unwrap_or(v["port"].as_u64().unwrap_or(0))))
            .collect();
        for i in app["ips"].as_array().into_iter().flatten() {
            let serves = if i["type"] == "dedicated" {
                if tcp.is_empty() {
                    "-".into()
                } else {
                    format!("tcp {}", tcp.join(", "))
                }
            } else {
                format!("http(s) {}.<apps-domain>", s(&app["name"]))
            };
            rows.push(vec![s(&i["ip"]), format!("{} v4", s(&i["type"])), serves]);
        }
        table(rows);
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn deploy(client: &Client, d: Deploy) -> Result<ExitCode, String> {
    let env: serde_json::Map<String, Value> = d.env.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    let mut body = serde_json::json!({ "image": d.image, "count": d.count, "cmd": d.cmd, "env": env });
    // --port is the shortcut for one http service; the API validates both forms.
    match (d.port, d.service.is_empty()) {
        (Some(port), true) => body["port"] = port.into(),
        (None, false) => body["services"] = serde_json::to_value(d.service).expect("serialisable"),
        (Some(_), false) => return Err("--port is a shortcut for one http service: use it or --service, not both".into()),
        (None, true) => return Err("want --port <n> or --service <spec>".into()),
    }
    if let Some(org) = d.org {
        body["org"] = org.into();
    }
    // Unset: the API's defaults. The API validates the range.
    for (v, key) in [(d.cpus, "cpus"), (d.memory, "memory")] {
        if let Some(v) = v {
            body[key] = v.into();
        }
    }
    change(client, "deploy", &d.app, Method::PUT, Some(body), &d.wait).await
}

pub(crate) async fn scale(client: &Client, app: &str, count: u64, wait: &Wait) -> Result<ExitCode, String> {
    change(client, "scale", app, Method::PATCH, Some(serde_json::json!({ "count": count })), wait).await
}

pub(crate) async fn destroy(client: &Client, app: &str, wait: &Wait) -> Result<ExitCode, String> {
    change(client, "destroy", app, Method::DELETE, None, wait).await
}

/// A change to an app, then (unless --detach) the wait until it converged: exit 2 if it didn't in time.
async fn change(client: &Client, verb: &str, app: &str, method: Method, body: Option<Value>, wait: &Wait) -> Result<ExitCode, String> {
    client.ok(method, &format!("/v1/apps/{app}"), body).await?;
    println!("{verb} {app}: accepted");
    let timeout = Duration::from_secs(wait.timeout);
    if wait.detach || converged(client, app, timeout).await? {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("flyyy: {app} not converged after {} s (see `flyyy status {app}`)", timeout.as_secs());
        Ok(ExitCode::from(2))
    }
}

pub(crate) fn print_status(app: &Value) {
    println!(
        "{}  {}  port {}  {} cpu {} MiB  {}/{} started  {}{}",
        s(&app["name"]),
        s(&app["image"]),
        app["port"],
        app["cpus"],
        app["memory"],
        app["started"],
        app["count"],
        s(&app["state"]),
        if app["converged"] == true { "  converged" } else { "" }
    );
    if app["org"].as_str().is_some_and(|o| !o.is_empty()) {
        println!("org: {}", s(&app["org"]));
    }
    let addrs = |kind: &str| -> Vec<String> { app["ips"].as_array().into_iter().flatten().filter(|i| i["type"] == kind).map(|i| s(&i["ip"])).collect() };
    let (dedicated, shared) = (addrs("dedicated"), addrs("shared"));
    if !(dedicated.is_empty() && shared.is_empty()) {
        println!(
            "ips: {} dedicated, {} shared",
            if dedicated.is_empty() { "-".into() } else { dedicated.join(" ") },
            if shared.is_empty() { "-".into() } else { shared.join(" ") }
        );
    }
    for svc in app["services"].as_array().into_iter().flatten() {
        let at = match s(&svc["protocol"]).as_str() {
            // Published on the app's own address; edge_port unset is the VM port.
            "tcp" => format!("{}:{}", dedicated.first().map_or("-", String::as_str), svc["edge_port"].as_u64().unwrap_or(svc["port"].as_u64().unwrap_or(0))),
            _ => format!("https://{}.<apps-domain>/", s(&app["name"])),
        };
        let c = &svc["check"];
        let check = if s(&svc["protocol"]) == "http" {
            c["path"].as_str().map_or("GET / (any answer below 500)".to_owned(), |p| format!("GET {p}"))
        } else {
            "connect".into()
        };
        // Concurrency per instance, per edge (balance.rs): "-" for none.
        let limit = |k: &str| svc[k].as_u64().map_or("-".to_owned(), |n| n.to_string());
        println!(
            "service {}: {} port {}, {at}; check {check} every {} s: {}/{} passing; limits soft {} hard {}",
            s(&svc["name"]),
            s(&svc["protocol"]),
            svc["port"],
            c["interval"],
            svc["passing"],
            app["count"],
            limit("soft_limit"),
            limit("hard_limit")
        );
    }
    match app["image_bytes"].as_u64() {
        Some(b) => println!("image size: {:.1} MiB ({b} bytes)", b as f64 / 1048576.0),
        None => println!("image size: not built yet"),
    }
    if app["cmd"].as_array().is_some_and(|a| !a.is_empty()) {
        println!("cmd: {}", app["cmd"]);
    }
    if app["env"].as_array().is_some_and(|a| !a.is_empty()) {
        let names: Vec<String> = app["env"].as_array().into_iter().flatten().map(s).collect();
        println!("env: {} (values not shown)", names.join(" "));
    }
    let mut rows = vec![["ID", "WORKER", "DESIRED", "STATE", "ADDR", "ERROR"].map(String::from).to_vec()];
    for m in app["machines"].as_array().into_iter().flatten() {
        rows.push(["id", "worker", "desired", "state", "addr", "error"].iter().map(|k| s(&m[*k])).collect());
    }
    if rows.len() > 1 {
        table(rows);
    }
}

/// Bytes for people: B, KiB, MiB, GiB.
fn bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1048576 => format!("{:.1} KiB", n as f64 / 1024.0),
        1048576..1073741824 => format!("{:.1} MiB", n as f64 / 1048576.0),
        _ => format!("{:.1} GiB", n as f64 / 1073741824.0),
    }
}

pub(crate) async fn metrics(client: &Client, app: &str, window: &str, json: &Json) -> Result<ExitCode, String> {
    let m = client.ok(Method::GET, &format!("/v1/apps/{app}/metrics?window={window}"), None).await?;
    show(json, &m, &print_metrics);
    Ok(ExitCode::SUCCESS)
}

/// The window's totals, then each machine's CPU and memory now, then who didn't answer.
pub(crate) fn print_metrics(m: &Value) {
    let t = &m["totals"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let ms = |v: &Value| v.as_f64().map_or("-".to_owned(), |v| format!("{v} ms"));
    println!("{}  last {} s  (asked {} edges, {} workers)", s(&m["app"]), m["window_s"], m["asked"]["edges"], m["asked"]["workers"]);
    println!(
        "requests  {} ({}/s): 2xx {}  3xx {}  4xx {}  5xx {} (the edges' own: 502 {}, 503 {})",
        n(&t["requests"]),
        t["rps"],
        n(&t["2xx"]),
        n(&t["3xx"]),
        n(&t["4xx"]),
        n(&t["5xx"]),
        n(&t["edge_502"]),
        n(&t["edge_503"])
    );
    println!("latency   p50 {}  p95 {}  p99 {}", ms(&t["p50_ms"]), ms(&t["p95_ms"]), ms(&t["p99_ms"]));
    println!("bytes     in {}  out {}", bytes(n(&t["bytes_in"])), bytes(n(&t["bytes_out"])));
    if n(&t["tcp_opened"]) > 0 || n(&t["tcp_open"]) > 0 {
        println!("tcp       {} connections opened, {} open", n(&t["tcp_opened"]), n(&t["tcp_open"]));
    }
    let mut rows = vec![["MACHINE", "WORKER", "CPU", "MEMORY"].map(String::from).to_vec()];
    for v in m["machines"].as_array().into_iter().flatten() {
        let cpu = v["cpu_now"].as_f64().map_or("-".to_owned(), |c| format!("{:.0}%", c * 100.0));
        let memory = v["memory_now"].as_u64().map_or("-".to_owned(), |b| format!("{} of {} MiB", bytes(b), m["memory_mb"]));
        rows.push(vec![s(&v["id"]), s(&v["worker"]), cpu, memory]);
    }
    if rows.len() > 1 {
        table(rows);
    }
    for g in m["missing"].as_array().into_iter().flatten() {
        println!("missing: {} {} didn't answer ({}): its share isn't counted", s(&g["kind"]), s(&g["name"]), s(&g["error"]));
    }
}

/// Poll the app until converged, printing a line whenever its machine states change.
async fn converged(client: &Client, app: &str, timeout: Duration) -> Result<bool, String> {
    let t0 = Instant::now();
    let mut last = String::new();
    loop {
        let v = client.ok(Method::GET, &format!("/v1/apps/{app}"), None).await?;
        let mut states: HashMap<String, usize> = HashMap::new();
        for m in v["machines"].as_array().into_iter().flatten() {
            let state = if m["desired"] == "destroyed" { "destroying".to_owned() } else { s(&m["state"]).replace('-', "pending") };
            *states.entry(state).or_default() += 1;
        }
        let mut parts: Vec<_> = states.into_iter().map(|(k, n)| format!("{n} {k}")).collect();
        parts.sort();
        let line = format!("{}/{} started{}{}", v["started"], v["count"], if parts.is_empty() { "" } else { ": " }, parts.join(", "));
        if line != last {
            println!("[{:5.1}s] {line}", t0.elapsed().as_secs_f64());
            last = line;
        }
        if v["converged"] == true {
            println!("{app}: converged in {:.1} s", t0.elapsed().as_secs_f64());
            return Ok(true);
        }
        if t0.elapsed() > timeout {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
