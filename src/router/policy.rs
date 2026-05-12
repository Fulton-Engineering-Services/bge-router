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
//! 3. `None` — return 503.

use std::net::SocketAddr;

use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamStatus};

/// Pick the best available upstream from the given snapshot.
///
/// Returns `(addr, pool_type)` or `None` if no healthy upstream is available.
#[must_use]
pub fn pick(snapshot: &PoolSnapshot) -> Option<(SocketAddr, PoolType)> {
    pick_from_pool(&snapshot.gpu, PoolType::Gpu)
        .or_else(|| pick_from_pool(&snapshot.cpu, PoolType::Cpu))
}

/// Pick the best GPU upstream, or `None` if no healthy GPU is available.
#[must_use]
pub fn pick_gpu(snapshot: &PoolSnapshot) -> Option<(SocketAddr, PoolType)> {
    pick_from_pool(&snapshot.gpu, PoolType::Gpu)
}

/// Pick the best CPU upstream, or `None` if no healthy CPU is available.
#[must_use]
pub fn pick_cpu(snapshot: &PoolSnapshot) -> Option<(SocketAddr, PoolType)> {
    pick_from_pool(&snapshot.cpu, PoolType::Cpu)
}

fn pick_from_pool(
    pool: &[crate::upstream::snapshot::UpstreamInfo],
    pool_type: PoolType,
) -> Option<(SocketAddr, PoolType)> {
    pool.iter()
        .filter(|u| u.status == UpstreamStatus::Ok)
        .min_by_key(|u| u.queue_depth)
        .map(|u| (u.addr, pool_type))
}

#[cfg(test)]
mod tests;
