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

//! DNS-based upstream discovery.
//!
//! Periodically resolves `BGE_ROUTER_GPU_DNS` and `BGE_ROUTER_CPU_DNS` via
//! `tokio::net::lookup_host`. Discovered addresses are merged into the current
//! [`PoolSnapshot`]: new addresses are added as [`UpstreamStatus::Unknown`],
//! disappeared addresses are removed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

/// Spawn the DNS discovery background task.
///
/// The task runs until the process exits. It refreshes both DNS names every
/// `config.dns_refresh` and atomically replaces the pool snapshot.
pub fn spawn(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>) {
    tokio::spawn(async move {
        run(pool, config).await;
    });
}

async fn run(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>) {
    let mut interval = tokio::time::interval(config.dns_refresh);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        refresh(&pool, &config).await;
    }
}

async fn refresh(pool: &ArcSwap<PoolSnapshot>, config: &Config) {
    let (gpu_addrs, cpu_addrs) = tokio::join!(
        resolve(&config.gpu_dns, 8081),
        resolve(&config.cpu_dns, 8081),
    );

    tracing::debug!(
        gpu_resolved = gpu_addrs.len(),
        cpu_resolved = cpu_addrs.len(),
        "DNS refresh"
    );

    let current = pool.load();
    let new_snapshot = merge(
        &current,
        &gpu_addrs,
        PoolType::Gpu,
        &cpu_addrs,
        PoolType::Cpu,
    );
    pool.store(Arc::new(new_snapshot));
}

async fn resolve(dns_name: &str, port: u16) -> Vec<SocketAddr> {
    let host = format!("{dns_name}:{port}");
    let result = tokio::net::lookup_host(host.as_str()).await;
    match result {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            tracing::warn!(dns_name, err = %e, "DNS lookup failed");
            Vec::new()
        }
    }
}

/// Merge newly-resolved addresses with the existing snapshot.
///
/// - Addresses already in the snapshot keep their current health state.
/// - New addresses are inserted with [`UpstreamStatus::Unknown`].
/// - Addresses no longer in DNS are removed.
fn merge(
    current: &PoolSnapshot,
    gpu_addrs: &[SocketAddr],
    gpu_pool: PoolType,
    cpu_addrs: &[SocketAddr],
    cpu_pool: PoolType,
) -> PoolSnapshot {
    let gpu = merge_pool(&current.gpu, gpu_addrs, gpu_pool);
    let cpu = merge_pool(&current.cpu, cpu_addrs, cpu_pool);
    PoolSnapshot {
        gpu,
        cpu,
        updated_at: Instant::now(),
    }
}

fn merge_pool(
    existing: &[UpstreamInfo],
    resolved: &[SocketAddr],
    pool_type: PoolType,
) -> Vec<UpstreamInfo> {
    resolved
        .iter()
        .map(|&addr| {
            existing
                .iter()
                .find(|u| u.addr == addr)
                .cloned()
                .unwrap_or_else(|| UpstreamInfo {
                    addr,
                    pool_type,
                    status: UpstreamStatus::Unknown,
                    queue_depth: 0,
                    live_workers: 0,
                    last_seen: Instant::now(),
                })
        })
        .collect()
}
