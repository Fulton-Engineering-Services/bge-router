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

/// URL scheme used when contacting upstream bge-m3 instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamScheme {
    /// Plain HTTP — default when `BGE_ROUTER_UPSTREAM_TLS` is not set.
    Http,
    /// HTTPS — used when `BGE_ROUTER_UPSTREAM_TLS=1`.
    Https,
}

impl UpstreamScheme {
    /// Returns the scheme string (`"http"` or `"https"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

impl std::fmt::Display for UpstreamScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use super::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamScheme, UpstreamStatus};

    // ── UpstreamScheme ─────────────────────────────────────────────────────

    #[test]
    fn upstream_scheme_http_as_str() {
        assert_eq!(UpstreamScheme::Http.as_str(), "http");
    }

    #[test]
    fn upstream_scheme_https_as_str() {
        assert_eq!(UpstreamScheme::Https.as_str(), "https");
    }

    #[test]
    fn upstream_scheme_display() {
        assert_eq!(format!("{}", UpstreamScheme::Http), "http");
        assert_eq!(format!("{}", UpstreamScheme::Https), "https");
    }

    // ── PoolSnapshot ────────────────────────────────────────────────────────

    #[test]
    fn default_snapshot_has_empty_pools() {
        let snap = PoolSnapshot::default();
        assert!(snap.gpu.is_empty());
        assert!(snap.cpu.is_empty());
    }

    // ── PoolType ────────────────────────────────────────────────────────────

    #[test]
    fn pool_type_gpu_as_str() {
        assert_eq!(PoolType::Gpu.as_str(), "gpu");
    }

    #[test]
    fn pool_type_cpu_as_str() {
        assert_eq!(PoolType::Cpu.as_str(), "cpu");
    }

    #[test]
    fn pool_type_eq() {
        assert_eq!(PoolType::Gpu, PoolType::Gpu);
        assert_eq!(PoolType::Cpu, PoolType::Cpu);
        assert_ne!(PoolType::Gpu, PoolType::Cpu);
    }

    #[test]
    fn pool_type_copy() {
        let t = PoolType::Gpu;
        // PoolType is Copy — assignment should not move t.
        let copy = t;
        assert_eq!(t, copy);
    }

    // ── UpstreamStatus::parse ───────────────────────────────────────────────

    #[test]
    fn status_ok_parses_to_ok() {
        assert_eq!(UpstreamStatus::parse("ok"), UpstreamStatus::Ok);
    }

    #[test]
    fn status_warn_parses_to_ok() {
        // "warn" means some workers exited but service still accepts requests.
        assert_eq!(UpstreamStatus::parse("warn"), UpstreamStatus::Ok);
    }

    #[test]
    fn status_loading_parses_to_loading() {
        assert_eq!(UpstreamStatus::parse("loading"), UpstreamStatus::Loading);
    }

    #[test]
    fn status_idle_parses_to_loading() {
        // "idle" means models were unloaded after timeout; treated same as loading.
        assert_eq!(UpstreamStatus::parse("idle"), UpstreamStatus::Loading);
    }

    #[test]
    fn status_fail_parses_to_fail() {
        assert_eq!(UpstreamStatus::parse("fail"), UpstreamStatus::Fail);
    }

    #[test]
    fn status_unknown_string_parses_to_unknown() {
        assert_eq!(UpstreamStatus::parse("unknown"), UpstreamStatus::Unknown);
        assert_eq!(UpstreamStatus::parse(""), UpstreamStatus::Unknown);
        assert_eq!(UpstreamStatus::parse("FAIL"), UpstreamStatus::Unknown);
        assert_eq!(UpstreamStatus::parse("OK"), UpstreamStatus::Unknown);
    }

    // ── UpstreamInfo construction ───────────────────────────────────────────

    #[test]
    fn upstream_info_fields_are_stored_correctly() {
        let addr: SocketAddr = "10.0.0.1:8081".parse().unwrap();
        let info = UpstreamInfo {
            addr,
            pool_type: PoolType::Gpu,
            status: UpstreamStatus::Ok,
            queue_depth: 5,
            live_workers: 8,
            last_seen: Instant::now(),
        };
        assert_eq!(info.addr, addr);
        assert_eq!(info.pool_type, PoolType::Gpu);
        assert_eq!(info.status, UpstreamStatus::Ok);
        assert_eq!(info.queue_depth, 5);
        assert_eq!(info.live_workers, 8);
    }

    #[test]
    fn upstream_info_clones_correctly() {
        let addr: SocketAddr = "10.0.0.2:8081".parse().unwrap();
        let original = UpstreamInfo {
            addr,
            pool_type: PoolType::Cpu,
            status: UpstreamStatus::Loading,
            queue_depth: 0,
            live_workers: 2,
            last_seen: Instant::now(),
        };
        let cloned = original.clone();
        assert_eq!(cloned.addr, original.addr);
        assert_eq!(cloned.pool_type, original.pool_type);
        assert_eq!(cloned.status, original.status);
        assert_eq!(cloned.queue_depth, original.queue_depth);
        assert_eq!(cloned.live_workers, original.live_workers);
    }
}
