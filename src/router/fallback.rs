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

//! Fallback routing with a per-request time budget.
//!
//! Attempt order:
//! 1. GPU upstream (preferred).
//! 2. If GPU fails (connection error or 5xx) within `fallback_budget_ms` AND
//!    no response bytes have been streamed yet, try the best available CPU
//!    upstream.
//! 3. If no candidates remain, return 503.

use axum::{
    http::{HeaderMap, Method},
    response::Response,
};
use bytes::Bytes;

use crate::error::AppError;
use crate::router::{policy, proxy};
use crate::state::AppState;
use crate::upstream::snapshot::PoolType;

/// Route a request through the appropriate upstream with fallback support.
///
/// # Errors
///
/// Returns [`AppError::NoUpstreamAvailable`] when no healthy upstream can be
/// reached within the fallback budget.
pub async fn route(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let snapshot = state.pool.load_full();
    let gpu_candidate = policy::pick_gpu(&snapshot);
    let cpu_candidate = policy::pick_cpu(&snapshot);

    // Try GPU pool first when available.
    if let Some((addr, _)) = gpu_candidate {
        let result = tokio::time::timeout(
            state.config.fallback_budget,
            proxy::forward(
                &state.client,
                addr,
                PoolType::Gpu,
                &method,
                path_and_query,
                &headers,
                body.clone(),
            ),
        )
        .await;

        match result {
            // Successful non-5xx response from GPU — return immediately.
            Ok(Ok(resp)) if !resp.status().is_server_error() => {
                return Ok(resp);
            }
            // GPU returned 5xx — log and fall through to CPU.
            Ok(Ok(resp)) => {
                tracing::warn!(
                    upstream = %addr,
                    status = resp.status().as_u16(),
                    "GPU upstream returned 5xx, attempting CPU fallback"
                );
            }
            // GPU connection/request error — fall through to CPU.
            Ok(Err(e)) => {
                tracing::warn!(upstream = %addr, err = %e, "GPU upstream error, attempting CPU fallback");
            }
            // GPU timed out within fallback budget — fall through to CPU.
            Err(_) => {
                tracing::warn!(
                    upstream = %addr,
                    budget_ms = state.config.fallback_budget.as_millis(),
                    "GPU upstream timed out within fallback budget, attempting CPU fallback"
                );
            }
        }

        // GPU failed — attempt CPU fallback.
        if let Some((cpu_addr, _)) = cpu_candidate {
            return proxy::forward(
                &state.client,
                cpu_addr,
                PoolType::Cpu,
                &method,
                path_and_query,
                &headers,
                body,
            )
            .await;
        }
    } else if let Some((cpu_addr, _)) = cpu_candidate {
        // No GPU candidate, route directly to CPU.
        return proxy::forward(
            &state.client,
            cpu_addr,
            PoolType::Cpu,
            &method,
            path_and_query,
            &headers,
            body,
        )
        .await;
    }

    Err(AppError::NoUpstreamAvailable)
}
