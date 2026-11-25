use std::collections::{BTreeMap, HashMap, HashSet};

use super::{
    guest::{guest_config, DEFAULT_PATH},
    plan::{plan, Action, Actual},
    report::changed_images,
    spec::{
        cmd_ok, env_ok, size_ok, spec_ok, Spec, DEFAULT_CPUS, DEFAULT_MEMORY, MAX_ARGS, MAX_ARG_BYTES, MAX_CPUS, MAX_ENV, MAX_ENV_BYTES, MAX_MEMORY, MIN_MEMORY,
    },
};
use crate::{images::ImageConfig, states::Desired};

const IMAGE: &str = "traefik/whoami@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn spec(id: &str) -> Spec {
    Spec {
        id: id.into(),
        app: "web".into(),
        image: IMAGE.into(),
        cmd: vec![],
        env: BTreeMap::new(),
        port: 80,
        cpus: 1,
        memory: 128,
        services: crate::services::http_on(80),
    }
}

#[test]
fn specs_are_checked() {
    assert_eq!(spec_ok(&spec("ok")), Ok(()));
    let bad = [
        Spec { id: "bad/../x".into(), ..spec("x") },
        Spec { image: "traefik/whoami:latest".into(), ..spec("x") },
        Spec { port: 0, ..spec("x") },
        Spec { cpus: 0, ..spec("x") },
        Spec { memory: 99_999_999, ..spec("x") },
        Spec { cmd: vec!["a\0b".into()], ..spec("x") },
        Spec { services: vec![], ..spec("x") },
    ];
    for s in bad {
        assert!(spec_ok(&s).is_err(), "{s:?}");
    }
}

#[test]
fn the_store_is_idempotent_and_destroyed_is_final() {
    use super::desired::{Refused, Store};
    let dir = std::env::temp_dir().join(format!("flyyy-desired-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let dir = dir.to_string_lossy().into_owned();
    let store = Store::open(&dir, Default::default()).unwrap();
    assert_eq!(store.put("a", spec("a")), Ok(true));
    assert_eq!(store.put("a", spec("a")), Ok(false));
    assert!(matches!(store.put("a", Spec { port: 81, ..spec("a") }), Err(Refused::Conflict(_))));
    assert!(matches!(store.put("b", spec("a")), Err(Refused::Bad(_))));
    assert!(matches!(store.put("c", Spec { port: 0, ..spec("c") }), Err(Refused::Bad(_))));
    assert_eq!(store.all()["a"].desired, Desired::Stopped);
    assert_eq!(store.set("a", Desired::Started), Ok(true));
    assert_eq!(store.set("a", Desired::Started), Ok(false));
    assert_eq!(store.set("nope", Desired::Started), Err(Refused::Missing));
    assert_eq!(store.set("a", Desired::Destroyed), Ok(true));
    assert_eq!(store.set("a", Desired::Destroyed), Ok(false));
    assert!(matches!(store.set("a", Desired::Started), Err(Refused::Conflict(_))));
    // What it was told survives a restart.
    let again = Store::open(&dir, Default::default()).unwrap();
    assert_eq!(again.all(), store.all());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn guest_config_follows_docker_semantics() {
    let spec = |cmd: &[&str], env: &[(&str, &str)]| Spec {
        id: "pg-1a2b3c".into(),
        app: "pg".into(),
        image: String::new(),
        cmd: cmd.iter().map(|s| s.to_string()).collect(),
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        port: 5432,
        cpus: 1,
        memory: 128,
        services: vec![],
    };
    let image = ImageConfig {
        env: vec!["PATH=/usr/lib/postgresql/bin:/usr/bin".into(), "PGDATA=/var/lib/postgresql/data".into(), "LANG=en_US.utf8".into()],
        entrypoint: vec!["docker-entrypoint.sh".into()],
        cmd: vec!["postgres".into()],
        workdir: String::new(),
        user: "postgres".into(),
        volumes: vec![],
    };
    let c = guest_config(&spec(&[], &[("POSTGRES_PASSWORD", "a b; --c"), ("LANG", "C")]), &image).unwrap();
    assert_eq!(c.argv, ["docker-entrypoint.sh", "postgres"]);
    assert_eq!(
        c.env,
        ["PATH=/usr/lib/postgresql/bin:/usr/bin", "PGDATA=/var/lib/postgresql/data", "LANG=C", "POSTGRES_PASSWORD=a b; --c", "HOSTNAME=pg-1a2b3c"]
    );
    assert_eq!((c.workdir.as_str(), c.user.as_str(), c.hostname.as_str()), ("/", "postgres", "pg-1a2b3c"));
    // cmd replaces Cmd, Entrypoint stays; no PATH in the image → Docker's default.
    let image = ImageConfig { env: vec![], cmd: vec!["ignored".into()], workdir: "/app".into(), ..image };
    let c = guest_config(&spec(&["-c", "echo a b -- c"], &[]), &image).unwrap();
    assert_eq!(c.argv, ["docker-entrypoint.sh", "-c", "echo a b -- c"]);
    assert_eq!(c.env, [format!("PATH={DEFAULT_PATH}"), "HOSTNAME=pg-1a2b3c".into()]);
    assert_eq!(c.workdir, "/app");
    assert!(guest_config(&spec(&[], &[]), &ImageConfig::default()).is_err());
    let with = |v: &[&str]| ImageConfig { volumes: v.iter().map(|s| s.to_string()).collect(), ..image.clone() };
    assert_eq!(guest_config(&spec(&[], &[]), &with(&["/app/data", "/var/lib/x/"])).unwrap().volumes, ["/app/data", "/var/lib/x/"]);
    for bad in ["data", "/a/../etc", "/a/./b", "/a\0b"] {
        assert!(guest_config(&spec(&[], &[]), &with(&[bad])).is_err(), "{bad}");
    }
}

#[test]
fn image_reports_only_changes_and_gone_images_as_zero() {
    let m = |kv: &[(&str, i64)]| kv.iter().map(|(k, v)| (k.to_string(), *v)).collect::<HashMap<_, _>>();
    let last = m(&[("sha256:a", 100), ("sha256:b", 200), ("sha256:c", 0)]);
    // a unchanged, b grew, c still gone (0, not written again), d new.
    let now = m(&[("sha256:a", 100), ("sha256:b", 250), ("sha256:d", 7)]);
    assert_eq!(changed_images(&last, &now), [("sha256:b".to_string(), 250), ("sha256:d".to_string(), 7)]);
    // a's file deleted: reported as 0 once; c already 0: not again.
    let now = m(&[("sha256:b", 200)]);
    assert_eq!(changed_images(&last, &now), [("sha256:a".to_string(), 0)]);
    assert!(changed_images(&last, &m(&[("sha256:a", 100), ("sha256:b", 200)])).is_empty());
}

#[test]
fn cmd_and_env_limits() {
    assert!(cmd_ok(&vec!["a b \" -- '".to_string(); MAX_ARGS]).is_ok());
    assert!(cmd_ok(&vec!["a".to_string(); MAX_ARGS + 1]).is_err());
    assert!(cmd_ok(&["x".repeat(MAX_ARG_BYTES + 1)]).is_err());
    assert!(cmd_ok(&["a\0b".to_string()]).is_err());
    let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
    assert!(env_ok(&env(&[("A_1", "x y=z"), ("_B", "")])).is_ok());
    for bad in [env(&[("1A", "x")]), env(&[("A-B", "x")]), env(&[("", "x")]), env(&[("A", "a\0")]), env(&[("A", &"x".repeat(MAX_ENV_BYTES))])] {
        assert!(env_ok(&bad).is_err(), "{bad:?}");
    }
    assert!(env_ok(&(0..=MAX_ENV).map(|i| (format!("V{i}"), String::new())).collect()).is_err());
}

#[test]
fn size_limits() {
    assert!(size_ok(DEFAULT_CPUS, DEFAULT_MEMORY).is_ok() && size_ok(MAX_CPUS, MAX_MEMORY).is_ok());
    for (cpus, memory) in [(0, 128), (MAX_CPUS + 1, 128), (1, MIN_MEMORY - 1), (1, MAX_MEMORY + 1)] {
        assert!(size_ok(cpus, memory).is_err(), "{cpus} {memory}");
    }
}

#[test]
fn plan_creates_starts_stops_destroys_orphans_and_respects_backoff() {
    let want = |rows: &[(&str, &str)]| -> BTreeMap<String, Desired> { rows.iter().map(|(id, d)| (id.to_string(), d.parse().unwrap())).collect() };
    let desired = want(&[
        ("new", "started"),
        ("cold", "started"),
        ("hot", "stopped"),
        ("gone", "destroyed"),
        ("done", "destroyed"),
        ("parked", "stopped"),
        ("broken", "started"),
    ]);
    let a = |local, running| Actual { local, running };
    let actual = BTreeMap::from([
        ("cold".to_string(), a(true, false)),
        ("hot".to_string(), a(true, true)),
        ("gone".to_string(), a(true, false)),
        ("orphan".to_string(), a(false, true)),
    ]);
    let backoff = HashSet::from(["broken".to_string()]);
    let got = plan(&desired, &actual, &backoff, &HashSet::new());
    let got: Vec<(&str, Action)> = got.iter().map(|(id, a)| (id.as_str(), *a)).collect();
    use Action::*;
    assert_eq!(got, [("cold", Start), ("gone", Destroy), ("hot", Stop), ("new", Create), ("new", Start), ("parked", Create), ("orphan", Destroy)]);

    // In backoff, a failed start is not retried, but a stop/destroy still goes through.
    let desired = want(&[("broken", "destroyed")]);
    let actual = BTreeMap::from([("broken".to_string(), a(true, false))]);
    assert_eq!(plan(&desired, &actual, &backoff, &HashSet::new()), [("broken".to_string(), Destroy)]);

    // Waiting on an image: no Create (so no Start) until it's ready. One already created starts anyway.
    let desired = want(&[("new", "started"), ("cold", "started")]);
    let actual = BTreeMap::from([("cold".to_string(), a(true, false))]);
    let pulling = HashSet::from(["new".to_string(), "cold".to_string()]);
    assert_eq!(plan(&desired, &actual, &HashSet::new(), &pulling), [("cold".to_string(), Start)]);
}

#[test]
fn a_failed_command_keeps_its_exit_code() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let e = rt.block_on(super::sys::run("sh", &["-c", "echo nope >&2; exit 5"])).unwrap_err();
    assert_eq!((e.code, e.to_string().as_str()), (Some(super::sys::NOT_LOADED), "sh -c echo nope >&2; exit 5: nope"));
    assert_eq!(rt.block_on(super::sys::run("/nonexistent/cmd", &[])).unwrap_err().code, None);
}
