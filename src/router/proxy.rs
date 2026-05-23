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

use arc_swap::ArcSwap;
use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::Response,
};
use bytes::Bytes;

use crate::config::RetryConfig;
use crate::error::AppError;
use crate::upstream::circuit_breaker;
use crate::upstream::snapshot::PoolSnapshot;
use crate::upstream::snapshot::{PoolType, UpstreamScheme};

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

/// Build an upstream URL from scheme, address, and path.
fn upstream_url(scheme: UpstreamScheme, addr: SocketAddr, path_and_query: &str) -> String {
    format!("{scheme}://{addr}{path_and_query}")
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
    scheme: UpstreamScheme,
    addr: SocketAddr,
    pool_type: PoolType,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let url = upstream_url(scheme, addr, path_and_query);

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

/// Forward with retry/backoff and circuit-breaker cooldown on exhaustion.
///
/// A retryable failure is either an upstream transport error or an HTTP 5xx
/// status code. 4xx responses are returned immediately.
///
/// ## Design note: retry and trip responsibility
///
/// `forward_with_retry` owns both the retry orchestration and the
/// [`crate::upstream::circuit_breaker::trip`] side-effect. This colocation
/// keeps all retry state local and avoids a second pass through the result in
/// the callsite. The `pool` parameter exists solely for the trip side-effect;
/// it is not used for routing. Callers that need to distinguish "forwarding
/// failed" from "upstream was tripped" can inspect the snapshot after the call.
///
/// # Errors
///
/// Returns the last transport error or HTTP response outcome from [`forward`]
/// after retries are exhausted.
#[allow(clippy::too_many_arguments)]
pub async fn forward_with_retry(
    client: &reqwest::Client,
    pool: &ArcSwap<PoolSnapshot>,
    retry_cfg: RetryConfig,
    scheme: UpstreamScheme,
    addr: SocketAddr,
    pool_type: PoolType,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let mut backoff = retry_cfg.initial_backoff;
    for attempt in 0..=retry_cfg.max_retries {
        let result = forward(
            client,
            scheme,
            addr,
            pool_type,
            method,
            path_and_query,
            headers,
            body.clone(),
        )
        .await;
        let is_retryable = match &result {
            Ok(resp) => resp.status().is_server_error(),
            Err(_) => true,
        };
        if !is_retryable {
            return result;
        }
        if attempt == retry_cfg.max_retries {
            circuit_breaker::trip(pool, addr, retry_cfg.cooldown);
            tracing::warn!(
                upstream = %addr,
                pool = pool_type.as_str(),
                attempts = retry_cfg.max_retries + 1,
                cooldown_secs = retry_cfg.cooldown.as_secs(),
                "upstream retries exhausted; tripping cooldown"
            );
            return result;
        }

        tracing::info!(
            upstream = %addr,
            pool = pool_type.as_str(),
            attempt = attempt + 1,
            max_retries = retry_cfg.max_retries,
            backoff_ms = backoff.as_millis(),
            "upstream 5xx/error, retrying"
        );
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(retry_cfg.max_backoff);
    }

    Err(AppError::NoUpstreamAvailable)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::{UpstreamScheme, upstream_url};

    #[test]
    fn upstream_url_http_scheme() {
        let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        assert_eq!(
            upstream_url(UpstreamScheme::Http, addr, "/health"),
            "http://127.0.0.1:8081/health"
        );
    }

    #[test]
    fn upstream_url_https_scheme() {
        let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        assert_eq!(
            upstream_url(UpstreamScheme::Https, addr, "/health"),
            "https://127.0.0.1:8081/health"
        );
    }

    #[test]
    fn upstream_url_preserves_path_and_query() {
        let addr: SocketAddr = "10.0.0.1:8081".parse().unwrap();
        assert_eq!(
            upstream_url(UpstreamScheme::Http, addr, "/v1/embeddings?foo=bar"),
            "http://10.0.0.1:8081/v1/embeddings?foo=bar"
        );
    }
}
