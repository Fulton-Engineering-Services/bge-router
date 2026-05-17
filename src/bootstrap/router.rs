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

//! Builds the Axum [`Router`] with all routes and middleware.

use axum::{Router, routing::get};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

use crate::{
    handler::{health::router_health, proxy::handle_proxy},
    state::AppState,
};

/// Construct the application [`Router`] with state and middleware.
pub fn build(state: AppState) -> Router {
    let x_request_id = axum::http::HeaderName::from_static("x-request-id");

    Router::new()
        .route("/router/health", get(router_health))
        // All other paths are proxied transparently to the upstream pool.
        .fallback(handle_proxy)
        .with_state(state)
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
}
