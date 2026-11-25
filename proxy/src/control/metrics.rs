//! `GET /v1/apps/<name>/metrics?window=5m|1h`: the app's metrics, merged from where they're kept
//! (metrics.rs): its traffic from every edge in the `edges` table, its machines' CPU and memory from the
//! workers it has machines on, each asked over the mesh with the control plane's certificate (as load.rs).
//! A node that didn't answer is listed in `missing`, so a gap shows rather than as lower numbers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use corro_api_types::Statement;
use hyper::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

use super::{load::EDGES, model::App, Control};
use crate::{
    corro::query,
    metrics::{epoch_now, percentile, Traffic, Usage, BUCKET_S},
    util::now_ms,
};

#[derive(Deserialize)]
struct EdgeMetrics {
    services: HashMap<String, Vec<Traffic>>,
}

#[derive(Deserialize)]
struct WorkerMetrics {
    machines: BTreeMap<String, Vec<Usage>>,
}

fn round(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

impl Control {
    async fn ask<T: DeserializeOwned>(&self, addr: std::net::SocketAddr, path: &str) -> Result<T, String> {
        match self.agents.call(addr, Method::GET, path, None).await? {
            (StatusCode::OK, b) => serde_json::from_slice(&b).map_err(|e| format!("bad answer: {e}")),
            (code, b) => Err(format!("{}: {}", code.as_u16(), String::from_utf8_lossy(&b))),
        }
    }

    /// The app's last `n` buckets, as the API answers them.
    pub(crate) async fn app_metrics(&self, app: &App, n: i64) -> Result<Value, String> {
        let edges: Vec<(String, String)> = query(&self.client, Statement::Simple(EDGES.into())).await?;
        let machines: Vec<_> = self.machines().await?.into_iter().filter(|m| m.app == app.name).collect();
        let rows = self.workers().await?;
        let on: BTreeSet<&str> = machines.iter().map(|m| m.worker.as_str()).filter(|w| !w.is_empty()).collect();
        let (edge_path, worker_path) = (format!("/metrics?app={}&n={n}", app.name), format!("/v1/metrics?app={}&n={n}", app.name));
        let edge_asks = edges.iter().map(|(name, announced)| async {
            let r = async { self.ask::<EdgeMetrics>(self.edge_addr(announced)?, &edge_path).await };
            (name.clone(), r.await)
        });
        let worker_asks = on.iter().map(|name| async {
            let r = async {
                let w = rows.iter().find(|w| w.name == *name).ok_or("no workers row")?;
                self.ask::<WorkerMetrics>(self.bound(w, &w.agent_api)?, &worker_path).await
            };
            (name.to_string(), r.await)
        });
        let (edges, workers) = futures::future::join(futures::future::join_all(edge_asks), futures::future::join_all(worker_asks)).await;

        let now = epoch_now();
        let first = now - n + 1;
        let slot = |epoch: i64| (first..=now).contains(&epoch).then(|| (epoch - first) as usize);
        let mut missing = Vec::new();
        let mut traffic = vec![Traffic::default(); n as usize];
        for (name, r) in &edges {
            match r {
                Ok(m) => {
                    for t in m.services.values().flatten() {
                        if let Some(i) = slot(t.epoch) {
                            traffic[i].add(t);
                        }
                    }
                }
                Err(e) => missing.push(json!({ "kind": "edge", "name": name, "error": e })),
            }
        }
        let mut usage: BTreeMap<String, (String, Vec<Option<Usage>>)> = BTreeMap::new();
        for m in machines.iter().filter(|m| m.live()) {
            usage.insert(m.id.clone(), (m.worker.clone(), vec![None; n as usize]));
        }
        for (name, r) in &workers {
            match r {
                Ok(w) => {
                    for (id, buckets) in &w.machines {
                        let (_, series) = usage.entry(id.clone()).or_insert_with(|| (name.clone(), vec![None; n as usize]));
                        for u in buckets.iter().filter(|u| u.samples > 0) {
                            if let Some(i) = slot(u.epoch) {
                                series[i] = Some(u.clone());
                            }
                        }
                    }
                }
                Err(e) => missing.push(json!({ "kind": "worker", "name": name, "error": e })),
            }
        }

        let mut total = Traffic::default();
        for t in &traffic {
            total.add(t);
        }
        let col = |f: &dyn Fn(&Traffic) -> Value| traffic.iter().map(f).collect::<Vec<Value>>();
        let pct = |q: f64| move |t: &Traffic| percentile(&t.latency, q).into();
        // The window's seconds so far: the current bucket only up to now.
        let seconds = (now_ms() as f64 / 1000.0 - (first * BUCKET_S) as f64).max(1.0);
        let machines: Vec<Value> = usage
            .into_iter()
            .map(|(id, (worker, series))| {
                let cpu: Vec<Value> = series.iter().map(|u| u.as_ref().map(|u| round(u.cpu / u.samples as f64, 3)).into()).collect();
                let memory: Vec<Value> = series.iter().map(|u| u.as_ref().and_then(|u| u.memory).into()).collect();
                let latest = series.iter().rev().flatten().next();
                json!({
                    "id": id, "worker": worker, "cpu": cpu, "memory": memory,
                    "cpu_now": latest.map(|u| round(u.cpu / u.samples as f64, 3)), "memory_now": latest.and_then(|u| u.memory),
                })
            })
            .collect();
        Ok(json!({
            "app": app.name, "bucket_s": BUCKET_S, "window_s": n * BUCKET_S, "memory_mb": app.memory, "cpus": app.cpus,
            "t": (first..=now).map(|e| e * BUCKET_S).collect::<Vec<_>>(),
            "requests": {
                "2xx": col(&|t| t.status[0].into()), "3xx": col(&|t| t.status[1].into()), "4xx": col(&|t| t.status[2].into()),
                "5xx": col(&|t| t.status[3].into()), "edge_502": col(&|t| t.edge_502.into()), "edge_503": col(&|t| t.edge_503.into()),
            },
            "latency_ms": { "p50": col(&pct(0.5)), "p95": col(&pct(0.95)), "p99": col(&pct(0.99)) },
            "bytes_in": col(&|t| t.bytes_in.into()), "bytes_out": col(&|t| t.bytes_out.into()),
            "tcp_opened": col(&|t| t.tcp_opened.into()), "tcp_open": col(&|t| t.tcp_open.into()),
            "totals": {
                "requests": total.requests(), "rps": round(total.requests() as f64 / seconds, 2),
                "2xx": total.status[0], "3xx": total.status[1], "4xx": total.status[2], "5xx": total.status[3],
                "edge_502": total.edge_502, "edge_503": total.edge_503,
                "p50_ms": percentile(&total.latency, 0.5), "p95_ms": percentile(&total.latency, 0.95), "p99_ms": percentile(&total.latency, 0.99),
                "bytes_in": total.bytes_in, "bytes_out": total.bytes_out, "tcp_opened": total.tcp_opened,
                "tcp_open": traffic.last().map_or(0, |t| t.tcp_open),
            },
            "machines": machines,
            "asked": { "edges": edges.len(), "workers": workers.len() },
            "missing": missing,
        }))
    }
}
