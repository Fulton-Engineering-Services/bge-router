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

use std::collections::HashMap;
use std::time::Duration;

use super::Config;

// `from_lookup` lets tests inject a deterministic environment without
// mutating process-global state, so these tests are safe to run in parallel
// even outside cargo-nextest.

fn from_map(entries: &[(&str, &str)]) -> anyhow::Result<Config> {
    let map: HashMap<String, String> = entries
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Config::from_lookup(|k| map.get(k).cloned())
}

fn empty_lookup() -> Config {
    Config::from_lookup(|_| None).expect("empty environment must produce defaults")
}

// ── defaults ────────────────────────────────────────────────────────────────

#[test]
fn from_lookup_succeeds_with_no_vars_set() {
    let _ = empty_lookup();
}

#[test]
fn default_bind_address() {
    assert_eq!(empty_lookup().bind, "0.0.0.0:8081");
}

#[test]
fn default_gpu_dns() {
    assert_eq!(empty_lookup().gpu_dns, "bge-m3-gpu.codekeeper.internal");
}

#[test]
fn default_cpu_dns() {
    assert_eq!(empty_lookup().cpu_dns, "bge-m3-cpu.codekeeper.internal");
}

#[test]
fn default_dns_refresh_is_30s() {
    assert_eq!(empty_lookup().dns_refresh, Duration::from_secs(30));
}

#[test]
fn default_health_poll_is_5s() {
    assert_eq!(empty_lookup().health_poll, Duration::from_secs(5));
}

#[test]
fn default_hedge_delay_is_5000ms() {
    assert_eq!(empty_lookup().hedge_delay, Duration::from_secs(5));
}

#[test]
fn default_control_timeout_is_1000ms() {
    assert_eq!(empty_lookup().control_timeout, Duration::from_secs(1));
}

#[test]
fn default_heartbeat_is_60s() {
    assert_eq!(empty_lookup().heartbeat, Duration::from_secs(60));
}

#[test]
fn default_does_not_set_legacy_flag() {
    assert!(!empty_lookup().legacy_fallback_budget_set);
}

// ── overrides via lookup ────────────────────────────────────────────────────

#[test]
fn explicit_hedge_delay_overrides_default() {
    let cfg = from_map(&[("BGE_ROUTER_HEDGE_DELAY_MS", "2500")]).unwrap();
    assert_eq!(cfg.hedge_delay, Duration::from_millis(2_500));
    assert!(!cfg.legacy_fallback_budget_set);
}

#[test]
fn explicit_control_timeout_overrides_default() {
    let cfg = from_map(&[("BGE_ROUTER_CONTROL_TIMEOUT_MS", "300")]).unwrap();
    assert_eq!(cfg.control_timeout, Duration::from_millis(300));
}

// ── back-compat: legacy BGE_ROUTER_FALLBACK_BUDGET_MS ──────────────────────

#[test]
fn legacy_fallback_budget_seeds_hedge_delay_when_new_var_absent() {
    // When only the legacy var is set, hedge_delay falls back to its value
    // (safer migration: keep the deployment's old budget).  The legacy flag
    // is set so `lib::run` can emit a one-time WARN at startup.
    let cfg = from_map(&[("BGE_ROUTER_FALLBACK_BUDGET_MS", "1500")]).unwrap();
    assert_eq!(cfg.hedge_delay, Duration::from_millis(1_500));
    assert!(cfg.legacy_fallback_budget_set);
}

#[test]
fn legacy_fallback_budget_does_not_affect_control_timeout() {
    // Control plane MUST keep its short hard timeout even when a deployment
    // is mid-migration: legacy var only seeds hedge_delay, never
    // control_timeout.
    let cfg = from_map(&[("BGE_ROUTER_FALLBACK_BUDGET_MS", "9000")]).unwrap();
    assert_eq!(cfg.control_timeout, Duration::from_secs(1));
}

#[test]
fn explicit_hedge_delay_wins_over_legacy_var() {
    let cfg = from_map(&[
        ("BGE_ROUTER_FALLBACK_BUDGET_MS", "9000"),
        ("BGE_ROUTER_HEDGE_DELAY_MS", "2000"),
    ])
    .unwrap();
    assert_eq!(cfg.hedge_delay, Duration::from_secs(2));
    // The flag is still set so a WARN is logged at startup recommending
    // operators drop the deprecated var.
    assert!(cfg.legacy_fallback_budget_set);
}

// ── validation ──────────────────────────────────────────────────────────────

#[test]
fn zero_hedge_delay_is_rejected() {
    let err = from_map(&[("BGE_ROUTER_HEDGE_DELAY_MS", "0")]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("BGE_ROUTER_HEDGE_DELAY_MS"),
        "error must name the offending var: {msg}"
    );
}

#[test]
fn zero_control_timeout_is_rejected() {
    let err = from_map(&[("BGE_ROUTER_CONTROL_TIMEOUT_MS", "0")]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("BGE_ROUTER_CONTROL_TIMEOUT_MS"),
        "error must name the offending var: {msg}"
    );
}

#[test]
fn non_numeric_hedge_delay_is_rejected() {
    let err = from_map(&[("BGE_ROUTER_HEDGE_DELAY_MS", "abc")]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("BGE_ROUTER_HEDGE_DELAY_MS"), "{msg}");
}
