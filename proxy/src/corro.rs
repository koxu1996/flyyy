//! Corrosion's API, typed: a query's rows (JSON arrays) deserialize into structs, column by column in
//! SELECT order, numbers as numbers. Field helpers for the column types SQLite has no JSON form for.
//! Every role reaches its node's Corrosion agent through `Api`: its address and the API's bearer token
//! (Corrosion's [api] authorization, required on every node, so other local processes can't read or
//! write cluster state), and `Api::client`, the one place a client is made.
use std::{net::SocketAddr, os::unix::fs::PermissionsExt};

use corro_api_types::{Statement, TypedQueryEvent};
use corro_client::CorrosionApiClient;
use futures::StreamExt;
use serde::{de::DeserializeOwned, Deserialize, Deserializer};

/// Every role's flags for its Corrosion agent.
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct ApiArgs {
    /// Its Corrosion agent's API.
    #[arg(long = "corrosion")]
    addr: SocketAddr,
    /// The file with that API's bearer token: one line, not readable by others (on a node, the
    /// corroapi group's /var/lib/flyyy-corroapi/token).
    #[arg(long, env = "FLYYY_CORROSION_TOKEN_FILE")]
    corrosion_token_file: String,
}

impl ApiArgs {
    /// The API with its token read and checked; a role can't start without one.
    pub(crate) fn open(&self) -> Api {
        let token = read_token(&self.corrosion_token_file).unwrap_or_else(|e| panic!("--corrosion-token-file {}: {e}", self.corrosion_token_file));
        Api { addr: self.addr, token }
    }
}

/// A node's Corrosion API: where it is and its bearer token.
#[derive(Clone)]
pub(crate) struct Api {
    pub(crate) addr: SocketAddr,
    token: String,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Never the token.
        write!(f, "Api({})", self.addr)
    }
}

impl Api {
    /// A client that sends the token on every request.
    pub(crate) fn client(&self) -> CorrosionApiClient {
        CorrosionApiClient::with_auth_token(self.addr, Some(self.token.clone())).expect("a checked token is a valid header")
    }
}

/// The token in `path`: one line of token68 characters (RFC 7235), at least 32, in a file no other user
/// can read or write.
fn read_token(path: &str) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.permissions().mode() & 0o007 != 0 {
        return Err(format!("mode {:o}: other users can read or write it", meta.permissions().mode() & 0o777));
    }
    let token = std::fs::read_to_string(path).map_err(|e| e.to_string())?.trim().to_owned();
    if token.len() < 32 {
        return Err("want a token of at least 32 characters".into());
    }
    if !token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) {
        return Err("want token68 characters only (letters, digits, -._~+/=)".into());
    }
    Ok(token)
}

/// Every row of a query, each as a `T` (a struct whose fields are the selected columns, in order).
pub(crate) async fn query<T: DeserializeOwned + Unpin>(client: &CorrosionApiClient, stmt: Statement) -> Result<Vec<T>, String> {
    let mut stream = client.query_typed::<T>(&stmt, None).await.map_err(|e| format!("query: {e}"))?;
    let mut rows = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev.map_err(|e| format!("query: {e}"))? {
            TypedQueryEvent::Row(_, row) => rows.push(row),
            TypedQueryEvent::Error(e) => return Err(format!("query: {e}")),
            TypedQueryEvent::EndOfQuery { .. } => break,
            _ => {}
        }
    }
    Ok(rows)
}

/// A column of a row nobody here wrote (gossip from anywhere): None, rather than failing the whole query,
/// when it isn't of its type (SQLite keeps a non-number written into an INTEGER column as text).
pub(crate) fn maybe<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    Ok(serde_json::Value::deserialize(d).ok().and_then(|v| serde_json::from_value(v).ok()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_files_are_checked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("corro-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str, mode: u32| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p.to_str().unwrap().to_owned()
        };
        let good = "0123456789abcdef".repeat(4);
        assert_eq!(super::read_token(&write("ok", &format!("{good}\n"), 0o640)).unwrap(), good);
        assert!(super::read_token(&write("world", &good, 0o644)).unwrap_err().contains("other users"));
        assert!(super::read_token(&write("short", "abc", 0o600)).unwrap_err().contains("32"));
        assert!(super::read_token(&write("space", &format!("{good} x"), 0o600)).unwrap_err().contains("token68"));
        assert!(super::read_token(dir.join("missing").to_str().unwrap()).is_err());
        let api = super::Api { addr: "127.0.0.1:8080".parse().unwrap(), token: good.clone() };
        assert!(!format!("{api:?}").contains(&good), "Debug leaves the token out");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use super::*;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Row {
        name: String,
        port: u16,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Heartbeat(String, #[serde(deserialize_with = "maybe")] Option<i64>);

    /// Every query a role sends, against the schema every Corrosion agent gets: SQLite prepares each one,
    /// so a table or column that isn't there fails here, not on a node.
    #[test]
    fn every_query_names_columns_of_the_schema() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../schema.sql")).unwrap();
        let queries = [
            ("edge subscription", crate::edge_routes::SUBSCRIPTION.to_owned()),
            ("backhaul subscription", crate::backhaul::SUBSCRIPTION.to_owned()),
            ("edge: pools", crate::edge::POOLS.to_owned()),
            ("edge: allocated", crate::edge::ALLOCATED.to_owned()),
            ("edge: heartbeats", crate::edge::HEARTBEATS.to_owned()),
            ("anyip and control: active prefixes", crate::ingress::ACTIVE_PREFIXES.to_owned()),
            ("health: stored", crate::health::STORED.to_owned()),
            ("agent: images reported", crate::agent::report::REPORTED.to_owned()),
            ("agent: drain", crate::agent::reconcile::UP.replace("{marks}", "?, ?")),
            ("control: checks", crate::control::store::CHECKS.to_owned()),
            ("control: images", crate::control::store::IMAGES.to_owned()),
            ("control: workers", crate::control::store::WORKERS.to_owned()),
            ("control: facts", crate::control::store::FACTS.to_owned()),
            ("control: edges", crate::control::load::EDGES.to_owned()),
        ];
        for (what, sql) in queries {
            if let Err(e) = db.prepare(&sql) {
                panic!("{what}: {e}\n{sql}");
            }
        }
        // And one that isn't so, so the check itself is known to bite.
        assert!(db.prepare("SELECT nope FROM workers").is_err());
    }

    #[test]
    fn untrusted_columns_are_none_when_off_type() {
        assert_eq!(serde_json::from_str::<Heartbeat>(r#"["w1", 12]"#).unwrap(), Heartbeat("w1".into(), Some(12)));
        assert_eq!(serde_json::from_str::<Heartbeat>(r#"["w1", "soon"]"#).unwrap(), Heartbeat("w1".into(), None));
    }

    #[test]
    fn rows_are_arrays_of_columns() {
        let r: Row = serde_json::from_str(r#"["blue", 80]"#).unwrap();
        assert_eq!(r, Row { name: "blue".into(), port: 80 });
        assert!(serde_json::from_str::<Row>(r#"["blue", "80"]"#).is_err(), "a TEXT where an INTEGER belongs");
    }
}
