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

//! Per-route fallback dispatch.
//!
//! Inference routes (`/v1/*embeddings*`) use a **hedged race**: GPU first;
//! after `hedge_delay`, fire the CPU upstream in parallel; first non-5xx
//! response wins; the loser's future is dropped, which cancels its in-flight
//! `reqwest` call and closes the upstream connection so the GPU can stop
//! computing the abandoned request.
//!
//! Control-plane routes (`/health`, `/v1/models`, etc.) use a **sequential
//! GPU→CPU fallback** with a hard timeout per upstream — fast failure
//! detection matters more than masking GPU latency.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    http::{HeaderMap, Method},
    response::Response,
};
use bytes::Bytes;

use crate::error::AppError;
use crate::headers::collect_x_headers;
use crate::router::{policy, proxy, route_policy::RoutePolicy};
use crate::state::AppState;
use crate::upstream::snapshot::{PoolType, UpstreamScheme};

#[cfg(test)]
mod tests;

/// Convert a `Duration` into milliseconds clamped at `u64::MAX`.
///
/// Used solely for log-attribute formatting; control flow does not depend
/// on the saturated value.
fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Buffered request data required to forward to either upstream.
///
/// Bundles the parameters that `proxy::forward` needs so we can pass a
/// single reference into the hedged-race helper without exceeding clippy's
/// `too_many_arguments` ceiling.
struct ForwardCtx<'a> {
    scheme: UpstreamScheme,
    method: &'a Method,
    path_and_query: &'a str,
    headers: &'a HeaderMap,
    body: Bytes,
}

/// Route a request through the appropriate upstream with the policy chosen
/// by [`RoutePolicy::for_path`].
///
/// # Errors
///
/// Returns [`AppError::NoUpstreamAvailable`] when no healthy upstream can be
/// reached, or the underlying upstream error if both pools fail.
pub async fn route(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    match RoutePolicy::for_path(path_and_query, &state.config) {
        RoutePolicy::Hedged { delay } => {
            hedged_race(state, delay, method, path_and_query, headers, body).await
        }
        RoutePolicy::SequentialTimeout { per_upstream } => {
            sequential_timeout(state, per_upstream, method, path_and_query, headers, body).await
        }
    }
}

// ── Hedged race (inference) ─────────────────────────────────────────────────

async fn hedged_race(
    state: &AppState,
    hedge_delay: Duration,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let snapshot = state.pool.load_full();
    let gpu_candidate = policy::pick_gpu(&snapshot);
    let cpu_candidate = policy::pick_cpu(&snapshot);

    // Collect X-* headers once; serialize to JSON for log events (None when empty).
    let x_headers = collect_x_headers(&headers);
    let x_headers_json: Option<String> = if x_headers.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&x_headers).unwrap_or_default())
    };

    let ctx = ForwardCtx {
        scheme: state.upstream_scheme(),
        method: &method,
        path_and_query,
        headers: &headers,
        body,
    };

    match (gpu_candidate, cpu_candidate) {
        (None, None) => Err(AppError::NoUpstreamAvailable),
        // No GPU — go straight to CPU with no hedge.
        (None, Some((cpu_addr, _))) => {
            let result = forward(&state.client, cpu_addr, PoolType::Cpu, &ctx).await;
            if let Ok(ref resp) = result {
                log_direct(
                    path_and_query,
                    cpu_addr,
                    "cpu",
                    resp.status().as_u16(),
                    x_headers_json.as_deref(),
                );
            }
            result
        }
        // No CPU — GPU only, no hedge to fire against.
        (Some((gpu_addr, _)), None) => {
            let result = forward(&state.client, gpu_addr, PoolType::Gpu, &ctx).await;
            if let Ok(ref resp) = result {
                log_direct(
                    path_and_query,
                    gpu_addr,
                    "gpu",
                    resp.status().as_u16(),
                    x_headers_json.as_deref(),
                );
            }
            result
        }
        (Some((gpu_addr, _)), Some((cpu_addr, _))) => {
            run_race(
                state,
                hedge_delay,
                gpu_addr,
                cpu_addr,
                &ctx,
                x_headers_json.as_deref(),
            )
            .await
        }
    }
}

async fn forward(
    client: &reqwest::Client,
    addr: SocketAddr,
    pool_type: PoolType,
    ctx: &ForwardCtx<'_>,
) -> Result<Response, AppError> {
    proxy::forward(
        client,
        ctx.scheme,
        addr,
        pool_type,
        ctx.method,
        ctx.path_and_query,
        ctx.headers,
        ctx.body.clone(),
    )
    .await
}

async fn run_race(
    state: &AppState,
    hedge_delay: Duration,
    gpu_addr: SocketAddr,
    cpu_addr: SocketAddr,
    ctx: &ForwardCtx<'_>,
    x_headers_json: Option<&str>,
) -> Result<Response, AppError> {
    let start = Instant::now();
    let cpu_started = Arc::new(AtomicBool::new(false));

    let gpu_fut = forward(&state.client, gpu_addr, PoolType::Gpu, ctx);

    // CPU fires only after hedge_delay.  Capture by reference; the async
    // block lives no longer than this stack frame.
    let cpu_started_ref = cpu_started.clone();
    let cpu_fut = async move {
        tokio::time::sleep(hedge_delay).await;
        cpu_started_ref.store(true, Ordering::SeqCst);
        tracing::info!(
            target: "bge_router::router::hedge",
            path = %ctx.path_and_query,
            hedge_delay_ms = ms(hedge_delay),
            gpu_upstream = %gpu_addr,
            cpu_upstream = %cpu_addr,
            "hedge: firing CPU race"
        );
        forward(&state.client, cpu_addr, PoolType::Cpu, ctx).await
    };

    tokio::pin!(gpu_fut);
    tokio::pin!(cpu_fut);

    select_winner(
        &mut gpu_fut,
        &mut cpu_fut,
        &cpu_started,
        start,
        ctx.path_and_query,
        gpu_addr,
        cpu_addr,
        x_headers_json,
    )
    .await
}

/// Drive the two pinned futures with `tokio::select!`, returning the first
/// non-5xx response or the GPU failure when both attempts lose.
#[allow(clippy::too_many_arguments)]
async fn select_winner(
    gpu_fut: &mut std::pin::Pin<&mut impl Future<Output = Result<Response, AppError>>>,
    cpu_fut: &mut std::pin::Pin<&mut impl Future<Output = Result<Response, AppError>>>,
    cpu_started: &AtomicBool,
    start: Instant,
    path_and_query: &str,
    gpu_addr: SocketAddr,
    cpu_addr: SocketAddr,
    x_headers_json: Option<&str>,
) -> Result<Response, AppError> {
    let mut gpu_done = false;
    let mut cpu_done = false;
    let mut gpu_failure: Option<Result<Response, AppError>> = None;
    let mut cpu_failure: Option<Result<Response, AppError>> = None;

    while !(gpu_done && cpu_done) {
        tokio::select! {
            // Bias toward GPU so a successful GPU return polled in the same
            // tick as a CPU completion is preferred (preserves GPU-primary
            // semantics).
            biased;
            result = &mut *gpu_fut, if !gpu_done => {
                gpu_done = true;
                if is_winner(&result) {
                    let loser_status = if cpu_done {
                        "errored"
                    } else if cpu_started.load(Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "not_started"
                    };
                    log_winner("GPU", path_and_query, start.elapsed(), gpu_addr, cpu_addr, loser_status, x_headers_json);
                    return result;
                }
                log_loser_attempt("GPU", gpu_addr, &result, start.elapsed());
                gpu_failure = Some(result);
            }
            result = &mut *cpu_fut, if !cpu_done => {
                cpu_done = true;
                if is_winner(&result) {
                    let loser_status = if gpu_done { "errored" } else { "cancelled" };
                    log_winner("CPU", path_and_query, start.elapsed(), gpu_addr, cpu_addr, loser_status, x_headers_json);
                    return result;
                }
                log_loser_attempt("CPU", cpu_addr, &result, start.elapsed());
                cpu_failure = Some(result);
            }
        }
    }

    // Both attempts failed.  Return the GPU outcome to preserve the prior
    // sequential semantics (clients that were already mapping reqwest errors
    // to GPU-side failures don't see a behaviour change).
    tracing::warn!(
        target: "bge_router::router::hedge",
        path = %path_and_query,
        gpu_upstream = %gpu_addr,
        cpu_upstream = %cpu_addr,
        "hedge: both failed"
    );
    gpu_failure
        .or(cpu_failure)
        .unwrap_or(Err(AppError::NoUpstreamAvailable))
}

fn log_winner(
    pool: &str,
    path_and_query: &str,
    elapsed: Duration,
    gpu_addr: SocketAddr,
    cpu_addr: SocketAddr,
    loser_status: &'static str,
    x_headers_json: Option<&str>,
) {
    if let Some(xh) = x_headers_json {
        tracing::info!(
            target: "bge_router::router::hedge",
            path = %path_and_query,
            winner_latency_ms = ms(elapsed),
            gpu_upstream = %gpu_addr,
            cpu_upstream = %cpu_addr,
            loser_status = loser_status,
            x_headers = xh,
            "hedge: {pool} won"
        );
    } else {
        tracing::info!(
            target: "bge_router::router::hedge",
            path = %path_and_query,
            winner_latency_ms = ms(elapsed),
            gpu_upstream = %gpu_addr,
            cpu_upstream = %cpu_addr,
            loser_status = loser_status,
            "hedge: {pool} won"
        );
    }
}

/// Log a direct (single-pool, no race) inference route completion.
fn log_direct(
    path_and_query: &str,
    addr: SocketAddr,
    pool: &str,
    status: u16,
    x_headers_json: Option<&str>,
) {
    if let Some(xh) = x_headers_json {
        tracing::info!(
            target: "bge_router::router::hedge",
            path = %path_and_query,
            upstream = %addr,
            pool = pool,
            status = status,
            x_headers = xh,
            "direct: {pool} response"
        );
    } else {
        tracing::info!(
            target: "bge_router::router::hedge",
            path = %path_and_query,
            upstream = %addr,
            pool = pool,
            status = status,
            "direct: {pool} response"
        );
    }
}

/// A "winner" is any non-5xx response — matches the existing semantics of
/// `proxy::forward` returning `Ok(resp)` and the previous code's
/// `!resp.status().is_server_error()` check.  4xx is a deterministic client
/// error and should not trigger fallback either.
fn is_winner(result: &Result<Response, AppError>) -> bool {
    matches!(result, Ok(resp) if !resp.status().is_server_error())
}

fn log_loser_attempt(
    pool: &str,
    addr: SocketAddr,
    result: &Result<Response, AppError>,
    elapsed: Duration,
) {
    match result {
        Ok(resp) => {
            tracing::warn!(
                target: "bge_router::router::hedge",
                upstream = %addr,
                pool = pool,
                status = resp.status().as_u16(),
                elapsed_ms = ms(elapsed),
                "{pool} upstream returned 5xx in race"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "bge_router::router::hedge",
                upstream = %addr,
                pool = pool,
                err = %e,
                elapsed_ms = ms(elapsed),
                "{pool} upstream errored in race"
            );
        }
    }
}

// ── Sequential timeout (control plane) ──────────────────────────────────────

async fn sequential_timeout(
    state: &AppState,
    per_upstream: Duration,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let snapshot = state.pool.load_full();
    let gpu_candidate = policy::pick_gpu(&snapshot);
    let cpu_candidate = policy::pick_cpu(&snapshot);
    let scheme = state.upstream_scheme();

    if let Some((gpu_addr, _)) = gpu_candidate {
        let result = tokio::time::timeout(
            per_upstream,
            proxy::forward(
                &state.client,
                scheme,
                gpu_addr,
                PoolType::Gpu,
                &method,
                path_and_query,
                &headers,
                body.clone(),
            ),
        )
        .await;

        match result {
            Ok(Ok(resp)) if !resp.status().is_server_error() => return Ok(resp),
            Ok(Ok(resp)) => {
                tracing::warn!(
                    upstream = %gpu_addr,
                    status = resp.status().as_u16(),
                    "GPU upstream returned 5xx, attempting CPU fallback"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    upstream = %gpu_addr,
                    err = %e,
                    "GPU upstream error, attempting CPU fallback"
                );
            }
            Err(_) => {
                tracing::warn!(
                    upstream = %gpu_addr,
                    budget_ms = ms(per_upstream),
                    "GPU upstream timed out within fallback budget, attempting CPU fallback"
                );
            }
        }

        if let Some((cpu_addr, _)) = cpu_candidate {
            return forward_cpu_with_timeout(
                state,
                per_upstream,
                cpu_addr,
                &method,
                path_and_query,
                &headers,
                body,
            )
            .await;
        }
    } else if let Some((cpu_addr, _)) = cpu_candidate {
        return forward_cpu_with_timeout(
            state,
            per_upstream,
            cpu_addr,
            &method,
            path_and_query,
            &headers,
            body,
        )
        .await;
    }

    Err(AppError::NoUpstreamAvailable)
}

async fn forward_cpu_with_timeout(
    state: &AppState,
    per_upstream: Duration,
    cpu_addr: SocketAddr,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let result = tokio::time::timeout(
        per_upstream,
        proxy::forward(
            &state.client,
            state.upstream_scheme(),
            cpu_addr,
            PoolType::Cpu,
            method,
            path_and_query,
            headers,
            body,
        ),
    )
    .await;

    if let Ok(inner) = result {
        inner
    } else {
        tracing::warn!(
            upstream = %cpu_addr,
            budget_ms = ms(per_upstream),
            "CPU upstream timed out within control-plane budget"
        );
        Err(AppError::NoUpstreamAvailable)
    }
}
