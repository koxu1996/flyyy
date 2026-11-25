//! The edge's load balancing: requests in flight per instance (connections, for tcp), and which instance
//! a request goes to. Power of two choices: of two random instances in the best group, the less loaded.
//! The groups, best first: under the hard limit, not ejected, on a fresh worker, under the soft limit. A
//! request holds a slot of its instance until its response body ends; an instance at its hard limit gets
//! none, and when every instance of a route is there the request waits (QUEUE) for one to free, then 503.
// shortcut: the limits and counts are per edge: two edges can each send an instance up to its hard limit.

use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasher, RandomState},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use hyper::body::{Body, Frame, SizeHint};
use tokio::sync::Notify;

use crate::table::Upstream;

/// How long a request waits for a slot when every instance is at its hard limit.
pub(crate) const QUEUE: Duration = Duration::from_secs(1);

/// One instance's counters on this edge.
#[derive(Debug, Default)]
pub(crate) struct Load {
    pub(crate) in_flight: AtomicU64,
    /// Requests (connections, for tcp) it was given.
    pub(crate) total: AtomicU64,
    /// Requests answered 503 (connections closed) because every instance was at its hard limit, counted
    /// on the instance the request would have gone to.
    pub(crate) rejected: AtomicU64,
    /// Tunnels to it that failed, and requests through it that got no answer.
    pub(crate) failures: AtomicU64,
    /// The most it ever had in flight at once.
    pub(crate) peak: AtomicU64,
}

impl Load {
    /// One more in flight, unless that would pass `hard` (0: no limit).
    fn try_acquire(&self, hard: u32) -> bool {
        let mut cur = self.in_flight.load(Ordering::Relaxed);
        loop {
            if hard > 0 && cur >= hard as u64 {
                return false;
            }
            match self.in_flight.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    self.total.fetch_add(1, Ordering::Relaxed);
                    self.peak.fetch_max(cur + 1, Ordering::Relaxed);
                    return true;
                }
                Err(now) => cur = now,
            }
        }
    }

    pub(crate) fn json(&self, up: Option<&Upstream>) -> serde_json::Value {
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        serde_json::json!({
            "in_flight": n(&self.in_flight), "total": n(&self.total), "rejected": n(&self.rejected), "failures": n(&self.failures), "peak": n(&self.peak),
            "soft_limit": up.map_or(0, |u| u.soft_limit), "hard_limit": up.map_or(0, |u| u.hard_limit),
        })
    }
}

/// Every instance's counters, and a wake-up for requests waiting on a slot.
#[derive(Default)]
pub(crate) struct Loads {
    by_id: std::sync::RwLock<HashMap<String, Arc<Load>>>,
    freed: Arc<Notify>,
}

/// A request's (or connection's) place at an instance: given back on drop.
pub(crate) struct Slot {
    pub(crate) load: Arc<Load>,
    freed: Arc<Notify>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.load.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.freed.notify_waiters();
    }
}

impl Loads {
    pub(crate) fn get(&self, id: &str) -> Arc<Load> {
        if let Some(l) = self.by_id.read().unwrap().get(id) {
            return l.clone();
        }
        self.by_id.write().unwrap().entry(id.to_owned()).or_default().clone()
    }

    pub(crate) fn in_flight(&self, id: &str) -> u64 {
        self.by_id.read().unwrap().get(id).map_or(0, |l| l.in_flight.load(Ordering::Relaxed))
    }

    /// A slot at this instance, or None while it's at its hard limit.
    pub(crate) fn acquire(&self, up: &Upstream) -> Option<Slot> {
        let load = self.get(&up.id);
        load.try_acquire(up.hard_limit).then(|| Slot { load, freed: self.freed.clone() })
    }

    /// Wakes when any slot is given back.
    pub(crate) fn freed(&self) -> Arc<Notify> {
        self.freed.clone()
    }

    /// Every instance's counters, with the limits of those still routed (`ups`).
    pub(crate) fn json(&self, ups: &HashMap<String, Upstream>) -> serde_json::Value {
        let by_id = self.by_id.read().unwrap();
        let mut ids: Vec<&String> = by_id.keys().collect();
        ids.sort();
        ids.into_iter().map(|id| (id.clone(), by_id[id].json(ups.get(id)))).collect::<serde_json::Map<_, _>>().into()
    }
}

/// A random number, for the two choices.
pub(crate) fn random() -> u64 {
    RandomState::new().hash_one(Instant::now())
}

/// An instance's place in the order: its group (at its hard limit, ejected, on a stale worker, over its soft
/// limit; all false is best) and then its requests in flight.
type Rank = ((bool, bool, bool, bool), u64);

/// The order to try a route's instances in: first the power-of-two-choices pick of the best group, then
/// every other instance, best group first and least loaded first within one (the fallbacks if its tunnel
/// fails). `in_flight` is each one's load now; `r1`, `r2` random.
pub(crate) fn order<'a>(
    ups: &'a [Upstream],
    in_flight: impl Fn(&str) -> u64,
    stale: &HashSet<String>,
    ejected: &HashMap<String, Instant>,
    now: Instant,
    (r1, r2): (u64, u64),
) -> Vec<&'a Upstream> {
    let key = |u: &Upstream| {
        let n = in_flight(&u.id);
        let at_hard = u.hard_limit > 0 && n >= u.hard_limit as u64;
        let over_soft = u.soft_limit > 0 && n >= u.soft_limit as u64;
        ((at_hard, ejected.get(&u.id).is_some_and(|t| *t > now), stale.contains(&u.worker), over_soft), n)
    };
    let mut all: Vec<(&Upstream, Rank)> = ups.iter().map(|u| (u, key(u))).collect();
    all.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.id.cmp(&b.0.id)));
    let Some(best) = all.first().map(|(_, (group, _))| *group) else { return vec![] };
    let group: Vec<usize> = (0..all.len()).filter(|&i| all[i].1 .0 == best).collect();
    let chosen = match group.len() {
        1 => group[0],
        n => {
            let a = group[(r1 % n as u64) as usize];
            let mut b = group[(r2 % (n as u64 - 1)) as usize];
            if b == a {
                b = group[n - 1];
            }
            if all[b].1 .1 < all[a].1 .1 {
                b
            } else {
                a
            }
        }
    };
    let first = all.remove(chosen).0;
    std::iter::once(first).chain(all.into_iter().map(|(u, _)| u)).collect()
}

/// A response body that holds its request's slot until it ends (or is dropped).
pub(crate) struct Held<B> {
    pub(crate) body: B,
    pub(crate) slot: Option<Slot>,
}

impl<B: Body + Unpin> Body for Held<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let r = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(r, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            self.slot = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Target;

    fn up(id: &str, worker: &str, soft_limit: u32, hard_limit: u32) -> Upstream {
        Upstream { id: id.into(), addr: Target::Tcp("10.250.1.1:4280".parse().unwrap()), app: "a".into(), worker: worker.into(), soft_limit, hard_limit }
    }

    fn ids(v: Vec<&Upstream>) -> Vec<&str> {
        v.into_iter().map(|u| u.id.as_str()).collect()
    }

    #[test]
    fn two_choices_take_the_less_loaded() {
        let ups = [up("a", "w1", 0, 0), up("b", "w2", 0, 0), up("c", "w3", 0, 0)];
        let load = HashMap::from([("a", 5), ("b", 1), ("c", 3)]);
        let f = |id: &str| load[id];
        let (none, no) = (HashSet::new(), HashMap::new());
        let now = Instant::now();
        // The choices are a and c (indexes in least-loaded order: b, c, a): c is less loaded than a.
        assert_eq!(ids(order(&ups, f, &none, &no, now, (2, 1))), ["c", "b", "a"]);
        // b against anything: b.
        for r in 0..9 {
            let first = order(&ups, f, &none, &no, now, (0, r))[0].id.clone();
            assert_eq!(first, "b");
        }
        // Over many random pairs the least loaded wins most, the most loaded never (it's never the less).
        let mut wins: HashMap<String, u32> = HashMap::new();
        for _ in 0..300 {
            *wins.entry(order(&ups, f, &none, &no, now, (random(), random()))[0].id.clone()).or_default() += 1;
        }
        assert!(!wins.contains_key("a") && wins["b"] > wins.get("c").copied().unwrap_or(0), "{wins:?}");
    }

    #[test]
    fn limits_then_fresh_workers_first_and_ejected_last() {
        let now = Instant::now();
        let none = HashSet::new();
        // a is at its hard limit, b over its soft one, c under both: c first, then b, a last.
        let ups = [up("a", "w1", 0, 2), up("b", "w2", 1, 4), up("c", "w3", 4, 8)];
        let load = HashMap::from([("a", 2), ("b", 3), ("c", 3)]);
        assert_eq!(ids(order(&ups, |id| load[id], &none, &HashMap::new(), now, (0, 0))), ["c", "b", "a"]);
        // Ejected after a stale worker, after a fresh one; an ejection that's over doesn't count.
        let ups = [up("a", "w1", 0, 0), up("b", "w2", 0, 0), up("c", "w3", 0, 0), up("d", "w1", 0, 0)];
        let stale = HashSet::from(["w2".to_string()]);
        let ejected = HashMap::from([("c".to_string(), now + Duration::from_secs(10)), ("d".to_string(), now - Duration::from_secs(1))]);
        let got = ids(order(&ups, |_| 0, &stale, &ejected, now, (0, 0)));
        assert_eq!(&got[2..], ["b", "c"]);
        assert_eq!(got[..2].iter().copied().collect::<HashSet<_>>(), HashSet::from(["a", "d"]));
    }

    #[test]
    fn a_slot_respects_the_hard_limit_and_is_given_back() {
        let loads = Loads::default();
        let u = up("a", "w1", 0, 1);
        let s = loads.acquire(&u).expect("one slot");
        assert!(loads.acquire(&u).is_none(), "at its hard limit");
        drop(s);
        let _s = loads.acquire(&u).expect("given back");
        let l = loads.get("a");
        assert_eq!((l.in_flight.load(Ordering::Relaxed), l.total.load(Ordering::Relaxed), l.peak.load(Ordering::Relaxed)), (1, 2, 1));
    }
}
