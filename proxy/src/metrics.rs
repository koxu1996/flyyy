//! Per-app metrics, kept where they happen, without Prometheus: 10 s buckets for the last hour. Each edge
//! keeps its traffic per app and service (requests by status class, latency, bytes, tcp connections), each
//! agent its machines' CPU and memory; the control plane asks them and merges (control/metrics.rs).
//! Buckets are by wall-clock epoch (unix seconds / BUCKET_S), so they line up across nodes.
// shortcut: in memory only (a restart loses the hour), and the nodes' clocks are trusted to agree (NTP).

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    task::{Context, Poll},
    time::Duration,
};

use hyper::body::{Body, Frame, SizeHint};
use serde::{Deserialize, Serialize};

use crate::util::now_ms;

/// A bucket's seconds, and how many are kept (an hour).
pub(crate) const BUCKET_S: i64 = 10;
pub(crate) const BUCKETS: i64 = 360;
/// The latency histogram: bin 0 is under 1 ms, bin b up to LAT_BASE^b ms; the last one has the rest
/// (from about 37 s). A percentile is its bin's upper bound: within 25%.
pub(crate) const LAT_BINS: usize = 48;
const LAT_BASE: f64 = 1.25;

/// The bucket now.
pub(crate) fn epoch_now() -> i64 {
    now_ms() / 1000 / BUCKET_S
}

/// The API's windows, as a number of buckets.
pub(crate) fn window(s: &str) -> Option<i64> {
    match s {
        "5m" => Some(300 / BUCKET_S),
        "1h" => Some(BUCKETS),
        _ => None,
    }
}

/// A node's answer for the last `n` buckets: `?app=<name>&n=<buckets>` (1..=BUCKETS).
pub(crate) fn query(q: Option<&str>) -> Result<(String, i64), String> {
    let mut app = None;
    let mut n = None;
    for kv in q.unwrap_or("").split('&') {
        match kv.split_once('=') {
            Some(("app", v)) if crate::control::model::app_name_ok(v) => app = Some(v.to_owned()),
            Some(("n", v)) => n = v.parse::<i64>().ok().filter(|n| (1..=BUCKETS).contains(n)),
            _ => {}
        }
    }
    Ok((app.ok_or("want ?app=<name>")?, n.ok_or("want &n=<buckets>, 1 to 360")?))
}

/// One bucket per epoch, the last BUCKETS of them; a slot is reset when a new epoch takes it.
pub(crate) struct Ring<T> {
    slots: Vec<Option<(i64, T)>>,
}

impl<T: Default> Default for Ring<T> {
    fn default() -> Self {
        Ring { slots: (0..BUCKETS).map(|_| None).collect() }
    }
}

impl<T: Default> Ring<T> {
    pub(crate) fn at(&mut self, epoch: i64) -> &mut T {
        let slot = &mut self.slots[epoch.rem_euclid(BUCKETS) as usize];
        if slot.as_ref().map(|(e, _)| *e) != Some(epoch) {
            *slot = Some((epoch, T::default()));
        }
        &mut slot.as_mut().expect("just set").1
    }

    /// The buckets with data among the last `n` up to `now`, oldest first.
    pub(crate) fn last(&self, now: i64, n: i64) -> Vec<(i64, &T)> {
        let mut v: Vec<(i64, &T)> = self.slots.iter().flatten().filter(|(e, _)| *e > now - n && *e <= now).map(|(e, t)| (*e, t)).collect();
        v.sort_by_key(|(e, _)| *e);
        v
    }

    /// Nothing in the last hour.
    fn idle(&self, now: i64) -> bool {
        self.slots.iter().flatten().all(|(e, _)| *e <= now - BUCKETS)
    }
}

/// An app service's traffic at one edge in one bucket; on the wire with its epoch.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Traffic {
    pub(crate) epoch: i64,
    /// Requests by status class: 2xx (1xx too), 3xx, 4xx, 5xx.
    pub(crate) status: [u64; 4],
    /// Of the 5xx, the edge's own: 502 (no instance answered) and 503 (every instance at its hard limit).
    pub(crate) edge_502: u64,
    pub(crate) edge_503: u64,
    /// Latency to the response's headers, by bin (LAT_BINS; trailing zeros left out on the wire).
    pub(crate) latency: Vec<u64>,
    pub(crate) bytes_in: u64,
    pub(crate) bytes_out: u64,
    pub(crate) tcp_opened: u64,
    /// tcp connections open: the most at once in the bucket (summed over edges when merged).
    pub(crate) tcp_open: u64,
}

impl Traffic {
    pub(crate) fn add(&mut self, o: &Traffic) {
        for (a, b) in self.status.iter_mut().zip(o.status) {
            *a += b;
        }
        self.edge_502 += o.edge_502;
        self.edge_503 += o.edge_503;
        if self.latency.len() < o.latency.len() {
            self.latency.resize(o.latency.len(), 0);
        }
        for (a, b) in self.latency.iter_mut().zip(&o.latency) {
            *a += b;
        }
        self.bytes_in += o.bytes_in;
        self.bytes_out += o.bytes_out;
        self.tcp_opened += o.tcp_opened;
        self.tcp_open += o.tcp_open;
    }

    pub(crate) fn requests(&self) -> u64 {
        self.status.iter().sum()
    }

    fn wire(&self, epoch: i64) -> Traffic {
        let mut t = self.clone();
        t.epoch = epoch;
        while t.latency.last() == Some(&0) {
            t.latency.pop();
        }
        t
    }
}

pub(crate) fn lat_bin(d: Duration) -> usize {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        return 0;
    }
    ((ms.ln() / LAT_BASE.ln()).floor() as usize + 1).min(LAT_BINS - 1)
}

/// The q-quantile (0..1) of a latency histogram, in ms (its bin's upper bound); None without requests.
pub(crate) fn percentile(latency: &[u64], q: f64) -> Option<f64> {
    let total: u64 = latency.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = ((q * total as f64).ceil() as u64).max(1);
    let mut seen = 0;
    for (b, n) in latency.iter().enumerate() {
        seen += n;
        if seen >= rank {
            return Some((LAT_BASE.powi(b as i32) * 10.0).round() / 10.0);
        }
    }
    None
}

/// One app service's buckets at an edge, and its tcp connections open now.
#[derive(Default)]
pub(crate) struct Series {
    ring: Mutex<Ring<Traffic>>,
    open: AtomicU64,
}

impl Series {
    fn with(&self, f: impl FnOnce(&mut Traffic)) {
        let mut ring = self.ring.lock().unwrap();
        let t = ring.at(epoch_now());
        if t.latency.len() < LAT_BINS {
            t.latency.resize(LAT_BINS, 0);
        }
        f(t)
    }

    /// A response's headers: its status, whether the edge made it, and the time since the request came.
    pub(crate) fn request(&self, status: u16, ours: bool, latency: Duration) {
        self.with(|t| {
            t.status[match status {
                0..=299 => 0,
                300..=399 => 1,
                400..=499 => 2,
                _ => 3,
            }] += 1;
            match (ours, status) {
                (true, 502) => t.edge_502 += 1,
                (true, 503) => t.edge_503 += 1,
                _ => {}
            }
            t.latency[lat_bin(latency)] += 1;
        })
    }

    /// Bytes in and out, counted in the bucket when their body (or connection) ends.
    pub(crate) fn bytes(&self, bytes_in: u64, bytes_out: u64) {
        self.with(|t| {
            t.bytes_in += bytes_in;
            t.bytes_out += bytes_out;
        })
    }

    /// A tcp connection: counted open until the guard drops.
    pub(crate) fn opened(self: &Arc<Self>) -> Open {
        let now = self.open.fetch_add(1, Ordering::AcqRel) + 1;
        self.with(|t| {
            t.tcp_opened += 1;
            t.tcp_open = t.tcp_open.max(now);
        });
        Open(self.clone())
    }

    /// The bucket now gets the connections open now (a long one spans buckets with no events).
    fn tick(&self) {
        let now = self.open.load(Ordering::Acquire);
        if now > 0 {
            self.with(|t| t.tcp_open = t.tcp_open.max(now));
        }
    }
}

pub(crate) struct Open(Arc<Series>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// An edge's series: (app, service) -> its buckets.
#[derive(Default)]
pub(crate) struct Edge {
    series: RwLock<HashMap<(String, String), Arc<Series>>>,
}

impl Edge {
    pub(crate) fn series(&self, app: &str, service: &str) -> Arc<Series> {
        let key = (app.to_owned(), service.to_owned());
        if let Some(s) = self.series.read().unwrap().get(&key) {
            return s.clone();
        }
        self.series.write().unwrap().entry(key).or_default().clone()
    }

    /// `GET /metrics`: the app's services, each its buckets with data among the last `n`.
    pub(crate) fn json(&self, app: &str, n: i64) -> serde_json::Value {
        let now = epoch_now();
        let series = self.series.read().unwrap();
        let services: serde_json::Map<String, serde_json::Value> = series
            .iter()
            .filter(|((a, _), _)| a == app)
            .map(|((_, svc), s)| {
                let ring = s.ring.lock().unwrap();
                let buckets: Vec<Traffic> = ring.last(now, n).into_iter().map(|(e, t)| t.wire(e)).collect();
                (svc.clone(), serde_json::to_value(buckets).expect("serialisable"))
            })
            .collect();
        serde_json::json!({ "services": services })
    }

    /// Every BUCKET_S / 2: open connections into the bucket now; series idle for an hour dropped.
    pub(crate) async fn tick(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(BUCKET_S as u64 / 2)).await;
            let now = epoch_now();
            for s in self.series.read().unwrap().values() {
                s.tick();
            }
            self.series.write().unwrap().retain(|_, s| s.open.load(Ordering::Acquire) > 0 || !s.ring.lock().unwrap().idle(now));
        }
    }
}

/// Which way a counted body's bytes go.
#[derive(Clone, Copy)]
pub(crate) enum Dir {
    In,
    Out,
}

/// A body whose bytes go into its series when it ends (or is dropped).
pub(crate) struct Counted<B> {
    pub(crate) body: B,
    series: Arc<Series>,
    dir: Dir,
    n: u64,
}

impl<B> Counted<B> {
    pub(crate) fn new(body: B, series: Arc<Series>, dir: Dir) -> Self {
        Counted { body, series, dir, n: 0 }
    }
}

impl<B> Drop for Counted<B> {
    fn drop(&mut self) {
        match self.dir {
            Dir::In => self.series.bytes(self.n, 0),
            Dir::Out => self.series.bytes(0, self.n),
        }
    }
}

impl<B: Body<Data = hyper::body::Bytes> + Unpin> Body for Counted<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let r = Pin::new(&mut self.body).poll_frame(cx);
        if let Poll::Ready(Some(Ok(f))) = &r {
            if let Some(d) = f.data_ref() {
                self.n += d.len() as u64;
            }
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// A machine's usage at its agent in one bucket; on the wire with its epoch.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Usage {
    pub(crate) epoch: i64,
    /// CPU in cores (1.0: one core busy), summed over `samples` (each over the interval before it).
    pub(crate) cpu: f64,
    /// Memory in bytes: the most of any sample (None: systemd doesn't account the unit's memory).
    pub(crate) memory: Option<u64>,
    pub(crate) samples: u32,
}

/// One machine at its agent: its app, its buckets, and the last CPU sample (total ns, and when in ms).
type Machine = (String, Ring<Usage>, Option<(u64, i64)>);

/// An agent's machines by id.
#[derive(Default)]
pub(crate) struct Machines {
    by_id: Mutex<HashMap<String, Machine>>,
}

impl Machines {
    /// One sample of a machine's unit: CPUUsageNSec and MemoryCurrent (None: not running, not known).
    pub(crate) fn sample(&self, id: &str, app: &str, cpu_ns: Option<u64>, memory: Option<u64>, at_ms: i64) {
        let mut by_id = self.by_id.lock().unwrap();
        let (_, ring, last) = by_id.entry(id.to_owned()).or_insert_with(|| (app.to_owned(), Ring::default(), None));
        let Some(cpu_ns) = cpu_ns else {
            *last = None;
            return;
        };
        // The first sample (or after a restart, when the counter starts over) only sets the baseline.
        let prev = last.replace((cpu_ns, at_ms));
        let Some((prev_ns, prev_ms)) = prev.filter(|(ns, ms)| *ns <= cpu_ns && *ms < at_ms) else { return };
        let u = ring.at(at_ms / 1000 / BUCKET_S);
        u.cpu += (cpu_ns - prev_ns) as f64 / ((at_ms - prev_ms) as f64 * 1e6);
        u.memory = memory.map(|m| m.max(u.memory.unwrap_or(0))).or(u.memory);
        u.samples += 1;
    }

    /// `GET /v1/metrics`: the app's machines here, each its buckets with data among the last `n`.
    pub(crate) fn json(&self, app: &str, n: i64) -> serde_json::Value {
        let now = epoch_now();
        let mut by_id = self.by_id.lock().unwrap();
        by_id.retain(|_, (_, ring, last)| last.is_some() || !ring.idle(now));
        let machines: serde_json::Map<String, serde_json::Value> = by_id
            .iter()
            .filter(|(_, (a, _, _))| a == app)
            .map(|(id, (_, ring, _))| {
                let buckets: Vec<Usage> = ring.last(now, n).into_iter().map(|(e, u)| Usage { epoch: e, ..u.clone() }).collect();
                (id.clone(), serde_json::to_value(buckets).expect("serialisable"))
            })
            .collect();
        serde_json::json!({ "machines": machines })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_hour() {
        let mut r: Ring<u64> = Ring::default();
        *r.at(100) += 1;
        *r.at(101) += 2;
        // The same slot an hour later: reset, not added to.
        *r.at(100 + BUCKETS) += 5;
        assert_eq!(r.last(100 + BUCKETS, BUCKETS).into_iter().map(|(e, v)| (e, *v)).collect::<Vec<_>>(), [(101, 2), (460, 5)]);
        assert_eq!(r.last(101, 1).len(), 1);
        assert!(!r.idle(101 + BUCKETS) && r.idle(460 + BUCKETS));
    }

    #[test]
    fn latency_bins_and_percentiles() {
        let ms = |m: f64| Duration::from_secs_f64(m / 1000.0);
        assert_eq!((lat_bin(ms(0.5)), lat_bin(ms(1.0)), lat_bin(ms(1.3)), lat_bin(ms(1e9))), (0, 1, 2, LAT_BINS - 1));
        let mut h = vec![0; LAT_BINS];
        for m in [2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 500.0] {
            h[lat_bin(ms(m))] += 1;
        }
        let (p50, p99) = (percentile(&h, 0.5).unwrap(), percentile(&h, 0.99).unwrap());
        assert!((2.0..2.5).contains(&p50) && (500.0..625.0).contains(&p99), "{p50} {p99}");
        assert_eq!(percentile(&[0; 4], 0.5), None);
    }

    #[test]
    fn traffic_counts_and_merges() {
        let s = Arc::new(Series::default());
        s.request(200, false, Duration::from_millis(3));
        s.request(503, true, Duration::from_millis(1000));
        s.request(502, false, Duration::from_millis(3));
        s.request(404, true, Duration::from_millis(1));
        s.bytes(10, 20);
        let open = s.opened();
        let _two = s.opened();
        drop(open);
        let edge = Edge::default();
        edge.series.write().unwrap().insert(("a".into(), "http".into()), s);
        let v = edge.json("a", 1);
        let t: Vec<Traffic> = serde_json::from_value(v["services"]["http"].clone()).unwrap();
        assert_eq!(t.len(), 1);
        let t = &t[0];
        // The app's own 502 is a 5xx, not the edge's.
        assert_eq!((t.status, t.edge_502, t.edge_503, t.requests()), ([1, 0, 1, 2], 0, 1, 4));
        assert_eq!((t.bytes_in, t.bytes_out, t.tcp_opened, t.tcp_open), (10, 20, 2, 2));
        assert!(t.latency.len() < LAT_BINS && t.latency.iter().sum::<u64>() == 4, "trailing zeros dropped");
        let mut sum = t.clone();
        sum.add(t);
        assert_eq!((sum.status, sum.edge_503, sum.latency.iter().sum::<u64>()), ([2, 0, 2, 4], 2, 8));
        assert_eq!(edge.json("b", 1)["services"], serde_json::json!({}));
    }

    #[test]
    fn usage_from_cpu_counter_deltas() {
        let m = Machines::default();
        let t0 = epoch_now() * BUCKET_S * 1000;
        m.sample("m1", "a", Some(1_000_000_000), Some(100), t0);
        // Half a core over the next 2 s, then the unit restarted (its counter starts over: baseline only).
        m.sample("m1", "a", Some(2_000_000_000), Some(300), t0 + 2000);
        m.sample("m1", "a", Some(5_000_000), Some(50), t0 + 4000);
        m.sample("m2", "b", None, None, t0);
        let v = m.json("a", 1);
        let u: Vec<Usage> = serde_json::from_value(v["machines"]["m1"].clone()).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!((u[0].cpu, u[0].memory, u[0].samples), (0.5, Some(300), 1));
        // Never sampled running, nothing kept: dropped.
        assert_eq!(m.json("b", 1)["machines"], serde_json::json!({}));
        assert!(query(Some("app=web&n=30")).is_ok() && query(Some("app=web&n=361")).is_err() && query(Some("n=3")).is_err() && query(None).is_err());
    }
}
