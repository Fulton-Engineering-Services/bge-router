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

//! Per-upstream health polling.
//!
//! Every `config.health_poll` seconds this task GETs `/health` on each known
//! upstream, parses the bge-m3 health JSON, and atomically replaces the pool
//! snapshot with updated status, queue depth, and live worker counts.

use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use serde::Deserialize;

use crate::config::Config;
use crate::upstream::snapshot::{PoolSnapshot, UpstreamInfo, UpstreamScheme, UpstreamStatus};

/// Spawn the health-polling background task.
pub fn spawn(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>, client: reqwest::Client) {
    tokio::spawn(async move {
        run(pool, config, client).await;
    });
}

async fn run(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>, client: reqwest::Client) {
    let scheme = config.upstream_scheme();
    let mut interval = tokio::time::interval(config.health_poll);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        poll_all(&pool, &client, scheme).await;
    }
}

async fn poll_all(pool: &ArcSwap<PoolSnapshot>, client: &reqwest::Client, scheme: UpstreamScheme) {
    let snapshot = pool.load_full();
    let addrs: Vec<_> = snapshot
        .gpu
        .iter()
        .chain(snapshot.cpu.iter())
        .map(|u| u.addr)
        .collect();

    if addrs.is_empty() {
        return;
    }

    // Poll all upstreams concurrently.
    let results = poll_concurrent(client, addrs, scheme).await;

    // Apply updates to the snapshot.
    let current = pool.load();
    let new_snapshot = apply_updates(&current, &results);
    pool.store(Arc::new(new_snapshot));
}

struct PollResult {
    addr: std::net::SocketAddr,
    status: UpstreamStatus,
    queue_depth: u32,
    live_workers: u32,
}

async fn poll_concurrent(
    client: &reqwest::Client,
    addrs: Vec<std::net::SocketAddr>,
    scheme: UpstreamScheme,
) -> Vec<PollResult> {
    let mut set = tokio::task::JoinSet::new();
    for addr in addrs {
        let client = client.clone();
        set.spawn(async move { poll_one(&client, addr, scheme).await });
    }
    let mut results = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(result) = r {
            results.push(result);
        }
    }
    results
}

async fn poll_one(
    client: &reqwest::Client,
    addr: std::net::SocketAddr,
    scheme: UpstreamScheme,
) -> PollResult {
    let url = format!("{scheme}://{addr}/health");
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<BgeMHealthResponse>().await {
                return PollResult {
                    addr,
                    status: UpstreamStatus::parse(&body.status),
                    queue_depth: body.queue_depth.unwrap_or(0),
                    live_workers: body.workers.map_or(0, |w| w.live),
                };
            }
            PollResult {
                addr,
                status: UpstreamStatus::Unknown,
                queue_depth: 0,
                live_workers: 0,
            }
        }
        Ok(_) => PollResult {
            addr,
            status: UpstreamStatus::Fail,
            queue_depth: 0,
            live_workers: 0,
        },
        Err(e) => {
            tracing::debug!(%addr, err = %e, "health poll failed");
            PollResult {
                addr,
                status: UpstreamStatus::Fail,
                queue_depth: 0,
                live_workers: 0,
            }
        }
    }
}

fn apply_updates(current: &PoolSnapshot, results: &[PollResult]) -> PoolSnapshot {
    let now = Instant::now();
    let gpu = update_pool(&current.gpu, results, now);
    let cpu = update_pool(&current.cpu, results, now);
    PoolSnapshot {
        gpu,
        cpu,
        updated_at: now,
    }
}

fn update_pool(pool: &[UpstreamInfo], results: &[PollResult], now: Instant) -> Vec<UpstreamInfo> {
    pool.iter()
        .map(|upstream| {
            if let Some(r) = results.iter().find(|r| r.addr == upstream.addr) {
                UpstreamInfo {
                    addr: upstream.addr,
                    pool_type: upstream.pool_type,
                    status: r.status,
                    queue_depth: r.queue_depth,
                    live_workers: r.live_workers,
                    last_seen: now,
                }
            } else {
                upstream.clone()
            }
        })
        .collect()
}

/// Subset of the bge-m3 `/health` response that the router cares about.
#[derive(Debug, Deserialize)]
struct BgeMHealthResponse {
    status: String,
    workers: Option<WorkersField>,
    queue_depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WorkersField {
    live: u32,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use super::{BgeMHealthResponse, PollResult, apply_updates};
    use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

    fn unknown_gpu(addr: SocketAddr) -> UpstreamInfo {
        UpstreamInfo {
            addr,
            pool_type: PoolType::Gpu,
            status: UpstreamStatus::Unknown,
            queue_depth: 0,
            live_workers: 0,
            last_seen: Instant::now(),
        }
    }

    // ── BgeMHealthResponse JSON parsing ────────────────────────────────────

    #[test]
    fn parse_ok_with_workers_and_queue_depth() {
        let json = r#"{"status":"ok","workers":{"live":8,"total":8},"queue_depth":3}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Ok);
        assert_eq!(resp.workers.unwrap().live, 8);
        assert_eq!(resp.queue_depth.unwrap(), 3);
    }

    #[test]
    fn parse_loading_gives_loading_status_and_zero_defaults() {
        let json = r#"{"status":"loading"}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Loading);
        assert_eq!(resp.queue_depth.unwrap_or(0), 0);
        assert_eq!(resp.workers.map_or(0, |w| w.live), 0);
    }

    #[test]
    fn parse_fail_gives_fail_status() {
        let json = r#"{"status":"fail"}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Fail);
    }

    #[test]
    fn parse_idle_is_treated_as_loading() {
        let json = r#"{"status":"idle"}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Loading);
    }

    #[test]
    fn parse_warn_is_treated_as_ok() {
        let json = r#"{"status":"warn","workers":{"live":3,"total":8}}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Ok);
        assert_eq!(resp.workers.unwrap().live, 3);
    }

    #[test]
    fn parse_unknown_status_string_gives_unknown() {
        let json = r#"{"status":"something_else"}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Unknown);
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let result = serde_json::from_str::<BgeMHealthResponse>("not valid json");
        assert!(result.is_err(), "invalid JSON should fail deserialization");
    }

    #[test]
    fn parse_ok_without_workers_field_gives_zero_live_workers() {
        let json = r#"{"status":"ok"}"#;
        let resp: BgeMHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(UpstreamStatus::parse(&resp.status), UpstreamStatus::Ok);
        assert_eq!(resp.workers.map_or(0, |w| w.live), 0);
        assert_eq!(resp.queue_depth.unwrap_or(0), 0);
    }

    // ── apply_updates ─────────────────────────────────────────────────────

    #[test]
    fn apply_updates_empty_pool_stays_empty() {
        let current = PoolSnapshot::default();
        let results: Vec<PollResult> = vec![];
        let updated = apply_updates(&current, &results);
        assert!(updated.gpu.is_empty());
        assert!(updated.cpu.is_empty());
    }

    #[test]
    fn apply_updates_matching_result_overwrites_status_and_depth() {
        let addr: SocketAddr = "10.0.0.1:8081".parse().unwrap();
        let current = PoolSnapshot {
            gpu: vec![unknown_gpu(addr)],
            cpu: vec![],
            updated_at: Instant::now(),
        };
        let results = vec![PollResult {
            addr,
            status: UpstreamStatus::Ok,
            queue_depth: 7,
            live_workers: 4,
        }];
        let updated = apply_updates(&current, &results);
        assert_eq!(updated.gpu.len(), 1);
        assert_eq!(updated.gpu[0].status, UpstreamStatus::Ok);
        assert_eq!(updated.gpu[0].queue_depth, 7);
        assert_eq!(updated.gpu[0].live_workers, 4);
    }

    #[test]
    fn apply_updates_no_matching_result_leaves_upstream_unchanged() {
        let addr: SocketAddr = "10.0.0.1:8081".parse().unwrap();
        let other: SocketAddr = "10.0.0.99:8081".parse().unwrap();
        let current = PoolSnapshot {
            gpu: vec![unknown_gpu(addr)],
            cpu: vec![],
            updated_at: Instant::now(),
        };
        let results = vec![PollResult {
            addr: other,
            status: UpstreamStatus::Ok,
            queue_depth: 0,
            live_workers: 0,
        }];
        let updated = apply_updates(&current, &results);
        assert_eq!(updated.gpu.len(), 1);
        assert_eq!(
            updated.gpu[0].status,
            UpstreamStatus::Unknown,
            "unmatched upstream should keep its original status"
        );
    }
}
