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

//! Immutable snapshot of the upstream pool state.
//!
//! [`PoolSnapshot`] is replaced atomically via [`arc_swap::ArcSwap`] whenever
//! DNS discovery or health polling updates the state. Readers pay only an
//! atomic load; no locks are held during routing policy evaluation.

use std::net::SocketAddr;
use std::time::Instant;

/// Which class of hardware the upstream runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolType {
    /// NVIDIA GPU worker (CUDA/TensorRT execution provider).
    Gpu,
    /// CPU worker (MLAS execution provider).
    Cpu,
}

impl PoolType {
    /// Returns a lowercase string label suitable for response headers and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }
}

/// Health status of a single upstream, derived from its `/health` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    /// Upstream is healthy and serving requests (`status=ok` or `status=warn`).
    Ok,
    /// Upstream is initialising (`status=loading` or `status=idle`).
    Loading,
    /// All workers have exited (`status=fail`).
    Fail,
    /// No health response received yet, or the response could not be parsed.
    Unknown,
}

impl UpstreamStatus {
    /// Parse a `status` field from the bge-m3 `/health` JSON response.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            // "warn" = some workers exited but service is still accepting requests.
            "ok" | "warn" => Self::Ok,
            "loading" | "idle" => Self::Loading,
            "fail" => Self::Fail,
            _ => Self::Unknown,
        }
    }
}

/// All known information about a single upstream instance.
#[derive(Debug, Clone)]
pub struct UpstreamInfo {
    /// TCP address of the upstream (`ip:8081`).
    pub addr: SocketAddr,
    /// Whether this upstream is in the GPU or CPU pool.
    pub pool_type: PoolType,
    /// Last observed health status.
    pub status: UpstreamStatus,
    /// Number of requests queued on the upstream's internal semaphore.
    pub queue_depth: u32,
    /// Number of live worker threads on the upstream.
    pub live_workers: u32,
    /// Monotonic timestamp of the last successful health poll.
    pub last_seen: Instant,
}

/// Immutable snapshot of both upstream pools at a point in time.
///
/// Replaced atomically on every DNS refresh or health-poll cycle.
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    /// All GPU upstreams discovered via `BGE_ROUTER_GPU_DNS`.
    pub gpu: Vec<UpstreamInfo>,
    /// All CPU upstreams discovered via `BGE_ROUTER_CPU_DNS`.
    pub cpu: Vec<UpstreamInfo>,
    /// Monotonic timestamp when this snapshot was created.
    pub updated_at: Instant,
}

impl Default for PoolSnapshot {
    fn default() -> Self {
        Self {
            gpu: Vec::new(),
            cpu: Vec::new(),
            updated_at: Instant::now(),
        }
    }
}
