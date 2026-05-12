// Copyright (c) 2026 J. Patrick Fulton
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Router health endpoint (`GET /router/health`).
//!
//! Returns the router's own view of the upstream pool — statuses, queue depths,
//! and live worker counts for every discovered upstream.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;
use std::time::Instant;

use crate::state::AppState;
use crate::upstream::snapshot::{PoolType, UpstreamStatus};

/// `GET /router/health` — router's own health and upstream pool snapshot.
pub async fn router_health(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.pool.load_full();
    let now = Instant::now();

    let gpu: Vec<_> = snapshot
        .gpu
        .iter()
        .map(|u| UpstreamView::from_info(u, now))
        .collect();
    let cpu: Vec<_> = snapshot
        .cpu
        .iter()
        .map(|u| UpstreamView::from_info(u, now))
        .collect();

    let gpu_ok = gpu.iter().filter(|u| u.status == "ok").count();
    let cpu_ok = cpu.iter().filter(|u| u.status == "ok").count();

    let status = if gpu_ok > 0 || cpu_ok > 0 {
        "ok"
    } else {
        "degraded"
    };
    let http_status = if gpu_ok == 0 && cpu_ok == 0 {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (
        http_status,
        Json(RouterHealthResponse {
            status,
            gpu_upstreams: gpu,
            cpu_upstreams: cpu,
        }),
    )
}

#[derive(Serialize)]
struct RouterHealthResponse {
    status: &'static str,
    gpu_upstreams: Vec<UpstreamView>,
    cpu_upstreams: Vec<UpstreamView>,
}

#[derive(Serialize)]
struct UpstreamView {
    addr: String,
    pool_type: &'static str,
    status: &'static str,
    queue_depth: u32,
    live_workers: u32,
    last_seen_secs_ago: f64,
}

impl UpstreamView {
    fn from_info(info: &crate::upstream::snapshot::UpstreamInfo, now: Instant) -> Self {
        Self {
            addr: info.addr.to_string(),
            pool_type: match info.pool_type {
                PoolType::Gpu => "gpu",
                PoolType::Cpu => "cpu",
            },
            status: match info.status {
                UpstreamStatus::Ok => "ok",
                UpstreamStatus::Loading => "loading",
                UpstreamStatus::Fail => "fail",
                UpstreamStatus::Unknown => "unknown",
            },
            queue_depth: info.queue_depth,
            live_workers: info.live_workers,
            last_seen_secs_ago: now.duration_since(info.last_seen).as_secs_f64(),
        }
    }
}
