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

//! Axum request handler for all proxied embedding endpoints.
//!
//! Buffers the request body (required for fallback retry), then delegates to
//! the fallback router which tries GPU first and CPU as a fallback.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;

use crate::router::fallback;
use crate::state::AppState;

/// Handle all embedding proxy requests (`/v1/embeddings`, `/v1/sparse-embeddings`, etc.).
///
/// This handler is registered on the wildcard route and proxies every inbound
/// request to the best available upstream after buffering the body.
pub async fn handle_proxy(
    State(state): State<AppState>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().path().to_owned(), |pq| pq.as_str().to_owned());
    let method = req.method().clone();
    let headers = req.headers().clone();

    // Buffer the body so we can retry on CPU if the GPU upstream fails.
    // Limit to 32 MiB; larger requests are rejected with 413.
    let body = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            tracing::error!(err = %e, "failed to read request body");
            return (StatusCode::BAD_REQUEST, "failed to read request body").into_response();
        }
    };

    match fallback::route(&state, method, &path_and_query, headers, body).await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(err = %e, path = %path_and_query, "routing failed");
            e.into_response()
        }
    }
}
