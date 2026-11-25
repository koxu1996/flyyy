//! `/v1/apps`: list, view, deploy, scale, destroy, and each app's load and metrics. A user sees and
//! changes its org's apps only: another org's app is as good as missing (404, not 403); its name is
//! all a user may learn of it, and only when trying to take that name (names are global: each is a vhost).

use std::{collections::BTreeMap, net::Ipv4Addr};

use hyper::{body::Incoming, Method, Request, Response, StatusCode};
use serde::Deserialize;

use super::{
    api::body,
    auth::Caller,
    db::Records,
    error, json,
    model::{app_image_bytes, services_view, App, Check, Image, Machine},
    placement::converged,
    routes::Route,
    Control, MAX_COUNT,
};
use crate::{
    agent::spec::{cmd_ok, env_ok, image_ok, size_ok, DEFAULT_CPUS, DEFAULT_MEMORY},
    api_error::ApiError,
    ingress::{self, Lease, Prefix},
    random,
    services::{http_on, services_ok, Service},
    states::{Fact, Lifecycle},
    util::now_ms,
    Body,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Deploy {
    pub(crate) image: String,
    /// The shortcut for one http service named http on this port; or `services`, not both.
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) services: Option<Vec<Service>>,
    pub(crate) count: u32,
    /// argv replacing the image's Cmd: exact strings, so spaces, quotes and `--` are fine.
    #[serde(default)]
    pub(crate) cmd: Vec<String>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    /// vCPUs and memory (MiB) of each machine.
    #[serde(default = "default_cpus")]
    pub(crate) cpus: u32,
    #[serde(default = "default_memory")]
    pub(crate) memory: u32,
    /// The operator's only: the org to deploy it into (a user's apps are always its own org's).
    #[serde(default)]
    pub(crate) org: Option<String>,
}

pub(crate) fn default_cpus() -> u32 {
    DEFAULT_CPUS
}

pub(crate) fn default_memory() -> u32 {
    DEFAULT_MEMORY
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scale {
    pub(crate) count: u32,
}

pub(crate) fn count_ok(count: u32) -> Result<(), ApiError> {
    if count > MAX_COUNT {
        return Err(error(StatusCode::BAD_REQUEST, format!("count {count}: want 0-{MAX_COUNT}")));
    }
    Ok(())
}

/// What a deploy's rules need besides the request and the store.
pub(crate) struct DeployCtx<'a> {
    pub(crate) caller: &'a Caller,
    /// The managed machines of an app of that name (they hold another org's name until they're gone).
    pub(crate) machines: &'a [Machine],
    /// The control plane's ingress: pools for dedicated addresses (none: no tcp services), shared IPs.
    pub(crate) pools: &'a [Prefix],
    pub(crate) shared: &'a [Ipv4Addr],
    pub(crate) now: i64,
    /// A random number, for the address pick.
    pub(crate) random: u64,
}

/// A deploy's rules against the store, in the order they're checked: the request's validity, its org,
/// the name (another org's while active or until its machines are gone), tcp needing a pool, the same
/// spec as an active app's (a new spec needs a destroy first), then the write: the app, and a dedicated
/// address (kept from a previous deploy, else a free one) in the same write, so a refused one leaks none.
/// Returns the address it allocated, if a new one.
// shortcut: no rolling or blue/green deploys; and concurrent deploys of one app race (last write wins),
// fine for one operator.
pub(crate) fn deploy_rules(recs: &mut Records, name: &str, d: Deploy, ctx: &DeployCtx) -> Result<Option<Ipv4Addr>, ApiError> {
    let bad = |e: String| error(StatusCode::BAD_REQUEST, e);
    if !image_ok(&d.image) {
        return Err(bad(format!("bad image {:?}: want <repo>@sha256:<64 hex> (no tags)", d.image)));
    }
    let services = match (d.port, d.services) {
        (Some(0), None) => return Err(bad("port 0: want 1-65535".into())),
        (Some(p), None) => http_on(p),
        (None, Some(s)) => s,
        _ => return Err(bad("want one of port (one http service) or services".into())),
    };
    count_ok(d.count)?;
    cmd_ok(&d.cmd).and_then(|_| env_ok(&d.env)).and_then(|_| size_ok(d.cpus, d.memory)).and_then(|_| services_ok(&services)).map_err(bad)?;
    let (existing, theirs) = match recs.apps.get(name) {
        Some(a) if ctx.caller.sees(a) => (Some(a), None),
        other => (None, other),
    };
    let active = existing.filter(|a| a.state == Lifecycle::Active);
    let org = match (ctx.caller, d.org) {
        (Caller::User { org, .. }, None) => org.clone(),
        (Caller::User { org, .. }, Some(o)) if &o == org => o,
        (Caller::User { org, .. }, Some(o)) => return Err(bad(format!("org {o:?}: your token deploys into {org} only"))),
        (Caller::Operator, Some(o)) => o,
        (Caller::Operator, None) => active.map_or(String::new(), |a| a.org.clone()),
    };
    if !org.is_empty() && !recs.orgs.get(&org).is_some_and(|o| o.state == Lifecycle::Active) {
        return Err(error(StatusCode::NOT_FOUND, format!("no org {org:?}")));
    }
    if let Some(t) = theirs {
        let left: Vec<&Machine> = ctx.machines.iter().filter(|m| m.managed && m.app == name).collect();
        if t.state == Lifecycle::Active || !converged(t, &left, &[]) {
            return Err(error(StatusCode::CONFLICT, format!("app name {name} is taken (app names are global: each is a vhost)")));
        }
    }
    // tcp is published on the app's own address: without a pool there is none.
    if ctx.pools.is_empty() && services.iter().any(|s| s.published_port().is_some()) {
        return Err(bad("tcp services need a dedicated IP, and this control plane has no --ip-pool".into()));
    }
    if let Some(a) = active {
        if (&a.image, &a.services, &a.cmd, &a.env, a.cpus, a.memory, &a.org) != (&d.image, &services, &d.cmd, &d.env, d.cpus, d.memory, &org) {
            return Err(error(
                StatusCode::CONFLICT,
                format!("app {name} is deployed with a different image/services/cmd/env/cpus/memory/org; destroy it first"),
            ));
        }
    }
    let app = App {
        name: name.to_owned(),
        image: d.image,
        port: services[0].port,
        cmd: d.cmd,
        env: d.env,
        count: d.count,
        state: Lifecycle::Active,
        cpus: d.cpus,
        memory: d.memory,
        services,
        org,
    };
    let mut new_ip = None;
    if !ctx.pools.is_empty() && !recs.leases.values().any(|l| l.app == name && l.released_at == 0) {
        let leases: Vec<Lease> = recs.leases.values().cloned().collect();
        let ip = ingress::free(ctx.pools, ctx.shared, &leases, ctx.now, |n| (ctx.random % n as u64) as usize)
            .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "ip pools exhausted (released addresses come back after 24 h)"))?;
        recs.leases.insert(ip, Lease { ip, app: name.to_owned(), allocated_at: ctx.now, released_at: 0 });
        new_ip = Some(ip);
    }
    recs.apps.insert(name.to_owned(), app);
    Ok(new_ip)
}

impl Control {
    /// The caller's apps as the API lists them (no machines).
    pub(crate) async fn views(&self, caller: &Caller) -> Result<Vec<serde_json::Value>, String> {
        let (apps, machines) = (self.apps(None), self.machines().await?);
        let (images, checks, leases) = (self.images().await?, self.checks().await?, self.leases());
        Ok(apps.iter().filter(|a| caller.sees(a)).map(|a| self.view(a, &machines, &images, &checks, &leases, false)).collect())
    }

    /// An app's addresses for the API: its dedicated one (if any), then the shared ones.
    pub(crate) fn addresses(&self, app: &App, leases: &[Lease]) -> Vec<serde_json::Value> {
        let mine = leases.iter().filter(|l| l.app == app.name && l.released_at == 0 && app.state == Lifecycle::Active);
        mine.map(|l| serde_json::json!({ "ip": l.ip.to_string(), "type": "dedicated" }))
            .chain(self.shared.iter().map(|ip| serde_json::json!({ "ip": ip.to_string(), "type": "shared" })))
            .collect()
    }

    /// `checks`: this app's service_status rows, (machine, service, state).
    pub(crate) fn view(&self, app: &App, machines: &[Machine], images: &[Image], checks: &[Check], leases: &[Lease], with_machines: bool) -> serde_json::Value {
        let mine: Vec<&Machine> = machines.iter().filter(|m| m.managed && m.app == app.name).collect();
        let started = mine.iter().filter(|m| m.live() && m.state == Some(Fact::Started)).count();
        let mut v = serde_json::json!({
            // Env names only: values may be secrets.
            "name": app.name, "image": app.image, "port": app.port, "cmd": app.cmd, "env": app.env.keys().collect::<Vec<_>>(), "count": app.count,
            "cpus": app.cpus, "memory": app.memory, "image_bytes": app_image_bytes(app, images), "org": app.org,
            "services": services_view(app, &mine, checks),
            "ips": self.addresses(app, leases),
            "state": app.state, "started": started, "converged": converged(app, &mine, checks),
        });
        if with_machines {
            // Live machines plus destroyed ones that aren't gone yet.
            let shown: Vec<_> = mine.into_iter().filter(|m| m.live() || m.state != Some(Fact::Destroyed)).collect();
            v["machines"] = serde_json::to_value(shown).expect("serialisable");
        }
        v
    }

    /// The caller's app of that name: another org's is a 404, as if it weren't there.
    fn visible(&self, caller: &Caller, name: &str) -> Result<App, ApiError> {
        self.apps(Some(name)).into_iter().next().filter(|a| caller.sees(a)).ok_or_else(|| error(StatusCode::NOT_FOUND, format!("no app {name}")))
    }

    /// The app routes (routes.rs).
    pub(crate) async fn apps_route(&self, route: Route, req: Request<Incoming>, caller: Caller) -> Result<Response<Body>, ApiError> {
        let unavailable = |e: String| error(StatusCode::SERVICE_UNAVAILABLE, e);
        match route {
            Route::AppsList => Ok(json(StatusCode::OK, self.views(&caller).await.map_err(unavailable)?.into())),
            Route::AppGet { app } => {
                let app = self.visible(&caller, &app)?;
                let (machines, images) = (self.machines().await.map_err(unavailable)?, self.images().await.map_err(unavailable)?);
                let checks = self.checks().await.map_err(unavailable)?;
                Ok(json(StatusCode::OK, self.view(&app, &machines, &images, &checks, &self.leases(), true)))
            }
            Route::AppDeploy { app: name } => {
                let d: Deploy = body(req).await?;
                let count = d.count;
                // Another org's app of that name: its machines, for whether it's gone (read before the
                // write: Corrosion's facts, not the store).
                let theirs = self.apps(Some(&name)).into_iter().any(|a| !caller.sees(&a));
                let machines = if theirs { self.machines().await.map_err(unavailable)? } else { vec![] };
                let r = random::u64().map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
                let ctx = DeployCtx { caller: &caller, machines: &machines, pools: &self.pools, shared: &self.shared, now: now_ms(), random: r };
                let ip = self.db.write(|recs| deploy_rules(recs, &name, d, &ctx)).map_err(unavailable)??;
                if let Some(ip) = ip {
                    tracing::info!(%ip, app = %name, "ingress address allocated");
                }
                let image = self.apps(Some(&name)).into_iter().next().map(|a| a.image).unwrap_or_default();
                self.events.add(&name, "", format!("deployed {image}, {count} VM{}", if count == 1 { "" } else { "s" }), now_ms());
                self.wake.notify_one();
                Ok(json(StatusCode::ACCEPTED, serde_json::json!({ "name": name, "count": count })))
            }
            Route::AppScale { app: name } => {
                let s: Scale = body(req).await?;
                count_ok(s.count)?;
                let missing = || error(StatusCode::NOT_FOUND, format!("no active app {name}"));
                self.db
                    .write(|recs| match recs.apps.get_mut(&name).filter(|a| a.state == Lifecycle::Active && caller.sees(a)) {
                        Some(a) => {
                            a.count = s.count;
                            Ok(())
                        }
                        None => Err(missing()),
                    })
                    .map_err(unavailable)??;
                self.events.add(&name, "", format!("scaled to {} VM{}", s.count, if s.count == 1 { "" } else { "s" }), now_ms());
                self.wake.notify_one();
                Ok(json(StatusCode::ACCEPTED, serde_json::json!({ "name": name, "count": s.count })))
            }
            Route::AppDestroy { app: name } => {
                self.visible(&caller, &name)?;
                // Its address is released with it (and quarantined: ingress::QUARANTINE_MS).
                let now = now_ms();
                self.db
                    .write(|recs| {
                        if let Some(a) = recs.apps.get_mut(&name) {
                            a.state = Lifecycle::Destroyed;
                        }
                        for l in recs.leases.values_mut().filter(|l| l.app == name && l.released_at == 0) {
                            l.released_at = now;
                        }
                        Ok::<_, ApiError>(())
                    })
                    .map_err(unavailable)??;
                self.events.add(&name, "", "destroyed", now_ms());
                self.wake.notify_one();
                Ok(json(StatusCode::ACCEPTED, serde_json::json!({ "name": name, "state": "destroyed" })))
            }
            // Its load as the edges see it (load.rs).
            Route::AppLoad { app } => {
                let app = self.visible(&caller, &app)?;
                Ok(json(StatusCode::OK, self.app_load(&app).await.map_err(unavailable)?))
            }
            // Its metrics (metrics.rs).
            Route::AppMetrics { app, buckets } => {
                let app = self.visible(&caller, &app)?;
                Ok(json(StatusCode::OK, self.app_metrics(&app, buckets).await.map_err(unavailable)?))
            }
            // Its recent events, newest first (events.rs).
            Route::AppEvents { app } => {
                let app = self.visible(&caller, &app)?;
                Ok(json(StatusCode::OK, serde_json::json!({ "app": app.name, "events": self.events.list(&app.name) })))
            }
            // A machine's console: the last lines of its unit's journal, from its worker's agent.
            Route::AppMachineLogs { app, machine } => {
                let app = self.visible(&caller, &app)?;
                let m = self
                    .machines()
                    .await
                    .map_err(unavailable)?
                    .into_iter()
                    .find(|m| m.id == machine && m.app == app.name)
                    .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("no machine {machine} of {}", app.name)))?;
                let rows = self.workers().await.map_err(unavailable)?;
                let w = rows.iter().find(|w| w.name == m.worker).ok_or_else(|| unavailable(format!("{}: no workers row", m.worker)))?;
                let addr = self.bound(w, &w.agent_api).map_err(unavailable)?;
                let (code, bytes) = self.agents.call(addr, Method::GET, &format!("/v1/machines/{}/logs", m.id), None).await.map_err(unavailable)?;
                let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| unavailable(format!("{}: bad answer: {e}", m.worker)))?;
                if !code.is_success() {
                    return Err(error(StatusCode::BAD_GATEWAY, format!("{}: {}: {}", m.worker, code.as_u16(), v["error"].as_str().unwrap_or_default())));
                }
                Ok(json(StatusCode::OK, serde_json::json!({ "machine": m.id, "worker": m.worker, "lines": v["lines"] })))
            }
            r => unreachable!("not an app route: {r:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::db::OrgRec;

    const IMG: &str = "r/web@sha256:0000000000000000000000000000000000000000000000000000000000000000";

    fn d(json: serde_json::Value) -> Deploy {
        serde_json::from_value(json).unwrap()
    }

    fn web() -> Deploy {
        d(serde_json::json!({ "image": IMG, "port": 80, "count": 2 }))
    }

    fn recs() -> Records {
        let mut r = Records::default();
        for o in ["red", "blue"] {
            r.orgs.insert(o.into(), OrgRec { state: Lifecycle::Active, created_at: 0 });
        }
        r
    }

    fn alice() -> Caller {
        Caller::User { name: "alice".into(), org: "red".into() }
    }

    fn bob() -> Caller {
        Caller::User { name: "bob".into(), org: "blue".into() }
    }

    fn run(r: &mut Records, name: &str, dep: Deploy, caller: &Caller, pools: &[Prefix]) -> Result<Option<Ipv4Addr>, u16> {
        let ctx = DeployCtx { caller, machines: &[], pools, shared: &[], now: 1000, random: 7 };
        deploy_rules(r, name, dep, &ctx).map_err(|e| e.status.as_u16())
    }

    #[test]
    fn deploys_are_validated_before_anything_is_written() {
        let mut r = recs();
        for (body, why) in [
            (serde_json::json!({ "image": "r/web:latest", "port": 80, "count": 1 }), "a tag"),
            (serde_json::json!({ "image": IMG, "port": 0, "count": 1 }), "port 0"),
            (serde_json::json!({ "image": IMG, "count": 1 }), "no port or services"),
            (serde_json::json!({ "image": IMG, "port": 80, "count": 1000 }), "count"),
            (serde_json::json!({ "image": IMG, "port": 80, "count": 1, "cpus": 0 }), "cpus"),
            (serde_json::json!({ "image": IMG, "port": 80, "count": 1, "env": { "1A": "x" } }), "env"),
            (serde_json::json!({ "image": IMG, "services": [{ "name": "db", "protocol": "tcp", "port": 5432 }], "count": 1 }), "tcp without a pool"),
        ] {
            assert_eq!(run(&mut r, "web", d(body), &Caller::Operator, &[]), Err(400), "{why}");
        }
        assert!(r.apps.is_empty() && r.leases.is_empty());
    }

    #[test]
    fn orgs_names_and_specs() {
        let mut r = recs();
        // A user deploys into its own org, and only there; the operator into any active org.
        assert_eq!(run(&mut r, "web", web(), &alice(), &[]), Ok(None));
        assert_eq!(r.apps["web"].org, "red");
        let into_blue = d(serde_json::json!({ "image": IMG, "port": 80, "count": 1, "org": "blue" }));
        assert_eq!(run(&mut r, "x", into_blue, &alice(), &[]), Err(400));
        let into_green = d(serde_json::json!({ "image": IMG, "port": 80, "count": 1, "org": "green" }));
        assert_eq!(run(&mut r, "x", into_green, &Caller::Operator, &[]), Err(404));
        // Another org's active app holds its name.
        assert_eq!(run(&mut r, "web", web(), &bob(), &[]), Err(409));
        // The same spec again is fine (a scale through deploy); a different one needs a destroy first.
        let three = d(serde_json::json!({ "image": IMG, "port": 80, "count": 3 }));
        assert_eq!(run(&mut r, "web", three, &alice(), &[]), Ok(None));
        assert_eq!(r.apps["web"].count, 3);
        assert_eq!(run(&mut r, "web", d(serde_json::json!({ "image": IMG, "port": 8080, "count": 3 })), &alice(), &[]), Err(409));
        // The operator redeploying keeps the app's org.
        assert_eq!(run(&mut r, "web", web(), &Caller::Operator, &[]), Ok(None));
        assert_eq!(r.apps["web"].org, "red");
        // Destroyed with no machines left, the name is free for another org.
        r.apps.get_mut("web").unwrap().state = Lifecycle::Destroyed;
        assert_eq!(run(&mut r, "web", web(), &bob(), &[]), Ok(None));
        assert_eq!(r.apps["web"].org, "blue");
    }

    #[test]
    fn one_dedicated_address_per_app_from_the_pool() {
        let mut r = recs();
        let pools = [Prefix::parse("10.200.1.0/30").unwrap()];
        let ip = run(&mut r, "web", web(), &alice(), &pools).unwrap().expect("an address");
        assert!(pools[0].contains(ip));
        assert_eq!(r.leases[&ip].app, "web");
        // Kept on a redeploy: no second one.
        assert_eq!(run(&mut r, "web", web(), &alice(), &pools), Ok(None));
        assert_eq!(r.leases.len(), 1);
    }
}
