use std::collections::BTreeMap;

use super::{
    auth::{new_token, token_hash, Caller},
    model::{app_image_bytes, app_name_ok, live_workers, worker_image_bytes, App, Image, Machine, Worker},
    placement::{decide, Op},
    *,
};
use crate::{agent::spec::image_ok, services::http_on};

fn app(count: u32, state: &str) -> App {
    App {
        name: "blue".into(),
        image: String::new(),
        port: 80,
        cmd: vec![],
        env: BTreeMap::new(),
        count,
        state: state.parse().unwrap(),
        cpus: 1,
        memory: 128,
        services: http_on(80),
        org: String::new(),
    }
}

fn m(id: &str, worker: &str, app: &str, managed: bool, desired: &str, state: &str) -> Machine {
    Machine {
        id: id.into(),
        worker: worker.into(),
        app: app.into(),
        managed,
        desired: desired.parse().unwrap(),
        state: state.parse().ok(),
        addr: String::new(),
        error: String::new(),
        cpus: 1,
        memory: 128,
    }
}

fn workers() -> Vec<String> {
    ["w1", "w2", "w3"].map(String::from).to_vec()
}

fn creates(ops: &[Op]) -> Vec<&str> {
    ops.iter().filter_map(|o| if let Op::Create { worker } = o { Some(worker.as_str()) } else { None }).collect()
}

#[test]
fn decide_spreads_scales_and_destroys() {
    // Other load: w1 has 2 machines (one unmanaged), w2 1, w3 0. Destroyed machines don't count.
    let other = vec![
        m("whoami-1", "w1", "whoami", true, "started", "started"),
        m("m-1", "w1", "echo", false, "started", "started"),
        m("whoami-2", "w2", "whoami", true, "started", "started"),
        m("whoami-3", "w3", "whoami", true, "destroyed", "destroyed"),
    ];
    // Scale up from 0: one per worker first (least loaded overall first), then round again.
    assert_eq!(creates(&decide(&app(4, "active"), &other, &workers())), ["w3", "w2", "w1", "w3"]);

    // Scale down 5 → 2: a not-yet-started machine goes first, then the worker with most of this app's.
    let mut ms = other.clone();
    ms.extend([
        m("blue-a", "w1", "blue", true, "started", "started"),
        m("blue-b", "w2", "blue", true, "started", "started"),
        m("blue-c", "w3", "blue", true, "started", "started"),
        m("blue-d", "w3", "blue", true, "started", "started"),
        m("blue-e", "w2", "blue", true, "started", "starting"),
    ]);
    let ops = decide(&app(2, "active"), &ms, &workers());
    assert_eq!(ops[0], Op::Destroy { id: "blue-e".into() });
    assert_eq!(ops.len(), 3);
    let left: Vec<_> = ms.iter().filter(|m| m.app == "blue" && !ops.contains(&Op::Destroy { id: m.id.clone() })).map(|m| m.worker.as_str()).collect();
    assert!(left[0] != left[1], "survivors should be on different workers: {left:?}");

    // Exactly at count: nothing. Destroyed app: every live managed machine of it, nothing else.
    assert_eq!(decide(&app(5, "active"), &ms, &workers()), []);
    let ops = decide(&app(5, "destroyed"), &ms, &workers());
    assert_eq!(ops.len(), 5);
    assert!(ops.iter().all(|o| matches!(o, Op::Destroy { id } if id.starts_with("blue-"))));
}

#[test]
fn image_bytes_per_app_and_per_worker() {
    let img = |worker: &str, digest: &str, bytes: u64| Image { worker: worker.into(), digest: digest.into(), bytes };
    let images = [img("worker1", "sha256:aa", 100), img("worker2", "sha256:aa", 104), img("worker1", "sha256:bb", 50), img("worker3", "sha256:aa", 0)];
    let a = |image: &str| App { image: image.into(), ..app(1, "active") };
    assert_eq!(app_image_bytes(&a("r/x@sha256:aa"), &images), Some(104));
    assert_eq!(app_image_bytes(&a("r/x@sha256:cc"), &images), None);
    assert_eq!((worker_image_bytes("worker1", &images), worker_image_bytes("worker3", &images)), (150, 0));
}

#[test]
fn only_valid_workers_with_a_fresh_heartbeat_are_live() {
    let w = |name: &str, backhaul: &str, heartbeat_at: i64| Worker {
        name: name.into(),
        backhaul: backhaul.into(),
        agent_api: String::new(),
        started_at: 0,
        heartbeat_at,
        arch: String::new(),
        cpus: 0,
        memory: 0,
        machines: 0,
        allocated_cpus: 0,
        allocated_memory: 0,
    };
    let now = 100_000;
    let rows = [
        w("worker1", "10.250.1.1:4280", now - 1_000),
        w("worker2", "10.250.1.2:4280", now - ALIVE_MS - 1),
        w("worker3", "nope", now),
        w("Evil!", "10.250.1.9:4280", now),
        w("nuc", "10.250.1.6:4280", now),
        w("worker4", "10.250.1.4:4280", now - ALIVE_MS + 1),
        w("worker5", "10.99.5.2:4280", now),
    ];
    let live = live_workers(&rows, now, false);
    assert_eq!(live, ["worker1", "nuc", "worker4"]);
    // A dead worker gets nothing new, but its machines still count.
    let ms = vec![m("blue-a", "worker2", "blue", true, "started", "started")];
    let ops = decide(&app(3, "active"), &ms, &live);
    // Ties go by name (any name: nothing reads a number out of it).
    assert_eq!(creates(&ops), ["nuc", "worker1"]);
}

#[test]
fn validation() {
    assert!(app_name_ok("blue") && app_name_ok("a-1") && !app_name_ok("1a") && !app_name_ok("a-") && !app_name_ok("Blue") && !app_name_ok(&"a".repeat(21)));
    assert!(image_ok(&format!("traefik/whoami@sha256:{}", "0".repeat(64))));
    assert!(image_ok(&format!("10.99.0.3:5000/traefik/whoami@sha256:{}", "0".repeat(64))));
    for bad in ["10.99.0.3:/x", "10.99.0.3:abc/x", "h:123456/x", "a:1:2/x", "10.99.0.3:5000/", "Host:1/x", "x:5000"] {
        assert!(!image_ok(&format!("{bad}@sha256:{}", "0".repeat(64))), "{bad}");
    }
    assert!(!image_ok("traefik/whoami:latest") && !image_ok(&format!("../x@sha256:{}", "0".repeat(64))) && !image_ok(&format!("x@sha256:{}", "0".repeat(63))));
    let t = OperatorToken::new("abc").unwrap();
    assert!(t.matches("abc") && !t.matches("abd") && !t.matches("ab") && !t.matches(""));
}

#[test]
fn tokens_are_hashed_and_users_see_their_org_only() {
    // FIPS 180-2's "abc"; and no token hashes to "" (a destroyed org's users).
    assert_eq!(token_hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert!(!token_hash("").is_empty());
    let t = new_token().unwrap();
    assert!(t.starts_with("flyyy_") && t.len() == 6 + 64 && t != new_token().unwrap());
    let (red, blue) = (App { org: "red".into(), ..app(1, "active") }, App { org: "blue".into(), ..app(1, "active") });
    let alice = Caller::User { name: "alice".into(), org: "red".into() };
    assert!(alice.sees(&red) && !alice.sees(&blue) && !alice.sees(&app(1, "active")));
    assert!(Caller::Operator.sees(&red) && Caller::Operator.sees(&blue) && Caller::Operator.sees(&app(1, "active")));
}
