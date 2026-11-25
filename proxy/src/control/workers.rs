//! `GET /v1/workers`, the operator's: every worker's row, whether it's alive and valid, its capacity
//! and allocation, and its images' size.

use hyper::{Response, StatusCode};

use super::{
    error, json,
    model::{live_workers, valid_worker, worker_image_bytes},
    Control,
};
use crate::{api_error::ApiError, util::now_ms, Body};

impl Control {
    pub(crate) async fn workers_route(&self) -> Result<Response<Body>, ApiError> {
        let unavailable = |e: String| error(StatusCode::SERVICE_UNAVAILABLE, e);
        let rows = self.workers().await.map_err(unavailable)?;
        let images = self.images().await.map_err(unavailable)?;
        let now = now_ms();
        let live = live_workers(&rows, now, self.single_host);
        let list: Vec<_> = rows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "name": w.name, "backhaul": w.backhaul, "alive": live.contains(&w.name),
                    "valid": valid_worker(w, self.single_host).is_ok(), "heartbeat_age_ms": now - w.heartbeat_at, "uptime_ms": now - w.started_at,
                    "single_host": self.single_host,
                    "machines": w.machines,
                    "arch": w.arch, "cpus": w.cpus, "memory": w.memory,
                    "allocated_cpus": w.allocated_cpus, "allocated_memory": w.allocated_memory,
                    "image_bytes": worker_image_bytes(&w.name, &images),
                })
            })
            .collect();
        Ok(json(StatusCode::OK, serde_json::Value::Array(list)))
    }
}
