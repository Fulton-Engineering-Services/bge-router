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

//! Per-route fallback strategy classification.
//!
//! Inference routes (`/v1/*embeddings*`) use a hedged race: GPU first, then
//! fire CPU in parallel after `hedge_delay` if GPU has not yet returned a
//! successful response.  Control-plane routes (`/health`, `/v1/models`, etc.)
//! use a sequential GPU→CPU fallback with a hard timeout per upstream — fast
//! failure detection matters more than masking latency.

use std::time::Duration;

use crate::config::Config;

/// Strategy used by [`crate::router::fallback::route`] for a given request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutePolicy {
    /// Hedged race: start GPU; after `delay`, fire CPU in parallel; first
    /// non-5xx response wins; loser's future is dropped (cancels the in-flight
    /// reqwest call and closes the connection).
    Hedged {
        /// Delay before the parallel CPU race fires.
        delay: Duration,
    },
    /// Sequential GPU→CPU with a hard timeout per upstream.  Used for
    /// control-plane routes where fast failure detection matters more than
    /// hiding GPU latency.
    SequentialTimeout {
        /// Per-upstream hard timeout.  Each of GPU and CPU is bounded
        /// independently; total worst-case is `2 × per_upstream`.
        per_upstream: Duration,
    },
}

impl RoutePolicy {
    /// Classify the request path and return the fallback strategy to apply.
    ///
    /// Inference paths — anything matching `/v1/*embeddings*` — are hedged.
    /// Everything else (health checks, model listings, future control plane
    /// endpoints) gets the sequential timeout treatment.
    #[must_use]
    pub fn for_path(path_and_query: &str, config: &Config) -> Self {
        if is_inference_path(path_and_query) {
            Self::Hedged {
                delay: config.hedge_delay,
            }
        } else {
            Self::SequentialTimeout {
                per_upstream: config.control_timeout,
            }
        }
    }
}

/// Returns `true` if the path should be hedged.
///
/// The matcher strips the query string (so `/v1/embeddings?foo=bar` still
/// matches) and then tests for the `/v1/` prefix and the literal substring
/// `embeddings`.  This deliberately covers `/v1/embeddings`,
/// `/v1/sparse-embeddings`, `/v1/embeddings:both`, and any future
/// `/v1/*embeddings*` variant the upstream might add.
fn is_inference_path(path_and_query: &str) -> bool {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(p, _)| p);
    path.starts_with("/v1/") && path.contains("embeddings")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RoutePolicy, is_inference_path};
    use crate::config::Config;

    fn cfg(hedge_ms: u64, ctl_ms: u64) -> Config {
        Config::from_lookup(|key| match key {
            "BGE_ROUTER_HEDGE_DELAY_MS" => Some(hedge_ms.to_string()),
            "BGE_ROUTER_CONTROL_TIMEOUT_MS" => Some(ctl_ms.to_string()),
            _ => None,
        })
        .expect("config from_lookup must succeed")
    }

    // ── is_inference_path ───────────────────────────────────────────────────

    #[test]
    fn dense_embeddings_is_inference() {
        assert!(is_inference_path("/v1/embeddings"));
    }

    #[test]
    fn sparse_embeddings_is_inference() {
        assert!(is_inference_path("/v1/sparse-embeddings"));
    }

    #[test]
    fn embeddings_both_is_inference() {
        assert!(is_inference_path("/v1/embeddings:both"));
    }

    #[test]
    fn query_string_does_not_break_inference_match() {
        assert!(is_inference_path("/v1/embeddings?model=bge-m3"));
    }

    #[test]
    fn future_embeddings_variant_is_inference() {
        // "any future /v1/*embeddings*" — covered by the substring match.
        assert!(is_inference_path("/v1/colbert-embeddings"));
        assert!(is_inference_path("/v1/embeddings-v2"));
    }

    #[test]
    fn health_is_control_plane() {
        assert!(!is_inference_path("/health"));
    }

    #[test]
    fn router_health_is_control_plane() {
        assert!(!is_inference_path("/router/health"));
    }

    #[test]
    fn models_is_control_plane() {
        assert!(!is_inference_path("/v1/models"));
    }

    #[test]
    fn unknown_path_is_control_plane() {
        // Anything not matching the inference shape falls through to the
        // sequential-timeout path so unexpected paths fail fast.
        assert!(!is_inference_path("/foo/bar"));
        assert!(!is_inference_path("/"));
    }

    // ── RoutePolicy::for_path ───────────────────────────────────────────────

    #[test]
    fn for_path_hedges_inference_paths() {
        let c = cfg(2_000, 500);
        assert_eq!(
            RoutePolicy::for_path("/v1/embeddings", &c),
            RoutePolicy::Hedged {
                delay: Duration::from_secs(2)
            }
        );
    }

    #[test]
    fn for_path_uses_sequential_for_control_plane() {
        let c = cfg(2_000, 500);
        assert_eq!(
            RoutePolicy::for_path("/health", &c),
            RoutePolicy::SequentialTimeout {
                per_upstream: Duration::from_millis(500)
            }
        );
        assert_eq!(
            RoutePolicy::for_path("/v1/models", &c),
            RoutePolicy::SequentialTimeout {
                per_upstream: Duration::from_millis(500)
            }
        );
    }
}
