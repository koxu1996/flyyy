//! Following a Corrosion subscription into a `Table`: a snapshot, then its changes, resyncing on errors.
//! Rows are typed (`Table::Row`), and lenient per column where the writer isn't trusted, since a row that
//! doesn't deserialize ends the subscription's stream (and a table would then never go live).

use std::{
    collections::BTreeMap,
    fmt::Display,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{info, warn};

use backon::{BackoffBuilder, ExponentialBuilder};
use corro_api_types::{sqlite::ChangeType, ChangeId, Statement, TypedQueryEvent};
use corro_client::CorrosionApiClient;
use futures::StreamExt;
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// What follows a subscription: an edge's routes, a backhaul's instances, an agent's machines.
pub(crate) trait Table: Send + Sync + 'static {
    /// One row, its fields the selected columns in order; Display is the log line's columns.
    type Row: DeserializeOwned + Serialize + Display + Clone + Unpin + Send + 'static;

    fn sub(&self) -> &Mutex<Sub<Self::Row>>;

    /// A snapshot went live: everything from these rows.
    fn snapshot(&self, rows: &BTreeMap<u64, Self::Row>);

    /// One change: the row it replaced (or removed) and the row it is now; `rows` has it applied.
    fn change(&self, old: Option<&Self::Row>, new: Option<&Self::Row>, rows: &BTreeMap<u64, Self::Row>);
}

/// Subscription bookkeeping and the current rows, also what `/state` reports.
pub(crate) struct Sub<R> {
    pub(crate) id: Option<String>,
    pub(crate) last_change_id: Option<u64>,
    pub(crate) snapshots: u64,
    pub(crate) resyncs: u64,
    pub(crate) rows: BTreeMap<u64, R>,
}

impl<R> Default for Sub<R> {
    fn default() -> Self {
        Sub { id: None, last_change_id: None, snapshots: 0, resyncs: 0, rows: BTreeMap::new() }
    }
}

impl<R: Serialize> Sub<R> {
    /// Its part of `/state`.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "subscription_id": self.id,
            "last_change_id": self.last_change_id,
            "snapshots": self.snapshots,
            "resyncs": self.resyncs,
            "rows": self.rows.values().collect::<Vec<_>>(),
        })
    }
}

/// One subscription session: snapshot into staging, swap at end-of-query, then apply changes.
/// Returns whether a snapshot went live and why the session ended. Every ending is terminal:
/// the SDK already checks change ids (`MissedChange`) and resumes `from` on I/O errors itself.
/// A full snapshot of the existing subscription is preferred over a new one, since every
/// POST creates a new subscription on the agent, even for identical SQL.
async fn session<T: Table>(client: &CorrosionApiClient, stmt: &Statement, table: &T, sub_id: &mut Option<Uuid>) -> (bool, String) {
    let existing = match *sub_id {
        Some(id) => client.subscription_typed::<T::Row>(id, false, None).await.map_err(|e| warn!(sub = %id, error = %e, "re-attach failed")).ok(),
        None => None,
    };
    let mut stream = match existing {
        Some(s) => s,
        None => match client.subscribe_typed::<T::Row>(stmt, false, None).await {
            Ok(s) => s,
            Err(e) => return (false, format!("subscribe: {e}")),
        },
    };
    *sub_id = Some(stream.id());
    let id = stream.id().to_string();
    let mut staging = BTreeMap::new();
    let mut live = false;
    while let Some(event) = stream.next().await {
        match event {
            Err(e) => return (live, format!("stream: {e}")),
            Ok(TypedQueryEvent::Columns(_)) => {}
            Ok(TypedQueryEvent::Row(rowid, row)) if !live => {
                staging.insert(rowid.0, row);
            }
            Ok(TypedQueryEvent::EndOfQuery { change_id, .. }) if !live => {
                let mut sub = table.sub().lock().unwrap();
                sub.rows = std::mem::take(&mut staging);
                sub.id = Some(id.clone());
                sub.last_change_id = change_id.map(|c| c.0);
                sub.snapshots += 1;
                table.snapshot(&sub.rows);
                info!(sub = %id, rows = sub.rows.len(), change_id = ?sub.last_change_id, "snapshot");
                live = true;
            }
            Ok(TypedQueryEvent::Change(kind, rowid, row, ChangeId(cid))) if live => {
                let mut sub = table.sub().lock().unwrap();
                info!(change_id = cid, kind = ?kind, rowid = rowid.0, row = %row, "change");
                let old = match kind {
                    ChangeType::Delete => sub.rows.remove(&rowid.0),
                    ChangeType::Insert | ChangeType::Update => sub.rows.insert(rowid.0, row.clone()),
                };
                sub.last_change_id = Some(cid);
                let new = (kind != ChangeType::Delete).then_some(&row);
                table.change(old.as_ref(), new, &sub.rows);
            }
            Ok(TypedQueryEvent::Error(e)) => return (live, format!("error event: {e}")),
            Ok(other) => return (live, format!("unexpected event (live={live}): {:?}", other.meta())),
        }
    }
    (live, "stream ended".into())
}

/// Follow the subscription forever. The current table keeps serving while we resync.
pub(crate) async fn follow<T: Table>(api: crate::corro::Api, stmt: Statement, table: Arc<T>) {
    let client = api.client();
    let backoff = ExponentialBuilder::new().with_jitter().with_min_delay(Duration::from_millis(100)).with_max_delay(Duration::from_secs(5)).without_max_times();
    let mut delays = backoff.build();
    let mut sub_id = None;
    loop {
        let (went_live, reason) = session(&client, &stmt, &*table, &mut sub_id).await;
        if went_live {
            delays = backoff.build();
        }
        let delay = delays.next().expect("unbounded backoff");
        table.sub().lock().unwrap().resyncs += 1;
        warn!(delay = ?delay, reason = %reason, "resync");
        tokio::time::sleep(delay).await;
    }
}
