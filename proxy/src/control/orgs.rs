//! `/v1/orgs` and `/v1/users`, the operator's: create, list and destroy orgs, create and list users.

use hyper::{body::Incoming, Request, Response, StatusCode};
use serde::Deserialize;

use super::{
    api::body,
    auth::{new_token, token_hash},
    db::{OrgRec, UserRec},
    error, json,
    model::app_name_ok,
    routes::Route,
    Control,
};
use crate::{
    api_error::ApiError,
    control::model::{Org, User},
    states::{Lifecycle, Role},
    util::now_ms,
    Body,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewOrg {
    pub(crate) name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewUser {
    pub(crate) name: String,
    pub(crate) org: String,
    #[serde(default = "default_role")]
    pub(crate) role: String,
}

pub(crate) fn default_role() -> String {
    "member".into()
}

impl Control {
    /// `/v1/orgs[/<name>]`, the operator's: list, create (or bring a destroyed one back), destroy (refused
    /// while it has active apps; its users' tokens stop working).
    pub(crate) async fn orgs_route(&self, route: Route, req: Request<Incoming>) -> Result<Response<Body>, ApiError> {
        let unavailable = |e: String| error(StatusCode::SERVICE_UNAVAILABLE, e);
        match route {
            Route::OrgsList => {
                let (orgs, apps, users) = (self.orgs(), self.apps(None), self.users());
                Ok(json(
                    StatusCode::OK,
                    orgs.iter()
                        .map(|Org { name, state }| {
                            serde_json::json!({
                                "name": name, "state": state,
                                "apps": apps.iter().filter(|a| &a.org == name && a.state == Lifecycle::Active).count(),
                                "users": users.iter().filter(|u| &u.org == name).count(),
                            })
                        })
                        .collect(),
                ))
            }
            Route::OrgCreate => {
                let o: NewOrg = body(req).await?;
                if !app_name_ok(&o.name) {
                    return Err(error(StatusCode::BAD_REQUEST, format!("bad org name {:?}: want [a-z][a-z0-9-]{{0,19}}", o.name)));
                }
                self.db
                    .write(|r| match r.orgs.get_mut(&o.name) {
                        Some(org) if org.state == Lifecycle::Active => Err(error(StatusCode::CONFLICT, format!("org {} exists", o.name))),
                        Some(org) => {
                            org.state = Lifecycle::Active;
                            Ok(())
                        }
                        None => {
                            r.orgs.insert(o.name.clone(), OrgRec { state: Lifecycle::Active, created_at: now_ms() });
                            Ok(())
                        }
                    })
                    .map_err(unavailable)??;
                Ok(json(StatusCode::CREATED, serde_json::json!({ "name": o.name, "state": "active" })))
            }
            Route::OrgDestroy { org } => {
                let name = org.as_str();
                self.db
                    .write(|r| {
                        if !r.orgs.get(name).is_some_and(|o| o.state == Lifecycle::Active) {
                            return Err(error(StatusCode::NOT_FOUND, format!("no org {name}")));
                        }
                        let active: Vec<&str> = r.apps.values().filter(|a| a.org == name && a.state == Lifecycle::Active).map(|a| a.name.as_str()).collect();
                        if !active.is_empty() {
                            return Err(error(StatusCode::CONFLICT, format!("org {name} has active apps: {}; destroy them first", active.join(", "))));
                        }
                        r.orgs.get_mut(name).expect("checked above").state = Lifecycle::Destroyed;
                        for u in r.users.values_mut().filter(|u| u.org == name) {
                            u.token_sha256.clear();
                        }
                        Ok(())
                    })
                    .map_err(unavailable)??;
                Ok(json(StatusCode::ACCEPTED, serde_json::json!({ "name": name, "state": "destroyed" })))
            }
            r => unreachable!("not an orgs route: {r:?}"),
        }
    }

    /// `/v1/users`, the operator's: list them, or create one in an active org. The new token is in the
    /// answer once; only its hash is kept.
    pub(crate) async fn users_route(&self, route: Route, req: Request<Incoming>) -> Result<Response<Body>, ApiError> {
        let unavailable = |e: String| error(StatusCode::SERVICE_UNAVAILABLE, e);
        match route {
            Route::UsersList => {
                let users = self.users();
                Ok(json(StatusCode::OK, users.iter().map(|User { name, org, role }| serde_json::json!({ "name": name, "org": org, "role": role })).collect()))
            }
            Route::UserCreate => {
                let u: NewUser = body(req).await?;
                if !app_name_ok(&u.name) {
                    return Err(error(StatusCode::BAD_REQUEST, format!("bad user name {:?}: want [a-z][a-z0-9-]{{0,19}}", u.name)));
                }
                let role: Role = u.role.parse().map_err(|()| error(StatusCode::BAD_REQUEST, format!("bad role {:?}: want admin or member", u.role)))?;
                let token = new_token().map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
                self.db
                    .write(|r| {
                        if !r.orgs.get(&u.org).is_some_and(|o| o.state == Lifecycle::Active) {
                            return Err(error(StatusCode::NOT_FOUND, format!("no org {:?}", u.org)));
                        }
                        // Names are never reused.
                        if r.users.contains_key(&u.name) {
                            return Err(error(StatusCode::CONFLICT, format!("user {} exists", u.name)));
                        }
                        r.users.insert(u.name.clone(), UserRec { org: u.org.clone(), role, token_sha256: token_hash(&token), created_at: now_ms() });
                        Ok(())
                    })
                    .map_err(unavailable)??;
                tracing::info!(user = %u.name, org = %u.org, %role, "user created");
                Ok(json(StatusCode::CREATED, serde_json::json!({ "name": u.name, "org": u.org, "role": role, "token": token })))
            }
            r => unreachable!("not a users route: {r:?}"),
        }
    }
}
