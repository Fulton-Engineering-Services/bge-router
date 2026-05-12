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

use std::net::SocketAddr;
use std::time::Instant;

use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

use super::{pick, pick_cpu, pick_gpu};

fn make_upstream(
    addr: &str,
    pool_type: PoolType,
    status: UpstreamStatus,
    depth: u32,
) -> UpstreamInfo {
    UpstreamInfo {
        addr: addr.parse::<SocketAddr>().expect("test addr must be valid"),
        pool_type,
        status,
        queue_depth: depth,
        live_workers: 4,
        last_seen: Instant::now(),
    }
}

fn snapshot(gpu: Vec<UpstreamInfo>, cpu: Vec<UpstreamInfo>) -> PoolSnapshot {
    PoolSnapshot {
        gpu,
        cpu,
        updated_at: Instant::now(),
    }
}

// ── empty pool ──────────────────────────────────────────────────────────────

#[test]
fn empty_snapshot_returns_none() {
    let snap = PoolSnapshot::default();
    assert!(pick(&snap).is_none());
    assert!(pick_gpu(&snap).is_none());
    assert!(pick_cpu(&snap).is_none());
}

// ── GPU-only pool ────────────────────────────────────────────────────────────

#[test]
fn single_ok_gpu_with_zero_depth_is_picked() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![gpu], vec![]);
    let (addr, pool) = pick(&snap).expect("should pick the GPU upstream");
    assert_eq!(addr, "10.0.0.1:8081".parse::<SocketAddr>().unwrap());
    assert_eq!(pool, PoolType::Gpu);
}

#[test]
fn single_ok_gpu_with_nonzero_depth_is_still_picked() {
    // queue_depth > 0 does not disqualify an upstream — it is only used as a
    // tiebreaker among upstreams of the same pool type.
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Ok, 5);
    let snap = snapshot(vec![gpu], vec![]);
    let result = pick(&snap);
    assert!(
        result.is_some(),
        "Ok GPU with queue_depth=5 should still be picked"
    );
    assert_eq!(result.unwrap().1, PoolType::Gpu);
}

// ── CPU-only pool ────────────────────────────────────────────────────────────

#[test]
fn single_ok_cpu_is_picked_when_no_gpu_exists() {
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![], vec![cpu]);
    let (addr, pool) = pick(&snap).expect("should pick the CPU upstream");
    assert_eq!(addr, "10.0.1.1:8081".parse::<SocketAddr>().unwrap());
    assert_eq!(pool, PoolType::Cpu);
}

// ── GPU preferred over CPU ────────────────────────────────────────────────────

#[test]
fn ok_gpu_beats_ok_cpu_with_same_depth() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Ok, 0);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![gpu], vec![cpu]);
    let (_, pool) = pick(&snap).expect("should pick an upstream");
    assert_eq!(pool, PoolType::Gpu, "GPU should be preferred over CPU");
}

#[test]
fn ok_gpu_with_higher_depth_still_beats_ok_cpu_with_zero_depth() {
    // Policy: GPU is always tried first. If any GPU is Ok, it wins regardless
    // of queue_depth comparisons across pool types.
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Ok, 3);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![gpu], vec![cpu]);
    let (_, pool) = pick(&snap).expect("should pick an upstream");
    assert_eq!(pool, PoolType::Gpu, "GPU should win even with higher depth");
}

// ── Fallback to CPU ────────────────────────────────────────────────────────────

#[test]
fn fail_gpu_falls_through_to_ok_cpu() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Fail, 0);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![gpu], vec![cpu]);
    let (_, pool) = pick(&snap).expect("should fall back to CPU");
    assert_eq!(
        pool,
        PoolType::Cpu,
        "Failed GPU should cause fallback to CPU"
    );
}

#[test]
fn loading_gpu_falls_through_to_ok_cpu() {
    // Loading is not Ok — a loading upstream is filtered out by pick_from_pool.
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Loading, 0);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![gpu], vec![cpu]);
    let (_, pool) = pick(&snap).expect("should fall back to CPU");
    assert_eq!(
        pool,
        PoolType::Cpu,
        "Loading GPU should cause fallback to CPU"
    );
}

#[test]
fn unknown_gpu_falls_through_to_ok_cpu() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Unknown, 0);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 2);
    let snap = snapshot(vec![gpu], vec![cpu]);
    let (_, pool) = pick(&snap).expect("should fall back to CPU");
    assert_eq!(pool, PoolType::Cpu);
}

// ── Tiebreaking by queue depth ─────────────────────────────────────────────────

#[test]
fn picks_lowest_depth_among_multiple_ok_gpus() {
    let gpu_busy = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Ok, 10);
    let gpu_idle = make_upstream("10.0.0.2:8081", PoolType::Gpu, UpstreamStatus::Ok, 1);
    let gpu_med = make_upstream("10.0.0.3:8081", PoolType::Gpu, UpstreamStatus::Ok, 5);
    let snap = snapshot(vec![gpu_busy, gpu_idle, gpu_med], vec![]);
    let (addr, pool) = pick(&snap).expect("should pick the least-busy GPU");
    assert_eq!(pool, PoolType::Gpu);
    assert_eq!(
        addr,
        "10.0.0.2:8081".parse::<SocketAddr>().unwrap(),
        "should pick depth=1"
    );
}

#[test]
fn picks_lowest_depth_among_multiple_ok_cpus() {
    let cpu_busy = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 8);
    let cpu_idle = make_upstream("10.0.1.2:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let cpu_med = make_upstream("10.0.1.3:8081", PoolType::Cpu, UpstreamStatus::Ok, 4);
    let snap = snapshot(vec![], vec![cpu_busy, cpu_idle, cpu_med]);
    let (addr, pool) = pick(&snap).expect("should pick the least-busy CPU");
    assert_eq!(pool, PoolType::Cpu);
    assert_eq!(
        addr,
        "10.0.1.2:8081".parse::<SocketAddr>().unwrap(),
        "should pick depth=0"
    );
}

// ── All unavailable ─────────────────────────────────────────────────────────────

#[test]
fn all_fail_returns_none() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Fail, 0);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Fail, 0);
    let snap = snapshot(vec![gpu], vec![cpu]);
    assert!(pick(&snap).is_none());
}

#[test]
fn all_unknown_returns_none() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Unknown, 0);
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Unknown, 0);
    let snap = snapshot(vec![gpu], vec![cpu]);
    assert!(pick(&snap).is_none());
}

// ── pick_gpu / pick_cpu helpers ─────────────────────────────────────────────────

#[test]
fn pick_gpu_ignores_cpu_pool() {
    let cpu = make_upstream("10.0.1.1:8081", PoolType::Cpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![], vec![cpu]);
    assert!(
        pick_gpu(&snap).is_none(),
        "pick_gpu should not look at cpu pool"
    );
}

#[test]
fn pick_cpu_ignores_gpu_pool() {
    let gpu = make_upstream("10.0.0.1:8081", PoolType::Gpu, UpstreamStatus::Ok, 0);
    let snap = snapshot(vec![gpu], vec![]);
    assert!(
        pick_cpu(&snap).is_none(),
        "pick_cpu should not look at gpu pool"
    );
}
