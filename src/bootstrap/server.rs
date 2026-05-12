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

//! TCP listener, graceful shutdown, and top-level server loop.

use anyhow::Result;
use tokio::net::TcpListener;

use crate::bootstrap::router;
use crate::state::AppState;

/// Bind the TCP listener and serve HTTP until a shutdown signal is received.
///
/// # Errors
///
/// Returns an error if the TCP bind fails (e.g., the port is already in use).
pub async fn serve(state: AppState) -> Result<()> {
    let bind = state.config.bind.clone();
    let app = router::build(state);

    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {bind}: {e}"))?;

    tracing::info!(bind = %bind, "bge-router ready");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(Into::into)
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("installing ctrl-c handler should succeed");
    tracing::info!("shutdown signal received, draining in-flight requests");
}
