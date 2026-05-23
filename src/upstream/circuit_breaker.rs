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

//! Per-upstream circuit-breaker cooldown mutator.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::upstream::snapshot::{PoolSnapshot, UpstreamInfo};

/// Mark an upstream as tripped by setting `cooldown_until = now + duration`.
///
/// The update is idempotent: if the upstream is already cooling down it is
/// left unchanged so parallel callers do not keep extending cooldown windows.
pub fn trip(pool: &ArcSwap<PoolSnapshot>, addr: SocketAddr, duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let until = Instant::now() + duration;
    pool.rcu(|snapshot| {
        let mut next = snapshot.as_ref().clone();
        let mut changed = false;
        changed |= trip_pool(&mut next.gpu, addr, until);
        changed |= trip_pool(&mut next.cpu, addr, until);
        if changed {
            next.updated_at = Instant::now();
        }
        Arc::new(next)
    });
}

fn trip_pool(pool: &mut [UpstreamInfo], addr: SocketAddr, until: Instant) -> bool {
    if let Some(upstream) = pool.iter_mut().find(|u| u.addr == addr) {
        if upstream.is_in_cooldown(Instant::now()) {
            return false;
        }
        upstream.cooldown_until = Some(until);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use arc_swap::ArcSwap;

    use super::trip;
    use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

    fn upstream(addr: &str, pool_type: PoolType) -> UpstreamInfo {
        UpstreamInfo {
            addr: addr.parse::<SocketAddr>().unwrap(),
            pool_type,
            status: UpstreamStatus::Ok,
            queue_depth: 0,
            live_workers: 1,
            last_seen: Instant::now(),
            cooldown_until: None,
        }
    }

    #[test]
    fn trip_sets_cooldown_for_matching_upstream() {
        let snapshot = PoolSnapshot {
            gpu: vec![upstream("10.0.0.1:8081", PoolType::Gpu)],
            cpu: vec![],
            updated_at: Instant::now(),
        };
        let pool = ArcSwap::from_pointee(snapshot);
        trip(
            &pool,
            "10.0.0.1:8081".parse().unwrap(),
            Duration::from_secs(30),
        );
        let current = pool.load();
        assert!(
            current.gpu[0].cooldown_until.is_some(),
            "cooldown should be set"
        );
    }

    #[test]
    fn trip_is_idempotent_when_already_in_cooldown() {
        let snapshot = PoolSnapshot {
            gpu: vec![upstream("10.0.0.1:8081", PoolType::Gpu)],
            cpu: vec![],
            updated_at: Instant::now(),
        };
        let pool = ArcSwap::from_pointee(snapshot);
        let addr: SocketAddr = "10.0.0.1:8081".parse().unwrap();
        trip(&pool, addr, Duration::from_secs(30));
        let first = pool.load().gpu[0].cooldown_until;
        trip(&pool, addr, Duration::from_mins(2));
        let second = pool.load().gpu[0].cooldown_until;
        assert_eq!(
            first, second,
            "second trip should not extend existing cooldown"
        );
    }

    #[tokio::test]
    async fn concurrent_trip_calls_set_same_cooldown_window() {
        let snapshot = PoolSnapshot {
            gpu: vec![upstream("10.0.0.1:8081", PoolType::Gpu)],
            cpu: vec![],
            updated_at: Instant::now(),
        };
        let pool = Arc::new(ArcSwap::from_pointee(snapshot));
        let addr: SocketAddr = "10.0.0.1:8081".parse().unwrap();
        let p1 = pool.clone();
        let p2 = pool.clone();
        let ((), ()) = tokio::join!(
            async move { trip(&p1, addr, Duration::from_secs(30)) },
            async move { trip(&p2, addr, Duration::from_secs(30)) }
        );
        let current = pool.load();
        assert!(
            current.gpu[0].cooldown_until.is_some(),
            "cooldown should be present after concurrent trips"
        );
    }
}
