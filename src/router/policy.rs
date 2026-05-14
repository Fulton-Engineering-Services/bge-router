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

//! Routing policy: given a [`PoolSnapshot`], pick the best upstream.
//!
//! Priority order:
//! 1. GPU upstreams with `status=Ok`, lowest `queue_depth` first.
//! 2. CPU upstreams with `status=Ok`, lowest `queue_depth` first.
//! 3. GPU upstreams with `status=Loading` and `live_workers > 0` (idle/reloading),
//!    lowest `queue_depth` first.
//! 4. CPU upstreams with `status=Loading` and `live_workers > 0`, lowest
//!    `queue_depth` first.
//! 5. `None` — return 503.
//!
//! The loading fallback (tiers 3–4) handles the case where all upstreams are
//! idle (`status=idle` from bge-m3, mapped to `Loading` here). An idle
//! upstream accepts requests and reloads its models on demand; routing to it
//! breaks the deadlock where the router never sends traffic so the upstream
//! never reloads. Upstreams with `live_workers == 0` (actively starting, not
//! yet able to process) and `Fail`/`Unknown` upstreams are never selected.

use std::net::SocketAddr;

use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

/// Pick the best available upstream from the given snapshot.
///
/// Returns `(addr, pool_type)` or `None` if no routable upstream is available.
#[must_use]
pub fn pick(snapshot: &PoolSnapshot) -> Option<(SocketAddr, PoolType)> {
    pick_ok(&snapshot.gpu, PoolType::Gpu)
        .or_else(|| pick_ok(&snapshot.cpu, PoolType::Cpu))
        .or_else(|| pick_loading(&snapshot.gpu, PoolType::Gpu))
        .or_else(|| pick_loading(&snapshot.cpu, PoolType::Cpu))
}

/// Pick the best GPU upstream, or `None` if no routable GPU is available.
#[must_use]
pub fn pick_gpu(snapshot: &PoolSnapshot) -> Option<(SocketAddr, PoolType)> {
    pick_ok(&snapshot.gpu, PoolType::Gpu).or_else(|| pick_loading(&snapshot.gpu, PoolType::Gpu))
}

/// Pick the best CPU upstream, or `None` if no routable CPU is available.
#[must_use]
pub fn pick_cpu(snapshot: &PoolSnapshot) -> Option<(SocketAddr, PoolType)> {
    pick_ok(&snapshot.cpu, PoolType::Cpu).or_else(|| pick_loading(&snapshot.cpu, PoolType::Cpu))
}

fn pick_ok(pool: &[UpstreamInfo], pool_type: PoolType) -> Option<(SocketAddr, PoolType)> {
    pool.iter()
        .filter(|u| u.status == UpstreamStatus::Ok)
        .min_by_key(|u| u.queue_depth)
        .map(|u| (u.addr, pool_type))
}

/// Pick the least-loaded `Loading` upstream that has live workers.
///
/// `live_workers > 0` means the upstream process is running and will accept
/// requests (it will reload its models on the first request). Upstreams with
/// `live_workers == 0` are still initialising and cannot serve anything yet.
fn pick_loading(pool: &[UpstreamInfo], pool_type: PoolType) -> Option<(SocketAddr, PoolType)> {
    pool.iter()
        .filter(|u| u.status == UpstreamStatus::Loading && u.live_workers > 0)
        .min_by_key(|u| u.queue_depth)
        .map(|u| (u.addr, pool_type))
}

#[cfg(test)]
mod tests;
