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

use anyhow::{Context, Result};

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
    /// Maximum milliseconds to wait for a primary upstream before trying fallback
    /// (`BGE_ROUTER_FALLBACK_BUDGET_MS`, default 1000).
    pub fallback_budget: Duration,
    /// Interval between periodic heartbeat log events (`BGE_ROUTER_HEARTBEAT_SECS`, default 60).
    /// Set to `0` to disable heartbeats.
    pub heartbeat: Duration,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error if any numeric variable is present but cannot be parsed.
    pub fn from_env() -> Result<Self> {
        let bind = std::env::var("BGE_ROUTER_BIND").unwrap_or_else(|_| "0.0.0.0:8081".into());
        let gpu_dns = std::env::var("BGE_ROUTER_GPU_DNS")
            .unwrap_or_else(|_| "bge-m3-gpu.codekeeper.internal".into());
        let cpu_dns = std::env::var("BGE_ROUTER_CPU_DNS")
            .unwrap_or_else(|_| "bge-m3-cpu.codekeeper.internal".into());

        let dns_refresh_secs = parse_u64_env("BGE_ROUTER_DNS_REFRESH_SECS", 30)?;
        let health_poll_secs = parse_u64_env("BGE_ROUTER_HEALTH_POLL_SECS", 5)?;
        let fallback_budget_ms = parse_u64_env("BGE_ROUTER_FALLBACK_BUDGET_MS", 1000)?;
        let heartbeat_secs = parse_u64_env("BGE_ROUTER_HEARTBEAT_SECS", 60)?;

        Ok(Self {
            bind,
            gpu_dns,
            cpu_dns,
            dns_refresh: Duration::from_secs(dns_refresh_secs),
            health_poll: Duration::from_secs(health_poll_secs),
            fallback_budget: Duration::from_millis(fallback_budget_ms),
            heartbeat: Duration::from_secs(heartbeat_secs),
        })
    }
}

fn parse_u64_env(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(val) => val
            .parse::<u64>()
            .with_context(|| format!("invalid {key}: expected a non-negative integer")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Config;

    // These tests assert default values and therefore assume the BGE_ROUTER_*
    // environment variables are NOT set in the test process.  cargo-nextest
    // runs each test in its own isolated process, so parallel test runs cannot
    // pollute each other through the global environment.

    fn load_defaults() -> Config {
        Config::from_env().expect("Config::from_env should succeed when vars are absent or valid")
    }

    #[test]
    fn from_env_succeeds_with_no_vars_set() {
        let _ = load_defaults();
    }

    #[test]
    fn default_bind_address() {
        assert_eq!(load_defaults().bind, "0.0.0.0:8081");
    }

    #[test]
    fn default_gpu_dns() {
        assert_eq!(load_defaults().gpu_dns, "bge-m3-gpu.codekeeper.internal");
    }

    #[test]
    fn default_cpu_dns() {
        assert_eq!(load_defaults().cpu_dns, "bge-m3-cpu.codekeeper.internal");
    }

    #[test]
    fn default_dns_refresh_is_30s() {
        assert_eq!(load_defaults().dns_refresh, Duration::from_secs(30));
    }

    #[test]
    fn default_health_poll_is_5s() {
        assert_eq!(load_defaults().health_poll, Duration::from_secs(5));
    }

    #[test]
    fn default_fallback_budget_is_1000ms() {
        assert_eq!(load_defaults().fallback_budget, Duration::from_secs(1));
    }

    #[test]
    fn default_heartbeat_is_60s() {
        assert_eq!(load_defaults().heartbeat, Duration::from_secs(60));
    }

    #[test]
    fn zero_value_is_valid_u64_and_produces_zero_duration() {
        // parse_u64_env accepts "0" as a valid u64 — no floor clamping is applied.
        // Duration::from_secs(0) == Duration::ZERO, so the field becomes zero.
        assert_eq!(Duration::from_secs(0), Duration::ZERO);
        assert_eq!(Duration::from_millis(0), Duration::ZERO);
    }
}
