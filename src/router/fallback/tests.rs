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

//! Hedged-race and sequential-timeout integration tests.
//!
//! Each test spins up two in-process axum mock upstreams on `127.0.0.1:0`
//! and exercises [`super::route`] end-to-end through real reqwest calls.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use bytes::Bytes;
use tokio::net::TcpListener;

use super::route;
use crate::config::Config;
use crate::state::AppState;
use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

// Initialise a test-writer tracing subscriber once per process so that
// `--no-capture` runs print real `hedge: ...` log lines.  Without this the
// events are emitted to a no-op subscriber and are invisible.
static INIT_TRACING: Once = Once::new();
fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::INFO)
            .with_target(true)
            .try_init();
    });
}

// ── Mock upstream helpers ───────────────────────────────────────────────────

/// State shared between the mock-upstream axum handler and the test that
/// inspects request/cancellation counters.
#[derive(Clone)]
struct MockState {
    response_status: u16,
    delay: Duration,
    received: Arc<AtomicU32>,
    completed: Arc<AtomicU32>,
    cancelled: Arc<AtomicU32>,
}

struct MockHandle {
    addr: SocketAddr,
    received: Arc<AtomicU32>,
    completed: Arc<AtomicU32>,
    cancelled: Arc<AtomicU32>,
}

/// Drop-guard that increments either `completed` or `cancelled` based on
/// whether the handler reached its post-sleep success path before the future
/// was dropped.  Reqwest closes the TCP connection when its request future is
/// dropped; axum then drops the handler future, firing this guard.
struct CancelGuard {
    done: bool,
    completed: Arc<AtomicU32>,
    cancelled: Arc<AtomicU32>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.done {
            self.completed.fetch_add(1, Ordering::SeqCst);
        } else {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

async fn mock_handler(State(s): State<MockState>) -> impl IntoResponse {
    s.received.fetch_add(1, Ordering::SeqCst);
    let mut guard = CancelGuard {
        done: false,
        completed: s.completed.clone(),
        cancelled: s.cancelled.clone(),
    };
    tokio::time::sleep(s.delay).await;
    guard.done = true;
    let status = StatusCode::from_u16(s.response_status).expect("test status code must be valid");
    (status, "ok")
}

async fn spawn_mock(response_status: u16, delay: Duration) -> MockHandle {
    let received = Arc::new(AtomicU32::new(0));
    let completed = Arc::new(AtomicU32::new(0));
    let cancelled = Arc::new(AtomicU32::new(0));

    let state = MockState {
        response_status,
        delay,
        received: received.clone(),
        completed: completed.clone(),
        cancelled: cancelled.clone(),
    };

    let app = Router::new().fallback(any(mock_handler)).with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream addr");

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    MockHandle {
        addr,
        received,
        completed,
        cancelled,
    }
}

fn config_with(hedge_delay: Duration, control_timeout: Duration) -> Config {
    Config::from_lookup(|key| match key {
        "BGE_ROUTER_HEDGE_DELAY_MS" => Some(hedge_delay.as_millis().to_string()),
        "BGE_ROUTER_CONTROL_TIMEOUT_MS" => Some(control_timeout.as_millis().to_string()),
        _ => None,
    })
    .expect("test config must build")
}

fn ok_upstream(addr: SocketAddr, pool_type: PoolType) -> UpstreamInfo {
    UpstreamInfo {
        addr,
        pool_type,
        status: UpstreamStatus::Ok,
        queue_depth: 0,
        live_workers: 1,
        last_seen: Instant::now(),
    }
}

fn state_with(
    gpu: Option<SocketAddr>,
    cpu: Option<SocketAddr>,
    hedge_delay: Duration,
    control_timeout: Duration,
) -> AppState {
    let state =
        AppState::new(config_with(hedge_delay, control_timeout)).expect("test state must build");
    let snapshot = PoolSnapshot {
        gpu: gpu
            .map(|a| vec![ok_upstream(a, PoolType::Gpu)])
            .unwrap_or_default(),
        cpu: cpu
            .map(|a| vec![ok_upstream(a, PoolType::Cpu)])
            .unwrap_or_default(),
        updated_at: Instant::now(),
    };
    state.pool.store(Arc::new(snapshot));
    state
}

// ── Hedged race (inference paths) ───────────────────────────────────────────

#[tokio::test]
async fn hedged_race_fast_cpu_wins_and_gpu_is_cancelled() {
    init_tracing();
    // GPU is glacial (200 ms response); CPU is fast (50 ms after the
    // hedge delay).  Hedge fires at 20 ms → CPU is racing GPU.  CPU wins
    // around t = 70 ms; GPU's future is dropped at that point, which closes
    // its TCP connection, which fires the GPU mock's CancelGuard.
    let gpu = spawn_mock(200, Duration::from_millis(200)).await;
    let cpu = spawn_mock(200, Duration::from_millis(50)).await;
    let state = state_with(
        Some(gpu.addr),
        Some(cpu.addr),
        Duration::from_millis(20),
        Duration::from_secs(1),
    );

    let resp = route(
        &state,
        Method::POST,
        "/v1/embeddings",
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
    )
    .await
    .expect("CPU should win the race");
    assert_eq!(resp.status(), StatusCode::OK);

    // Give the GPU mock time to observe the dropped connection.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        gpu.received.load(Ordering::SeqCst),
        1,
        "GPU should have received the request first"
    );
    assert_eq!(
        cpu.received.load(Ordering::SeqCst),
        1,
        "CPU should have been raced after hedge delay"
    );
    assert_eq!(
        gpu.completed.load(Ordering::SeqCst),
        0,
        "GPU should NOT have completed — its future was dropped"
    );
    assert_eq!(
        gpu.cancelled.load(Ordering::SeqCst),
        1,
        "GPU handler should have been cancelled when reqwest dropped"
    );
    assert_eq!(cpu.completed.load(Ordering::SeqCst), 1, "CPU completed");
}

#[tokio::test]
async fn hedge_delay_not_elapsed_means_cpu_is_never_fired() {
    // GPU returns in 30 ms; hedge delay is 500 ms.  CPU must never be hit.
    let gpu = spawn_mock(200, Duration::from_millis(30)).await;
    let cpu = spawn_mock(200, Duration::from_millis(10)).await;
    let state = state_with(
        Some(gpu.addr),
        Some(cpu.addr),
        Duration::from_millis(500),
        Duration::from_secs(1),
    );

    let resp = route(
        &state,
        Method::POST,
        "/v1/embeddings",
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
    )
    .await
    .expect("GPU should win before hedge delay");
    assert_eq!(resp.status(), StatusCode::OK);

    // Wait well past hedge delay to confirm CPU stays untouched.
    tokio::time::sleep(Duration::from_millis(700)).await;

    assert_eq!(gpu.received.load(Ordering::SeqCst), 1);
    assert_eq!(
        cpu.received.load(Ordering::SeqCst),
        0,
        "CPU upstream MUST NOT see a request when GPU returns within the hedge delay"
    );
}

#[tokio::test]
async fn hedged_race_both_fail_returns_gpu_error() {
    init_tracing();
    // Both upstreams return 500.  Hedged race should mark both as losers
    // and return the GPU outcome (preserves prior sequential semantics).
    let gpu = spawn_mock(500, Duration::from_millis(20)).await;
    let cpu = spawn_mock(500, Duration::from_millis(40)).await;
    let state = state_with(
        Some(gpu.addr),
        Some(cpu.addr),
        Duration::from_millis(10),
        Duration::from_secs(1),
    );

    let resp = route(
        &state,
        Method::POST,
        "/v1/embeddings",
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
    )
    .await
    .expect("both 5xx is still an upstream-Ok response, so route returns Ok(resp)");
    // The router returns the GPU's 5xx response when both fail.
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let pool_header = resp
        .headers()
        .get("x-bge-router-pool")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(
        pool_header.as_deref(),
        Some("gpu"),
        "GPU's response is the canonical failure to surface"
    );
}

// ── Sequential timeout (control-plane paths) ────────────────────────────────

#[tokio::test]
async fn control_plane_uses_per_upstream_timeout_and_falls_back_to_cpu() {
    init_tracing();
    // GPU exceeds the 100 ms control-plane budget; CPU is fast.  Sequential
    // path should time out the GPU, then return the CPU response.
    let gpu = spawn_mock(200, Duration::from_secs(5)).await;
    let cpu = spawn_mock(200, Duration::from_millis(20)).await;
    let state = state_with(
        Some(gpu.addr),
        Some(cpu.addr),
        Duration::from_secs(60), // hedge delay should not be touched here
        Duration::from_millis(100),
    );

    let start = Instant::now();
    let resp = route(
        &state,
        Method::GET,
        "/health",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .expect("CPU should answer after GPU control-plane timeout");
    let elapsed = start.elapsed();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        elapsed < Duration::from_millis(800),
        "control-plane fallback must not wait for the slow GPU response: took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(100),
        "GPU per-upstream timeout must elapse before CPU is tried: took {elapsed:?}"
    );

    let pool_header = resp
        .headers()
        .get("x-bge-router-pool")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(pool_header.as_deref(), Some("cpu"));
}

// ── Back-compat: legacy BGE_ROUTER_FALLBACK_BUDGET_MS ───────────────────────

#[test]
fn legacy_fallback_budget_seeds_hedge_delay() {
    let cfg = Config::from_lookup(|key| match key {
        "BGE_ROUTER_FALLBACK_BUDGET_MS" => Some("2750".to_string()),
        _ => None,
    })
    .expect("legacy var alone is valid");
    assert_eq!(cfg.hedge_delay, Duration::from_millis(2_750));
    assert!(cfg.legacy_fallback_budget_set);
    // Control plane MUST keep its short hard timeout.
    assert_eq!(cfg.control_timeout, Duration::from_secs(1));
}
