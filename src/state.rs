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

use anyhow::Context;
use arc_swap::ArcSwap;

use crate::config::Config;
use crate::upstream::snapshot::{PoolSnapshot, UpstreamScheme};

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
    /// When `config.upstream_ca_bundle` is set the reqwest client is configured to
    /// trust that CA bundle for all upstream connections.
    ///
    /// # Errors
    ///
    /// Returns an error if the CA-bundle file cannot be read or parsed, or if the
    /// [`reqwest::Client`] cannot be built.
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let mut client_builder = reqwest::Client::builder().pool_max_idle_per_host(32);
        if let Some(ca_path) = &config.upstream_ca_bundle {
            let ca_pem = std::fs::read(ca_path).with_context(|| {
                format!("reading upstream CA bundle from {}", ca_path.display())
            })?;
            let cert = reqwest::Certificate::from_pem(&ca_pem)
                .context("upstream CA bundle is not valid PEM")?;
            client_builder = client_builder.add_root_certificate(cert);
        }
        let client = client_builder.build().context("building reqwest::Client")?;
        Ok(Self {
            pool: Arc::new(ArcSwap::from_pointee(PoolSnapshot::default())),
            config: Arc::new(config),
            client,
        })
    }

    /// Return the [`UpstreamScheme`] to use when contacting upstream bge-m3
    /// instances. Delegates to [`Config::upstream_scheme`].
    #[must_use]
    pub fn upstream_scheme(&self) -> UpstreamScheme {
        self.config.upstream_scheme()
    }
}
