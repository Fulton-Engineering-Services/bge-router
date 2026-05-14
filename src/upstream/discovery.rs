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
//!
//! ## Failure handling
//!
//! A DNS lookup that returns an error (NXDOMAIN, timeout, network error) is
//! distinguished from a successful lookup that returned zero addresses:
//!
//! - On `Err`: the pool is **preserved** at its last-known-good state. The
//!   health poller continues running against the existing addresses; any
//!   address that is genuinely gone will be marked `Fail` within one health
//!   cycle and become unroutable.
//! - On `Ok(empty)`: the pool is cleared. This represents a legitimate
//!   "service has zero instances" signal (e.g. ECS scale-to-zero) and the
//!   addresses really are gone.
//!
//! ## Adaptive refresh cadence
//!
//! The loop uses `compute_next_interval` to back off on failure:
//!
//! - Steady state (both pools have addresses): refresh every
//!   `config.dns_refresh` (default 30 s).
//! - On transition healthy → unhealthy: drop to `INITIAL_RETRY_INTERVAL`
//!   (2 s).
//! - While unhealthy: double the previous interval each cycle, capped at
//!   `config.dns_refresh`.
//!
//! This collapses cold-start latency when an upstream service comes up shortly
//! after the router boots, without hammering DNS during extended outages.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

/// Initial retry interval used the moment a refresh produces an empty or
/// failed result. Subsequent failures back off exponentially up to
/// `config.dns_refresh`.
pub(crate) const INITIAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Outcome of a single DNS resolution attempt.
///
/// Distinguishing `Failed` from `Resolved(vec![])` lets the merge step
/// preserve the last-known-good pool through transient DNS errors while still
/// honouring a legitimate "zero instances" signal from the resolver.
#[derive(Debug, Clone)]
enum ResolveResult {
    /// DNS lookup succeeded. The vector may be empty (legitimate scale-to-zero).
    Resolved(Vec<SocketAddr>),
    /// DNS lookup errored. Caller should preserve the previous pool state.
    Failed,
}

impl ResolveResult {
    /// Returns `true` when the lookup succeeded and produced at least one address.
    fn has_addrs(&self) -> bool {
        matches!(self, Self::Resolved(addrs) if !addrs.is_empty())
    }
}

/// Spawn the DNS discovery background task.
///
/// The task runs until the process exits. It refreshes both DNS names on an
/// adaptive cadence — see the module-level docs for details.
pub fn spawn(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>) {
    tokio::spawn(async move {
        run(pool, config).await;
    });
}

async fn run(pool: Arc<ArcSwap<PoolSnapshot>>, config: Arc<Config>) {
    let mut sleep_for = Duration::ZERO;
    // Optimistic initial assumption: we expect to be healthy. The first
    // refresh will correct this if either pool comes back empty.
    let mut last_healthy = true;

    loop {
        if !sleep_for.is_zero() {
            tokio::time::sleep(sleep_for).await;
        }

        let (gpu_result, cpu_result) = refresh(&pool, &config).await;
        let healthy = gpu_result.has_addrs() && cpu_result.has_addrs();

        if healthy != last_healthy {
            log_state_transition(last_healthy, healthy, &pool);
        }

        sleep_for = compute_next_interval(sleep_for, last_healthy, healthy, config.dns_refresh);
        last_healthy = healthy;
    }
}

async fn refresh(pool: &ArcSwap<PoolSnapshot>, config: &Config) -> (ResolveResult, ResolveResult) {
    let (gpu_result, cpu_result) = tokio::join!(
        resolve(&config.gpu_dns, 8081),
        resolve(&config.cpu_dns, 8081),
    );

    tracing::debug!(
        gpu_resolved = describe_resolve(&gpu_result),
        cpu_resolved = describe_resolve(&cpu_result),
        "DNS refresh"
    );

    let current = pool.load();
    let new_snapshot = merge(&current, &gpu_result, &cpu_result);
    pool.store(Arc::new(new_snapshot));

    (gpu_result, cpu_result)
}

async fn resolve(dns_name: &str, port: u16) -> ResolveResult {
    let host = format!("{dns_name}:{port}");
    // Bind to a local before matching so `host`'s lifetime covers the
    // iterator returned by `lookup_host` (it borrows the input string).
    let result = tokio::net::lookup_host(host.as_str()).await;
    match result {
        Ok(addrs) => ResolveResult::Resolved(addrs.collect()),
        Err(e) => {
            tracing::warn!(dns_name, err = %e, "DNS lookup failed");
            ResolveResult::Failed
        }
    }
}

/// Format a [`ResolveResult`] as a short string for tracing event fields.
fn describe_resolve(result: &ResolveResult) -> String {
    match result {
        ResolveResult::Resolved(addrs) => format!("ok({})", addrs.len()),
        ResolveResult::Failed => "failed".to_owned(),
    }
}

/// Merge newly-resolved addresses with the existing snapshot.
fn merge(
    current: &PoolSnapshot,
    gpu_result: &ResolveResult,
    cpu_result: &ResolveResult,
) -> PoolSnapshot {
    let gpu = merge_pool(&current.gpu, gpu_result, PoolType::Gpu);
    let cpu = merge_pool(&current.cpu, cpu_result, PoolType::Cpu);
    PoolSnapshot {
        gpu,
        cpu,
        updated_at: Instant::now(),
    }
}

/// Apply a resolution result to a single pool.
///
/// - [`ResolveResult::Failed`]: return the existing pool unchanged. The health
///   poller will continue probing the addresses; truly-dead ones will be
///   marked `Fail` and excluded from routing within one health cycle.
/// - [`ResolveResult::Resolved`]: rebuild the pool from the resolved address
///   set. Addresses already in the pool keep their current health state;
///   newly-discovered addresses enter as [`UpstreamStatus::Unknown`];
///   disappeared addresses are dropped.
fn merge_pool(
    existing: &[UpstreamInfo],
    result: &ResolveResult,
    pool_type: PoolType,
) -> Vec<UpstreamInfo> {
    match result {
        ResolveResult::Failed => existing.to_vec(),
        ResolveResult::Resolved(resolved) => resolved
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
            .collect(),
    }
}

/// Compute the duration the discovery loop should sleep before the next
/// refresh attempt.
///
/// - `previous` — the sleep duration used before the iteration that just
///   completed (zero on the very first call).
/// - `was_healthy` — whether the previous iteration considered the pool state
///   healthy.
/// - `is_healthy` — whether the iteration that just completed considered it
///   healthy.
/// - `max` — the configured `dns_refresh` interval; also the ceiling for the
///   backoff schedule.
///
/// State transitions:
/// - `is_healthy`: sleep `max` (steady state).
/// - `was_healthy && !is_healthy`: drop to [`INITIAL_RETRY_INTERVAL`] (fast retry).
/// - `!was_healthy && !is_healthy`: double `previous`, capped at `max`.
pub(crate) fn compute_next_interval(
    previous: Duration,
    was_healthy: bool,
    is_healthy: bool,
    max: Duration,
) -> Duration {
    // `INITIAL_RETRY_INTERVAL` exceeds `max` only when an operator has set a
    // very short `BGE_ROUTER_DNS_REFRESH_SECS` (<2 s). Cap defensively.
    let initial = INITIAL_RETRY_INTERVAL.min(max);
    if is_healthy {
        max
    } else if was_healthy {
        initial
    } else {
        previous.saturating_mul(2).min(max).max(initial)
    }
}

/// Emit an INFO event whenever the discovery loop crosses the healthy
/// boundary so operators can correlate routing 503s with pool state changes.
fn log_state_transition(was_healthy: bool, is_healthy: bool, pool: &ArcSwap<PoolSnapshot>) {
    let snapshot = pool.load();
    if is_healthy {
        tracing::info!(
            target: "bge_router::upstream::discovery",
            gpu_upstreams = snapshot.gpu.len(),
            cpu_upstreams = snapshot.cpu.len(),
            "DNS discovery recovered: both pools populated"
        );
    } else if was_healthy {
        tracing::warn!(
            target: "bge_router::upstream::discovery",
            gpu_upstreams = snapshot.gpu.len(),
            cpu_upstreams = snapshot.cpu.len(),
            "DNS discovery degraded: at least one pool empty or unresolved; \
             entering fast-retry backoff"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use super::{compute_next_interval, merge_pool, ResolveResult, INITIAL_RETRY_INTERVAL};
    use crate::upstream::snapshot::{PoolType, UpstreamInfo, UpstreamStatus};

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("test address must parse")
    }

    fn upstream(s: &str, status: UpstreamStatus, queue_depth: u32) -> UpstreamInfo {
        UpstreamInfo {
            addr: addr(s),
            pool_type: PoolType::Gpu,
            status,
            queue_depth,
            live_workers: 4,
            last_seen: Instant::now(),
        }
    }

    // ── merge_pool: ResolveResult::Failed preserves last-known-good ────────

    #[test]
    fn merge_pool_failed_preserves_existing_pool() {
        let existing = vec![
            upstream("10.0.0.1:8081", UpstreamStatus::Ok, 3),
            upstream("10.0.0.2:8081", UpstreamStatus::Ok, 1),
        ];
        let result = merge_pool(&existing, &ResolveResult::Failed, PoolType::Gpu);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].addr, addr("10.0.0.1:8081"));
        assert_eq!(result[0].status, UpstreamStatus::Ok);
        assert_eq!(result[0].queue_depth, 3);
        assert_eq!(result[1].addr, addr("10.0.0.2:8081"));
    }

    #[test]
    fn merge_pool_failed_on_empty_existing_stays_empty() {
        let result = merge_pool(&[], &ResolveResult::Failed, PoolType::Gpu);
        assert!(result.is_empty());
    }

    // ── merge_pool: ResolveResult::Resolved(empty) clears the pool ─────────

    #[test]
    fn merge_pool_resolved_empty_clears_existing_pool() {
        // Scale-to-zero: DNS resolved successfully but returned no addresses.
        // The pool MUST be cleared so we don't keep routing to dead instances.
        let existing = vec![upstream("10.0.0.1:8081", UpstreamStatus::Ok, 0)];
        let result = merge_pool(&existing, &ResolveResult::Resolved(vec![]), PoolType::Gpu);
        assert!(result.is_empty());
    }

    // ── merge_pool: ResolveResult::Resolved(some) merges as before ─────────

    #[test]
    fn merge_pool_resolved_preserves_health_for_existing_addresses() {
        let existing = vec![upstream("10.0.0.1:8081", UpstreamStatus::Ok, 5)];
        let result = merge_pool(
            &existing,
            &ResolveResult::Resolved(vec![addr("10.0.0.1:8081")]),
            PoolType::Gpu,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, UpstreamStatus::Ok);
        assert_eq!(result[0].queue_depth, 5);
    }

    #[test]
    fn merge_pool_resolved_adds_new_addresses_as_unknown() {
        let result = merge_pool(
            &[],
            &ResolveResult::Resolved(vec![addr("10.0.0.5:8081")]),
            PoolType::Cpu,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, UpstreamStatus::Unknown);
        assert_eq!(result[0].pool_type, PoolType::Cpu);
    }

    #[test]
    fn merge_pool_resolved_drops_disappeared_addresses() {
        let existing = vec![
            upstream("10.0.0.1:8081", UpstreamStatus::Ok, 0),
            upstream("10.0.0.2:8081", UpstreamStatus::Ok, 0),
        ];
        let result = merge_pool(
            &existing,
            &ResolveResult::Resolved(vec![addr("10.0.0.1:8081")]),
            PoolType::Gpu,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].addr, addr("10.0.0.1:8081"));
    }

    // ── ResolveResult::has_addrs ───────────────────────────────────────────

    #[test]
    fn resolve_result_has_addrs_semantics() {
        assert!(!ResolveResult::Failed.has_addrs());
        assert!(!ResolveResult::Resolved(vec![]).has_addrs());
        assert!(ResolveResult::Resolved(vec![addr("10.0.0.1:8081")]).has_addrs());
    }

    // ── compute_next_interval: state machine ───────────────────────────────

    const MAX: Duration = Duration::from_secs(30);

    #[test]
    fn next_interval_healthy_returns_max() {
        // is_healthy=true always returns max regardless of prior state.
        assert_eq!(compute_next_interval(Duration::ZERO, true, true, MAX), MAX);
        assert_eq!(compute_next_interval(Duration::ZERO, false, true, MAX), MAX);
        assert_eq!(compute_next_interval(MAX, false, true, MAX), MAX);
    }

    #[test]
    fn next_interval_transition_to_unhealthy_uses_initial() {
        // Healthy → unhealthy resets the backoff schedule to INITIAL.
        let next = compute_next_interval(MAX, true, false, MAX);
        assert_eq!(next, INITIAL_RETRY_INTERVAL);
    }

    #[test]
    fn next_interval_continuing_unhealthy_doubles() {
        let mut sleep = INITIAL_RETRY_INTERVAL;
        sleep = compute_next_interval(sleep, false, false, MAX);
        assert_eq!(sleep, Duration::from_secs(4));
        sleep = compute_next_interval(sleep, false, false, MAX);
        assert_eq!(sleep, Duration::from_secs(8));
        sleep = compute_next_interval(sleep, false, false, MAX);
        assert_eq!(sleep, Duration::from_secs(16));
        sleep = compute_next_interval(sleep, false, false, MAX);
        // Capped at MAX (30s), not 32s.
        assert_eq!(sleep, MAX);
        sleep = compute_next_interval(sleep, false, false, MAX);
        // Stays at MAX once capped.
        assert_eq!(sleep, MAX);
    }

    #[test]
    fn next_interval_cold_start_first_iteration() {
        // Cold start: previous=0, was_healthy=true (optimistic), is_healthy=false.
        // The "was_healthy → !is_healthy" branch fires, dropping to INITIAL.
        let next = compute_next_interval(Duration::ZERO, true, false, MAX);
        assert_eq!(next, INITIAL_RETRY_INTERVAL);
    }

    #[test]
    fn next_interval_handles_max_below_initial() {
        // Operator set BGE_ROUTER_DNS_REFRESH_SECS=1, smaller than INITIAL=2s.
        // Backoff must not exceed the configured max.
        let tiny_max = Duration::from_secs(1);
        let next = compute_next_interval(Duration::ZERO, true, false, tiny_max);
        assert!(next <= tiny_max);
        let next = compute_next_interval(tiny_max, false, false, tiny_max);
        assert_eq!(next, tiny_max);
    }

    #[test]
    fn next_interval_re_failure_after_recovery_resets_to_initial() {
        // Trace: unhealthy backoff up to MAX, then recovery, then re-failure.
        // The re-failure must drop us back to fast retry, not stay at MAX.
        let healthy = compute_next_interval(MAX, false, true, MAX);
        assert_eq!(healthy, MAX);
        let re_failed = compute_next_interval(healthy, true, false, MAX);
        assert_eq!(re_failed, INITIAL_RETRY_INTERVAL);
    }
}
