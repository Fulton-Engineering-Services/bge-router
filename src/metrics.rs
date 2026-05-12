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

//! Periodic heartbeat log emission.
//!
//! Every `BGE_ROUTER_HEARTBEAT_SECS` seconds, emits a structured `info!` event
//! with pool summary metrics. Set the env var to `0` to disable.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::upstream::snapshot::{PoolSnapshot, UpstreamStatus};

/// Spawn the heartbeat background task.
///
/// If `config.heartbeat` is zero, no task is spawned.
pub fn spawn(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>) {
    if config.heartbeat.is_zero() {
        return;
    }
    tokio::spawn(async move {
        run(pool, config).await;
    });
}

async fn run(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>) {
    let mut interval = tokio::time::interval(config.heartbeat);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the first tick (fires immediately at t=0).
    interval.tick().await;
    loop {
        interval.tick().await;
        emit_heartbeat(&pool);
    }
}

fn emit_heartbeat(pool: &ArcSwap<PoolSnapshot>) {
    let snapshot = pool.load();

    let gpu_ok = snapshot
        .gpu
        .iter()
        .filter(|u| u.status == UpstreamStatus::Ok)
        .count();
    let cpu_ok = snapshot
        .cpu
        .iter()
        .filter(|u| u.status == UpstreamStatus::Ok)
        .count();
    let gpu_queue_sum: u32 = snapshot.gpu.iter().map(|u| u.queue_depth).sum();
    let cpu_queue_sum: u32 = snapshot.cpu.iter().map(|u| u.queue_depth).sum();

    tracing::info!(
        message = "heartbeat",
        gpu_upstreams = snapshot.gpu.len(),
        cpu_upstreams = snapshot.cpu.len(),
        gpu_ok_count = gpu_ok,
        cpu_ok_count = cpu_ok,
        gpu_queue_depth_sum = gpu_queue_sum,
        cpu_queue_depth_sum = cpu_queue_sum,
    );
}
