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

//! `bge-router` — transparent HTTP reverse proxy for BGE-M3 embedding upstreams.
//!
//! Routes requests between GPU and CPU upstream pools discovered via DNS (AWS
//! Cloud Map compatible). Prefers GPU when warm; falls back to CPU within a
//! configurable budget window.

// Module layout follows bge-m3-embedding-server conventions:
//   bootstrap/  — server and router construction
//   config      — env-var driven Config struct
//   error       — AppError and IntoResponse impl
//   handler/    — Axum request handlers
//   headers     — XHeaders newtype and collect_x_headers utility
//   logging     — tracing init; prepends "bge_module" to every JSON log line
//   metrics     — periodic heartbeat logger
//   router/     — routing policy, proxy, fallback
//   state       — shared AppState (pool snapshot + config + http client)
//   upstream/   — DNS discovery, health polling, pool snapshot

pub mod bootstrap;
pub mod config;
pub mod error;
pub mod handler;
pub mod headers;
pub mod logging;
pub mod metrics;
pub mod router;
pub mod state;
pub mod upstream;

use anyhow::Result;
use std::sync::Arc;

/// Start the bge-router: load config, spin up background tasks, serve HTTP.
///
/// # Errors
///
/// Returns an error if the config is invalid or the TCP listener fails to bind.
pub async fn run() -> Result<()> {
    let cfg = config::Config::from_env()?;
    let hedge_delay_ms = u64::try_from(cfg.hedge_delay.as_millis()).unwrap_or(u64::MAX);
    let control_timeout_ms = u64::try_from(cfg.control_timeout.as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        bind = %cfg.bind,
        gpu_dns = %cfg.gpu_dns,
        cpu_dns = %cfg.cpu_dns,
        dns_refresh_secs = cfg.dns_refresh.as_secs(),
        health_poll_secs = cfg.health_poll.as_secs(),
        hedge_delay_ms = hedge_delay_ms,
        control_timeout_ms = control_timeout_ms,
        "bge-router starting"
    );

    if cfg.legacy_fallback_budget_set {
        tracing::warn!(
            hedge_delay_ms = hedge_delay_ms,
            "BGE_ROUTER_FALLBACK_BUDGET_MS is deprecated; \
             new vars are BGE_ROUTER_HEDGE_DELAY_MS (inference, default 5000) \
             and BGE_ROUTER_CONTROL_TIMEOUT_MS (control plane, default 1000). \
             The legacy var has been honoured as the default for hedge_delay; \
             control_timeout was NOT seeded from it. Update your deployment."
        );
    }

    let app_state = state::AppState::new(cfg);
    let pool = Arc::clone(&app_state.pool);
    let cfg_ref = Arc::clone(&app_state.config);

    // Spawn background tasks: DNS discovery, health polling, heartbeat.
    upstream::discovery::spawn(Arc::clone(&pool), Arc::clone(&cfg_ref));
    upstream::health::spawn(
        Arc::clone(&pool),
        Arc::clone(&cfg_ref),
        app_state.client.clone(),
    );
    metrics::spawn(Arc::clone(&pool), Arc::clone(&cfg_ref));

    bootstrap::server::serve(app_state).await
}
