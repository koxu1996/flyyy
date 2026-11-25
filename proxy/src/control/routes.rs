//! The API's routes: each request parsed once into a `Route` (its method, path and parameters), and who
//! may call each one, declared in one place (`Route::access`). An unknown path is a 404, a known one
//! with the wrong method a 405, a bad parameter a 400.
//!   POST   /v1/mesh/join                    a token's address (mesh listener): join
//!   GET    /v1/mesh/join                    a token's address: has it been approved?
//!   GET    /v1/mesh/peers                   a node's address: its WireGuard peers
//!   POST   /v1/mesh/cert                    a node's address: its certificate, for a CSR
//!   POST   /v1/mesh/local                   loopback, with a join token: an edge's certificate on this host
//!   GET    /v1/mesh, POST /v1/mesh          the operator: list the nodes, create the mesh
//!   POST   /v1/mesh/tokens                  the operator: a join token
//!   POST   /v1/mesh/nodes/<node>/approve    the operator
//!   GET    /v1/workers                      the operator
//!   GET    /v1/whoami                       any token: the operator, or a user and its org
//!   GET    /v1/orgs, POST /v1/orgs, DELETE /v1/orgs/<org>     the operator
//!   GET    /v1/users, POST /v1/users        the operator
//!   GET    /v1/apps                         a user (its org's apps) or the operator (all)
//!   GET, PUT, PATCH, DELETE /v1/apps/<app>  a user (its org's) or the operator: view, deploy, scale, destroy
//!   GET    /v1/apps/<app>/load              likewise
//!   GET    /v1/apps/<app>/metrics?window=5m|1h   likewise
//!   GET    /v1/apps/<app>/events            likewise: its recent events, newest first
//!   GET    /v1/apps/<app>/machines/<id>/logs   likewise: the last lines of a machine's console

use std::net::Ipv4Addr;

use hyper::{Method, StatusCode};

use super::{auth::Caller, error, model::app_name_ok};
use crate::{api_error::ApiError, metrics, wg};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Route {
    MeshJoin,
    MeshJoinStatus,
    MeshPeers,
    MeshCert,
    MeshLocal,
    MeshList,
    MeshInit,
    MeshToken,
    MeshApprove {
        node: String,
    },
    Workers,
    Whoami,
    OrgsList,
    OrgCreate,
    OrgDestroy {
        org: String,
    },
    UsersList,
    UserCreate,
    AppsList,
    AppGet {
        app: String,
    },
    AppDeploy {
        app: String,
    },
    AppScale {
        app: String,
    },
    AppDestroy {
        app: String,
    },
    AppLoad {
        app: String,
    },
    /// `buckets`: the window, in metrics buckets.
    AppMetrics {
        app: String,
        buckets: i64,
    },
    AppEvents {
        app: String,
    },
    AppMachineLogs {
        app: String,
        machine: String,
    },
}

/// Who may call a route.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Access {
    /// A join token's address (10.250.255.x), on the mesh listener: WireGuard ties it to that token.
    Bootstrap,
    /// An approved node's mesh address, on the mesh listener.
    MeshNode,
    /// Loopback on the main listener: this host (the request's join token is what authorizes it).
    Local,
    /// The operator's token.
    Operator,
    /// Any valid token: a user (its org's apps only, which the handlers enforce) or the operator.
    User,
}

fn not_found() -> ApiError {
    error(StatusCode::NOT_FOUND, "no such route")
}

fn only(allowed: &str) -> ApiError {
    error(StatusCode::METHOD_NOT_ALLOWED, format!("{allowed} only"))
}

/// A machine id of this app, as the reconciler makes them: `<app>-<hex>`.
fn machine_id_ok(app: &str, id: &str) -> bool {
    id.strip_prefix(app)
        .and_then(|r| r.strip_prefix('-'))
        .is_some_and(|h| (1..=16).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

fn app(name: &str) -> Result<String, ApiError> {
    match app_name_ok(name) {
        true => Ok(name.to_owned()),
        false => Err(error(StatusCode::BAD_REQUEST, format!("bad app name {name:?}: want [a-z][a-z0-9-]{{0,19}}"))),
    }
}

impl Route {
    pub(crate) fn parse(method: &Method, path: &str, query: Option<&str>) -> Result<Route, ApiError> {
        let m = method.clone();
        let route = match path {
            "/v1/mesh/join" => match m {
                Method::POST => Route::MeshJoin,
                Method::GET => Route::MeshJoinStatus,
                _ => return Err(only("GET or POST")),
            },
            "/v1/mesh/peers" => match m {
                Method::GET => Route::MeshPeers,
                _ => return Err(only("GET")),
            },
            "/v1/mesh/cert" => match m {
                Method::POST => Route::MeshCert,
                _ => return Err(only("POST")),
            },
            "/v1/mesh/local" => match m {
                Method::POST => Route::MeshLocal,
                _ => return Err(only("POST")),
            },
            "/v1/mesh" => match m {
                Method::GET => Route::MeshList,
                Method::POST => Route::MeshInit,
                _ => return Err(only("GET or POST")),
            },
            "/v1/mesh/tokens" => match m {
                Method::POST => Route::MeshToken,
                _ => return Err(only("POST")),
            },
            "/v1/workers" => match m {
                Method::GET => Route::Workers,
                _ => return Err(only("GET")),
            },
            "/v1/whoami" => match m {
                Method::GET => Route::Whoami,
                _ => return Err(only("GET")),
            },
            "/v1/orgs" => match m {
                Method::GET => Route::OrgsList,
                Method::POST => Route::OrgCreate,
                _ => return Err(only("GET or POST")),
            },
            "/v1/users" => match m {
                Method::GET => Route::UsersList,
                Method::POST => Route::UserCreate,
                _ => return Err(only("GET or POST")),
            },
            "/v1/apps" | "/v1/apps/" => match m {
                Method::GET => Route::AppsList,
                _ => return Err(only("GET")),
            },
            p => {
                if let Some(node) = p.strip_prefix("/v1/mesh/nodes/").and_then(|r| r.strip_suffix("/approve")).filter(|n| !n.is_empty() && !n.contains('/')) {
                    return match m {
                        Method::POST => Ok(Route::MeshApprove { node: node.to_owned() }),
                        _ => Err(only("POST")),
                    };
                }
                if let Some(org) = p.strip_prefix("/v1/orgs/") {
                    // A bad org name is a route that doesn't exist (as before: no org can have it).
                    return match (app_name_ok(org), m) {
                        (false, _) => Err(not_found()),
                        (true, Method::DELETE) => Ok(Route::OrgDestroy { org: org.to_owned() }),
                        (true, _) => Err(only("DELETE")),
                    };
                }
                let Some(rest) = p.strip_prefix("/v1/apps/") else { return Err(not_found()) };
                return match rest.split_once('/') {
                    None => {
                        let app = app(rest)?;
                        match m {
                            Method::GET => Ok(Route::AppGet { app }),
                            Method::PUT => Ok(Route::AppDeploy { app }),
                            Method::PATCH => Ok(Route::AppScale { app }),
                            Method::DELETE => Ok(Route::AppDestroy { app }),
                            _ => Err(error(StatusCode::METHOD_NOT_ALLOWED, "GET, PUT, PATCH or DELETE")),
                        }
                    }
                    Some((name, "load")) => {
                        let app = app(name)?;
                        match m {
                            Method::GET => Ok(Route::AppLoad { app }),
                            _ => Err(only("GET")),
                        }
                    }
                    Some((name, "metrics")) => {
                        let app = app(name)?;
                        if m != Method::GET {
                            return Err(only("GET"));
                        }
                        let window = query.unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("window=")).unwrap_or("5m");
                        let buckets = metrics::window(window).ok_or_else(|| error(StatusCode::BAD_REQUEST, format!("window {window:?}: want 5m or 1h")))?;
                        Ok(Route::AppMetrics { app, buckets })
                    }
                    Some((name, "events")) => {
                        let app = app(name)?;
                        match m {
                            Method::GET => Ok(Route::AppEvents { app }),
                            _ => Err(only("GET")),
                        }
                    }
                    Some((name, sub)) => {
                        let app = app(name)?;
                        // machines/<id>/logs: an id as the reconciler makes them, <app>-<hex>.
                        let Some(id) = sub.strip_prefix("machines/").and_then(|r| r.strip_suffix("/logs")) else { return Err(not_found()) };
                        if !machine_id_ok(&app, id) {
                            return Err(error(StatusCode::BAD_REQUEST, format!("bad machine id {id:?}: want {app}-<hex>")));
                        }
                        match m {
                            Method::GET => Ok(Route::AppMachineLogs { app, machine: id.to_owned() }),
                            _ => Err(only("GET")),
                        }
                    }
                };
            }
        };
        Ok(route)
    }

    /// Who may call it: the one table.
    pub(crate) fn access(&self) -> Access {
        match self {
            Route::MeshJoin | Route::MeshJoinStatus => Access::Bootstrap,
            Route::MeshPeers | Route::MeshCert => Access::MeshNode,
            Route::MeshLocal => Access::Local,
            Route::MeshList
            | Route::MeshInit
            | Route::MeshToken
            | Route::MeshApprove { .. }
            | Route::Workers
            | Route::OrgsList
            | Route::OrgCreate
            | Route::OrgDestroy { .. }
            | Route::UsersList
            | Route::UserCreate => Access::Operator,
            Route::Whoami
            | Route::AppsList
            | Route::AppGet { .. }
            | Route::AppDeploy { .. }
            | Route::AppScale { .. }
            | Route::AppDestroy { .. }
            | Route::AppLoad { .. }
            | Route::AppMetrics { .. }
            | Route::AppEvents { .. }
            | Route::AppMachineLogs { .. } => Access::User,
        }
    }
}

/// Who a request is from, once allowed: a mesh address (for the mesh routes), or a token's caller.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Who {
    Source(Ipv4Addr),
    /// A process on this host (loopback, not the mesh listener).
    Local,
    Caller(Caller),
}

/// Whether `access` lets this caller in. `from`: the source address, only on the mesh listener (there
/// WireGuard ties it to one key; anywhere else it means nothing). `loopback`: from this host, on the main
/// listener. `caller`: the bearer token's, if valid. The mesh routes exist only on the mesh listener (404
/// elsewhere), for the right kind of address (403); the local one only on loopback (404 elsewhere).
pub(crate) fn authorize(access: Access, from: Option<Ipv4Addr>, loopback: bool, caller: Option<Caller>) -> Result<Who, ApiError> {
    let source = |ok: fn(Ipv4Addr) -> bool| match from {
        None => Err(not_found()),
        Some(ip) if ok(ip) => Ok(Who::Source(ip)),
        Some(ip) => Err(error(StatusCode::FORBIDDEN, format!("not for {ip}"))),
    };
    match (access, caller) {
        (Access::Bootstrap, _) => source(wg::is_bootstrap),
        (Access::MeshNode, _) => source(wg::is_node),
        (Access::Local, _) if loopback && from.is_none() => Ok(Who::Local),
        (Access::Local, _) => Err(not_found()),
        (_, None) => Err(error(StatusCode::UNAUTHORIZED, "missing or wrong bearer token")),
        (Access::Operator, Some(Caller::Operator)) => Ok(Who::Caller(Caller::Operator)),
        (Access::Operator, Some(_)) => Err(error(StatusCode::FORBIDDEN, "the operator's token only")),
        (Access::User, Some(c)) => Ok(Who::Caller(c)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::model::App;

    fn parse(m: &str, path: &str) -> Result<Route, u16> {
        let (path, query) = path.split_once('?').map_or((path, None), |(p, q)| (p, Some(q)));
        Route::parse(&m.parse().unwrap(), path, query).map_err(|e| e.status.as_u16())
    }

    fn a(s: &str) -> String {
        s.into()
    }

    #[test]
    fn routes_parse_once() {
        for (m, p, want) in [
            ("POST", "/v1/mesh/join", Route::MeshJoin),
            ("GET", "/v1/mesh/join", Route::MeshJoinStatus),
            ("GET", "/v1/mesh/peers", Route::MeshPeers),
            ("POST", "/v1/mesh/cert", Route::MeshCert),
            ("POST", "/v1/mesh/local", Route::MeshLocal),
            ("GET", "/v1/mesh", Route::MeshList),
            ("POST", "/v1/mesh", Route::MeshInit),
            ("POST", "/v1/mesh/tokens", Route::MeshToken),
            ("POST", "/v1/mesh/nodes/worker4/approve", Route::MeshApprove { node: a("worker4") }),
            ("GET", "/v1/workers", Route::Workers),
            ("GET", "/v1/whoami", Route::Whoami),
            ("GET", "/v1/orgs", Route::OrgsList),
            ("POST", "/v1/orgs", Route::OrgCreate),
            ("DELETE", "/v1/orgs/red", Route::OrgDestroy { org: a("red") }),
            ("GET", "/v1/users", Route::UsersList),
            ("POST", "/v1/users", Route::UserCreate),
            ("GET", "/v1/apps", Route::AppsList),
            ("GET", "/v1/apps/", Route::AppsList),
            ("GET", "/v1/apps/web", Route::AppGet { app: a("web") }),
            ("PUT", "/v1/apps/web", Route::AppDeploy { app: a("web") }),
            ("PATCH", "/v1/apps/web", Route::AppScale { app: a("web") }),
            ("DELETE", "/v1/apps/web", Route::AppDestroy { app: a("web") }),
            ("GET", "/v1/apps/web/load", Route::AppLoad { app: a("web") }),
            ("GET", "/v1/apps/web/metrics", Route::AppMetrics { app: a("web"), buckets: 30 }),
            ("GET", "/v1/apps/web/metrics?window=1h", Route::AppMetrics { app: a("web"), buckets: 360 }),
            ("GET", "/v1/apps/web/metrics?x=1&window=5m", Route::AppMetrics { app: a("web"), buckets: 30 }),
            ("GET", "/v1/apps/web/events", Route::AppEvents { app: a("web") }),
            ("GET", "/v1/apps/web/machines/web-0a1b2c/logs", Route::AppMachineLogs { app: a("web"), machine: a("web-0a1b2c") }),
        ] {
            assert_eq!(parse(m, p), Ok(want), "{m} {p}");
        }
        for (m, p, code) in [
            ("GET", "/", 404),
            ("GET", "/v1", 404),
            ("GET", "/v1/nope", 404),
            ("GET", "/v1/apps/web/nope", 404),
            ("GET", "/v1/apps/web/load/x", 404),
            ("GET", "/v1/apps/web/machines/web-0a1b2c", 404),
            ("GET", "/v1/apps/web/machines/web-0a1b2c/logs/x", 404),
            ("GET", "/v1/apps/web/machines/blue-0a1b2c/logs", 400),
            ("GET", "/v1/apps/web/machines/web-../logs", 400),
            ("GET", "/v1/apps/web/machines/web-0A1B/logs", 400),
            ("POST", "/v1/apps/web/events", 405),
            ("DELETE", "/v1/apps/web/machines/web-0a1b2c/logs", 405),
            ("POST", "/v1/mesh/nodes//approve", 404),
            ("POST", "/v1/mesh/nodes/a/b/approve", 404),
            ("DELETE", "/v1/orgs/Red", 404),
            ("GET", "/v1/workersx", 404),
            ("PUT", "/v1/apps/Blue", 400),
            ("GET", "/v1/apps/Blue/load", 400),
            ("GET", "/v1/apps/x/metrics?window=2m", 400),
            ("POST", "/v1/apps", 405),
            ("POST", "/v1/apps/web", 405),
            ("POST", "/v1/apps/web/metrics", 405),
            ("DELETE", "/v1/apps/web/load", 405),
            ("PUT", "/v1/mesh/join", 405),
            ("POST", "/v1/mesh/peers", 405),
            ("GET", "/v1/mesh/cert", 405),
            ("GET", "/v1/mesh/local", 405),
            ("DELETE", "/v1/mesh", 405),
            ("GET", "/v1/mesh/tokens", 405),
            ("GET", "/v1/mesh/nodes/n/approve", 405),
            ("POST", "/v1/workers", 405),
            ("POST", "/v1/whoami", 405),
            ("DELETE", "/v1/orgs", 405),
            ("GET", "/v1/orgs/red", 405),
            ("DELETE", "/v1/users", 405),
        ] {
            assert_eq!(parse(m, p), Err(code), "{m} {p}");
        }
    }

    /// Every route against the operator, a user, a user of another org, no token, and mesh sources.
    #[test]
    fn who_may_call_each_route() {
        let routes = [
            Route::MeshJoin,
            Route::MeshJoinStatus,
            Route::MeshPeers,
            Route::MeshCert,
            Route::MeshLocal,
            Route::MeshList,
            Route::MeshInit,
            Route::MeshToken,
            Route::MeshApprove { node: a("n") },
            Route::Workers,
            Route::Whoami,
            Route::OrgsList,
            Route::OrgCreate,
            Route::OrgDestroy { org: a("red") },
            Route::UsersList,
            Route::UserCreate,
            Route::AppsList,
            Route::AppGet { app: a("web") },
            Route::AppDeploy { app: a("web") },
            Route::AppScale { app: a("web") },
            Route::AppDestroy { app: a("web") },
            Route::AppLoad { app: a("web") },
            Route::AppMetrics { app: a("web"), buckets: 30 },
            Route::AppEvents { app: a("web") },
            Route::AppMachineLogs { app: a("web"), machine: a("web-0a1b2c") },
        ];
        let alice = Caller::User { name: a("alice"), org: a("red") };
        let bob = Caller::User { name: a("bob"), org: a("blue") };
        let token_ip: Ipv4Addr = "10.250.255.3".parse().unwrap();
        let node_ip: Ipv4Addr = "10.250.1.2".parse().unwrap();
        let code =
            |r: &Route, from: Option<Ipv4Addr>, c: Option<Caller>| authorize(r.access(), from, false, c).map(|_| 200).unwrap_or_else(|e| e.status.as_u16());
        // From this host's loopback, on the main listener.
        let local = |r: &Route, c: Option<Caller>| authorize(r.access(), None, true, c).map(|_| 200).unwrap_or_else(|e| e.status.as_u16());
        // red's app: alice's (the route lets her in, and she sees it); bob passes the route but doesn't
        // see it, which every app handler answers as a 404 (Caller::sees, apps.rs).
        let web = App {
            name: a("web"),
            image: a("r/web@sha256:00"),
            port: 80,
            cmd: vec![],
            env: Default::default(),
            count: 1,
            state: crate::states::Lifecycle::Active,
            cpus: 1,
            memory: 128,
            services: crate::services::http_on(80),
            org: a("red"),
        };
        for r in &routes {
            let (op, user, other, none) =
                (code(r, None, Some(Caller::Operator)), code(r, None, Some(alice.clone())), code(r, None, Some(bob.clone())), code(r, None, None));
            let (bootstrap, node) = (code(r, Some(token_ip), None), code(r, Some(node_ip), None));
            match r.access() {
                Access::Bootstrap => assert_eq!((op, user, none, bootstrap, node), (404, 404, 404, 200, 403), "{r:?}"),
                Access::MeshNode => assert_eq!((op, user, none, bootstrap, node), (404, 404, 404, 403, 200), "{r:?}"),
                Access::Local => assert_eq!((op, user, none, bootstrap, node, local(r, None)), (404, 404, 404, 404, 404, 200), "{r:?}"),
                Access::Operator => assert_eq!((op, user, other, none, bootstrap, node), (200, 403, 403, 401, 401, 401), "{r:?}"),
                Access::User => {
                    assert_eq!((op, user, other, none, bootstrap, node), (200, 200, 200, 401, 401, 401), "{r:?}");
                    assert!(Caller::Operator.sees(&web) && alice.sees(&web) && !bob.sees(&web), "{r:?}");
                }
            }
            // A mesh address with a token is still just a token caller off the mesh routes.
            if !matches!(r.access(), Access::Bootstrap | Access::MeshNode | Access::Local) {
                assert_eq!(code(r, Some(token_ip), Some(Caller::Operator)), op, "{r:?}");
                // Loopback changes nothing for them either.
                assert_eq!(local(r, Some(Caller::Operator)), op, "{r:?}");
            }
        }
        // The table itself, route by route.
        let table: Vec<Access> = routes.iter().map(Route::access).collect();
        use Access::*;
        assert_eq!(
            table,
            [
                Bootstrap, Bootstrap, MeshNode, MeshNode, Local, Operator, Operator, Operator, Operator, Operator, User, Operator, Operator, Operator,
                Operator, Operator, User, User, User, User, User, User, User, User, User
            ]
        );
    }
}
