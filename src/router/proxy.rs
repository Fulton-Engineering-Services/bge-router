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

//! Zero-copy streaming reverse proxy.
//!
//! Forwards the buffered request to the selected upstream and streams the
//! response body back to the caller without intermediate buffering.
//! Hop-by-hop headers are stripped; observability headers are injected.

use std::net::SocketAddr;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::Response,
};
use bytes::Bytes;

use crate::error::AppError;
use crate::upstream::snapshot::PoolType;

/// Hop-by-hop headers that must not be forwarded to the upstream or the client.
static HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &HeaderName) -> bool {
    let lower = name.as_str();
    HOP_BY_HOP.contains(&lower)
}

/// Forward a buffered request to `addr` and return a streaming [`Response`].
///
/// # Errors
///
/// Returns [`AppError::Upstream`] if the upstream connection fails or returns
/// an unreadable response.
#[allow(clippy::too_many_arguments)]
pub async fn forward(
    client: &reqwest::Client,
    scheme: &str,
    addr: SocketAddr,
    pool_type: PoolType,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let url = format!("{scheme}://{addr}{path_and_query}");

    let mut builder = client
        .request(method.clone(), &url)
        .body(reqwest::Body::from(body));

    // Forward client headers, stripping hop-by-hop.
    for (name, value) in headers {
        if !is_hop_by_hop(name) {
            builder = builder.header(name, value);
        }
    }

    let upstream_resp = builder.send().await.map_err(AppError::Upstream)?;
    let status = upstream_resp.status();

    // Collect upstream response headers, strip hop-by-hop.
    let mut resp_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers() {
        if !is_hop_by_hop(name) {
            resp_headers.insert(name.clone(), value.clone());
        }
    }

    // Inject observability headers.
    if let Ok(v) = addr.to_string().parse() {
        resp_headers.insert("x-bge-router-upstream", v);
    }
    if let Ok(v) = pool_type.as_str().parse() {
        resp_headers.insert("x-bge-router-pool", v);
    }

    // Stream the response body without buffering.
    let body_stream = upstream_resp.bytes_stream();
    let axum_body = Body::from_stream(body_stream);

    let mut response = Response::new(axum_body);
    *response.status_mut() =
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    *response.headers_mut() = resp_headers;

    Ok(response)
}
