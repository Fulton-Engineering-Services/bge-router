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

//! Router health endpoint (`GET /router/health`).
//!
//! Returns the router's own view of the upstream pool — statuses, queue depths,
//! and live worker counts for every discovered upstream.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;
use std::time::Instant;

use crate::state::AppState;
use crate::upstream::snapshot::{PoolType, UpstreamStatus};

/// `GET /router/health` — router's own health and upstream pool snapshot.
pub async fn router_health(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.pool.load_full();
    let now = Instant::now();

    let gpu: Vec<_> = snapshot
        .gpu
        .iter()
        .map(|u| UpstreamView::from_info(u, now))
        .collect();
    let cpu: Vec<_> = snapshot
        .cpu
        .iter()
        .map(|u| UpstreamView::from_info(u, now))
        .collect();

    let gpu_ok = gpu.iter().filter(|u| u.status == "ok").count();
    let cpu_ok = cpu.iter().filter(|u| u.status == "ok").count();

    let status = if gpu_ok > 0 || cpu_ok > 0 {
        "ok"
    } else {
        "degraded"
    };
    let http_status = if gpu_ok == 0 && cpu_ok == 0 {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (
        http_status,
        Json(RouterHealthResponse {
            status,
            gpu_upstreams: gpu,
            cpu_upstreams: cpu,
        }),
    )
}

#[derive(Serialize)]
struct RouterHealthResponse {
    status: &'static str,
    gpu_upstreams: Vec<UpstreamView>,
    cpu_upstreams: Vec<UpstreamView>,
}

#[derive(Serialize)]
struct UpstreamView {
    addr: String,
    pool_type: &'static str,
    status: &'static str,
    queue_depth: u32,
    live_workers: u32,
    last_seen_secs_ago: f64,
}

impl UpstreamView {
    fn from_info(info: &crate::upstream::snapshot::UpstreamInfo, now: Instant) -> Self {
        Self {
            addr: info.addr.to_string(),
            pool_type: match info.pool_type {
                PoolType::Gpu => "gpu",
                PoolType::Cpu => "cpu",
            },
            status: match info.status {
                UpstreamStatus::Ok => "ok",
                UpstreamStatus::Loading => "loading",
                UpstreamStatus::Fail => "fail",
                UpstreamStatus::Unknown => "unknown",
            },
            queue_depth: info.queue_depth,
            live_workers: info.live_workers,
            last_seen_secs_ago: now.duration_since(info.last_seen).as_secs_f64(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::state::AppState;
    use crate::upstream::snapshot::{PoolSnapshot, PoolType, UpstreamInfo, UpstreamStatus};

    fn test_config() -> Config {
        Config {
            bind: "0.0.0.0:8081".to_string(),
            gpu_dns: "gpu.test.internal".to_string(),
            cpu_dns: "cpu.test.internal".to_string(),
            dns_refresh: Duration::from_secs(30),
            health_poll: Duration::from_secs(5),
            fallback_budget: Duration::from_secs(1),
            heartbeat: Duration::from_secs(60),
        }
    }

    fn ok_gpu_upstream(addr: &str) -> UpstreamInfo {
        UpstreamInfo {
            addr: addr.parse::<SocketAddr>().expect("test addr must be valid"),
            pool_type: PoolType::Gpu,
            status: UpstreamStatus::Ok,
            queue_depth: 0,
            live_workers: 8,
            last_seen: Instant::now(),
        }
    }

    #[tokio::test]
    async fn empty_pool_returns_503_with_degraded_status() {
        let state = AppState::new(test_config());
        let app = crate::bootstrap::router::build(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/router/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "degraded");
        assert!(body["gpu_upstreams"].is_array());
        assert!(body["cpu_upstreams"].is_array());
        assert_eq!(body["gpu_upstreams"].as_array().unwrap().len(), 0);
        assert_eq!(body["cpu_upstreams"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn ok_gpu_upstream_returns_200_with_ok_status() {
        let state = AppState::new(test_config());
        let snapshot = PoolSnapshot {
            gpu: vec![ok_gpu_upstream("10.0.0.1:8081")],
            cpu: vec![],
            updated_at: Instant::now(),
        };
        state.pool.store(Arc::new(snapshot));

        let app = crate::bootstrap::router::build(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/router/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "ok");
        assert!(body["gpu_upstreams"].is_array());
        assert!(body["cpu_upstreams"].is_array());
        assert_eq!(body["gpu_upstreams"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn response_body_is_valid_json_with_required_fields() {
        let state = AppState::new(test_config());
        let app = crate::bootstrap::router::build(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/router/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response body must be valid JSON");
        assert!(body.get("status").is_some(), "body must contain 'status'");
        assert!(
            body.get("gpu_upstreams").is_some(),
            "body must contain 'gpu_upstreams'"
        );
        assert!(
            body.get("cpu_upstreams").is_some(),
            "body must contain 'cpu_upstreams'"
        );
    }
}
