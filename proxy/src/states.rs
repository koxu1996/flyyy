//! The schema's enumerated columns (schema.sql), as the text they're stored as: serde writes and
//! reads them lowercase, so the tables and the API's JSON keep their strings.
use serde::{Deserialize, Serialize};

/// `machines.state`: what the control plane wants of a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    Started,
    Stopped,
    Destroyed,
}

/// `machine_status.state`: what a machine agent reports a machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fact {
    /// Its image is being built.
    Pulling,
    Creating,
    Starting,
    Started,
    Stopping,
    Stopped,
    Failed,
    Destroyed,
}

/// `apps.state` and `orgs.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Active,
    Destroyed,
}

/// `service_status.state`: a service's health check, or down once its machine isn't running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Passing,
    Failing,
    Down,
}

/// `users.role`.
// shortcut: recorded and shown; admin and member have the same rights over their org's apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
}

/// `ip_pools.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolKind {
    /// An address HTTP is served on, by Host.
    Shared,
    /// Where apps' dedicated addresses come from.
    Pool,
}

/// A mesh node's role: in its join and its certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Worker,
    Edge,
    /// The public dashboard: on the mesh for the control plane's API only; it gets no certificate.
    Dashboard,
}

macro_rules! display_as_stored {
    ($($t:ty),*) => {$(
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(&text(*self))
            }
        }

        impl std::str::FromStr for $t {
            type Err = ();

            /// From its stored text; Err for anything else.
            fn from_str(s: &str) -> Result<Self, ()> {
                serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|_| ())
            }
        }
    )*};
}
display_as_stored!(Desired, Fact, Lifecycle, Health, Role, PoolKind, NodeRole);

/// The text of a value, as the tables and logs have it (`"started"`, `"passing"`, ...).
pub fn text<T: Serialize>(v: T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        other => unreachable!("a unit variant serializes as a string, not {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_as_lowercase_text() {
        assert_eq!(text(Fact::Pulling), "pulling");
        assert_eq!(text(Health::Down), "down");
        assert_eq!(serde_json::from_str::<Desired>("\"destroyed\"").unwrap(), Desired::Destroyed);
        assert!(serde_json::from_str::<Role>("\"root\"").is_err());
        assert_eq!("edge".parse::<NodeRole>(), Ok(NodeRole::Edge));
        assert_eq!(Lifecycle::Active.to_string(), "active");
        assert!("Active".parse::<Lifecycle>().is_err());
    }
}
