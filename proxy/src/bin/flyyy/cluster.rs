//! The cluster, the operator's: orgs, users, workers.

use std::process::ExitCode;

use clap::Subcommand;
use hyper::Method;
use proxy::{client::Client, states::Role};
use serde_json::Value;

use crate::{
    out::{s, show, table},
    Json,
};

#[derive(Subcommand, Debug)]
pub(crate) enum OrgVerb {
    Create {
        org: String,
    },
    /// Refused while it has active apps; its users' tokens stop working.
    Destroy {
        org: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum UserVerb {
    /// Prints the user's API token, once.
    Create {
        user: String,
        #[arg(long)]
        org: String,
        #[arg(long, value_enum, default_value_t = Role::Member)]
        role: Role,
    },
}

pub(crate) async fn orgs(client: &Client, verb: Option<OrgVerb>, json: &Json) -> Result<ExitCode, String> {
    match verb {
        None => {
            let list = client.ok(Method::GET, "/v1/orgs", None).await?;
            show(json, &list, &|list| {
                let mut rows = vec![["NAME", "STATE", "APPS", "USERS"].map(String::from).to_vec()];
                rows.extend(list.as_array().into_iter().flatten().map(|o| vec![s(&o["name"]), s(&o["state"]), o["apps"].to_string(), o["users"].to_string()]));
                table(rows);
            });
            Ok(ExitCode::SUCCESS)
        }
        Some(OrgVerb::Create { org }) => {
            client.ok(Method::POST, "/v1/orgs", Some(serde_json::json!({ "name": org }))).await?;
            println!("org {org}: created");
            Ok(ExitCode::SUCCESS)
        }
        Some(OrgVerb::Destroy { org }) => {
            client.ok(Method::DELETE, &format!("/v1/orgs/{org}"), None).await?;
            println!("org {org}: destroyed");
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub(crate) async fn users(client: &Client, verb: Option<UserVerb>, json: &Json) -> Result<ExitCode, String> {
    match verb {
        None => {
            let list = client.ok(Method::GET, "/v1/users", None).await?;
            show(json, &list, &|list| {
                let mut rows = vec![["NAME", "ORG", "ROLE"].map(String::from).to_vec()];
                rows.extend(list.as_array().into_iter().flatten().map(|u| vec![s(&u["name"]), s(&u["org"]), s(&u["role"])]));
                table(rows);
            });
            Ok(ExitCode::SUCCESS)
        }
        Some(UserVerb::Create { user, org, role }) => {
            let body = serde_json::json!({ "name": user, "org": org, "role": role });
            let u = client.ok(Method::POST, "/v1/users", Some(body)).await?;
            eprintln!("flyyy: user {user} ({}) in org {}; its API token, shown only now:", s(&u["role"]), s(&u["org"]));
            println!("{}", s(&u["token"]));
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub(crate) async fn workers(client: &Client, json: &Json) -> Result<ExitCode, String> {
    let workers = client.ok(Method::GET, "/v1/workers", None).await?;
    show(json, &workers, &|workers| {
        if workers.as_array().into_iter().flatten().any(|w| w["single_host"] == true) {
            println!("single-host (dev) mode: backhauls on loopback, no mesh; real deployments are mesh-only");
        }
        let mut rows = vec![["NAME", "BACKHAUL", "ALIVE", "HEARTBEAT", "UPTIME", "MACHINES", "ARCH", "CPUS", "MEMORY (MIB)"].map(String::from).to_vec()];
        let secs = |v: &Value| format!("{:.1}s", v.as_f64().unwrap_or_default() / 1000.0);
        for w in workers.as_array().into_iter().flatten() {
            let alive = if w["valid"] == false {
                "invalid"
            } else if w["alive"] == true {
                "yes"
            } else {
                "no"
            };
            // Allocated (its live machines' sizes, overcommit allowed) / capacity (as it announced it).
            let used = |a: &str, c: &str| format!("{}/{}", w[a], w[c]);
            rows.push(vec![
                s(&w["name"]),
                s(&w["backhaul"]),
                alive.into(),
                format!("{} ago", secs(&w["heartbeat_age_ms"])),
                secs(&w["uptime_ms"]),
                w["machines"].to_string(),
                s(&w["arch"]),
                used("allocated_cpus", "cpus"),
                used("allocated_memory", "memory"),
            ]);
        }
        table(rows);
    });
    Ok(ExitCode::SUCCESS)
}
