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

/// Bind the TCP listener and serve HTTP (or HTTPS when the `tls` feature is
/// enabled and cert/key paths are configured) until a shutdown signal is received.
///
/// # Errors
///
/// Returns an error if the TCP bind fails or, when the `tls` feature is
/// enabled, if the TLS configuration cannot be loaded.
pub async fn serve(state: AppState) -> Result<()> {
    let bind = state.config.bind.clone();
    #[cfg(feature = "tls")]
    let tls_cert = state.config.tls_cert_path.clone();
    #[cfg(feature = "tls")]
    let tls_key = state.config.tls_key_path.clone();

    let app = router::build(state);

    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        use axum_server::tls_rustls::RustlsConfig;
        use axum_server::Handle;

        let tls_config = RustlsConfig::from_pem_file(&cert, &key)
            .await
            .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?;
        let addr: std::net::SocketAddr = bind
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid bind addr '{bind}': {e}"))?;

        let handle = Handle::new();
        let h = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("TLS shutdown signal received, draining connections");
            h.graceful_shutdown(Some(std::time::Duration::from_secs(30)));
        });

        tracing::info!(bind = %bind, mode = "tls", "bge-router ready");
        return axum_server::bind_rustls(addr, tls_config)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .map_err(Into::into);
    }

    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {bind}: {e}"))?;
    tracing::info!(bind = %bind, mode = "plain", "bge-router ready");
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
