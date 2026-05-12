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

//! Shared application state threaded through Axum handlers.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::upstream::snapshot::PoolSnapshot;

/// Shared state available to every request handler via [`axum::extract::State`].
///
/// All fields are cheaply cloneable (`Arc`-backed).
#[derive(Clone)]
pub struct AppState {
    /// Atomic snapshot of the upstream pool — updated by discovery and health tasks.
    pub pool: Arc<ArcSwap<PoolSnapshot>>,
    /// Resolved runtime configuration.
    pub config: Arc<Config>,
    /// Shared HTTP client for upstream requests.
    pub client: reqwest::Client,
}

impl AppState {
    /// Create a new [`AppState`] with an empty pool snapshot and a fresh HTTP client.
    ///
    /// # Panics
    ///
    /// Panics if the [`reqwest::Client`] cannot be built (should not happen with
    /// the default configuration).
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            pool: Arc::new(ArcSwap::from_pointee(PoolSnapshot::default())),
            config: Arc::new(config),
            client: reqwest::Client::builder()
                .pool_max_idle_per_host(32)
                .build()
                .expect("reqwest::Client::build should not fail with these settings"),
        }
    }
}
