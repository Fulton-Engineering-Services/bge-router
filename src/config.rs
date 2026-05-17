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

//! Environment-variable driven configuration for `bge-router`.
//!
//! All variables are optional with sensible defaults. Unknown variables are
//! silently ignored; invalid values produce errors at startup.

use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Default GPU→CPU hedge delay for inference routes (5 seconds).
const DEFAULT_HEDGE_DELAY_MS: u64 = 5_000;
/// Default per-upstream timeout for control-plane routes (1 second).
const DEFAULT_CONTROL_TIMEOUT_MS: u64 = 1_000;

/// Resolved runtime configuration derived from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP address to bind the HTTP server (`BGE_ROUTER_BIND`, default `0.0.0.0:8081`).
    pub bind: String,
    /// DNS name to resolve for GPU upstreams (`BGE_ROUTER_GPU_DNS`).
    pub gpu_dns: String,
    /// DNS name to resolve for CPU upstreams (`BGE_ROUTER_CPU_DNS`).
    pub cpu_dns: String,
    /// How often to re-resolve both DNS names (`BGE_ROUTER_DNS_REFRESH_SECS`, default 30).
    pub dns_refresh: Duration,
    /// How often to poll each upstream's `/health` endpoint (`BGE_ROUTER_HEALTH_POLL_SECS`,
    /// default 5).
    pub health_poll: Duration,
    /// Delay before firing the parallel CPU race for inference routes
    /// (`BGE_ROUTER_HEDGE_DELAY_MS`, default 5000).  Only applies to
    /// `/v1/*embeddings*` paths.
    pub hedge_delay: Duration,
    /// Per-upstream hard timeout for control-plane routes (`/health`, `/v1/models`,
    /// etc.) — `BGE_ROUTER_CONTROL_TIMEOUT_MS`, default 1000.
    pub control_timeout: Duration,
    /// `true` when the deployment set the deprecated `BGE_ROUTER_FALLBACK_BUDGET_MS`
    /// env var.  When set without the new vars, it is honoured as the default for
    /// `hedge_delay` for safer migration; a one-time WARN is logged at startup.
    pub legacy_fallback_budget_set: bool,
    /// Interval between periodic heartbeat log events (`BGE_ROUTER_HEARTBEAT_SECS`, default 60).
    /// Set to `0` to disable heartbeats.
    pub heartbeat: Duration,
    /// Path to the TLS certificate PEM for the inbound listener.
    /// Env: `BGE_ROUTER_TLS_CERT_PATH`. When set together with `tls_key_path`, the
    /// listener binds HTTPS (requires the `tls` Cargo feature).
    pub tls_cert_path: Option<std::path::PathBuf>,
    /// Path to the TLS private-key PEM for the inbound listener.
    /// Env: `BGE_ROUTER_TLS_KEY_PATH`.
    pub tls_key_path: Option<std::path::PathBuf>,
    /// Path to a CA-bundle PEM to trust for upstream (bge-m3) connections.
    /// Env: `BGE_ROUTER_UPSTREAM_CA_BUNDLE`. When set, reqwest validates upstream
    /// TLS using this bundle and all upstream URLs are built with `https://`.
    pub upstream_ca_bundle: Option<std::path::PathBuf>,
}

impl Config {
    /// Load configuration from process environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error if any numeric variable is present but cannot be parsed
    /// or if a `*_MS` budget that must be positive is set to `0`.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Load configuration from a caller-supplied lookup function.
    ///
    /// Tests use this to inject a deterministic environment without mutating
    /// the process-global `std::env` state.
    ///
    /// # Errors
    ///
    /// See [`Config::from_env`].
    pub fn from_lookup<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let bind = lookup("BGE_ROUTER_BIND").unwrap_or_else(|| "0.0.0.0:8081".into());
        let gpu_dns = lookup("BGE_ROUTER_GPU_DNS").unwrap_or_else(|| "bge-m3-gpu".into());
        let cpu_dns = lookup("BGE_ROUTER_CPU_DNS").unwrap_or_else(|| "bge-m3-cpu".into());

        let dns_refresh_secs = parse_u64("BGE_ROUTER_DNS_REFRESH_SECS", 30, &lookup)?;
        let health_poll_secs = parse_u64("BGE_ROUTER_HEALTH_POLL_SECS", 5, &lookup)?;
        let heartbeat_secs = parse_u64("BGE_ROUTER_HEARTBEAT_SECS", 60, &lookup)?;

        let legacy_raw = parse_optional_u64("BGE_ROUTER_FALLBACK_BUDGET_MS", &lookup)?;
        let legacy_fallback_budget_set = legacy_raw.is_some();

        // Hedge delay defaults to legacy var when present (back-compat), else
        // DEFAULT_HEDGE_DELAY_MS.  An explicit BGE_ROUTER_HEDGE_DELAY_MS always wins.
        let hedge_delay_ms = match parse_optional_u64("BGE_ROUTER_HEDGE_DELAY_MS", &lookup)? {
            Some(v) => v,
            None => legacy_raw.unwrap_or(DEFAULT_HEDGE_DELAY_MS),
        };
        if hedge_delay_ms == 0 {
            bail!("invalid BGE_ROUTER_HEDGE_DELAY_MS: must be > 0 (got 0)");
        }

        let control_timeout_ms = parse_u64(
            "BGE_ROUTER_CONTROL_TIMEOUT_MS",
            DEFAULT_CONTROL_TIMEOUT_MS,
            &lookup,
        )?;
        if control_timeout_ms == 0 {
            bail!("invalid BGE_ROUTER_CONTROL_TIMEOUT_MS: must be > 0 (got 0)");
        }

        Ok(Self {
            bind,
            gpu_dns,
            cpu_dns,
            dns_refresh: Duration::from_secs(dns_refresh_secs),
            health_poll: Duration::from_secs(health_poll_secs),
            hedge_delay: Duration::from_millis(hedge_delay_ms),
            control_timeout: Duration::from_millis(control_timeout_ms),
            legacy_fallback_budget_set,
            heartbeat: Duration::from_secs(heartbeat_secs),
            tls_cert_path: lookup("BGE_ROUTER_TLS_CERT_PATH").map(std::path::PathBuf::from),
            tls_key_path: lookup("BGE_ROUTER_TLS_KEY_PATH").map(std::path::PathBuf::from),
            upstream_ca_bundle: lookup("BGE_ROUTER_UPSTREAM_CA_BUNDLE")
                .map(std::path::PathBuf::from),
        })
    }
}

fn parse_u64<F>(key: &str, default: u64, lookup: &F) -> Result<u64>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        Some(val) => val
            .parse::<u64>()
            .with_context(|| format!("invalid {key}: expected a non-negative integer")),
        None => Ok(default),
    }
}

fn parse_optional_u64<F>(key: &str, lookup: &F) -> Result<Option<u64>>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        Some(val) => val
            .parse::<u64>()
            .map(Some)
            .with_context(|| format!("invalid {key}: expected a non-negative integer")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
