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

//! Application error type with automatic HTTP response conversion.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Top-level error type for request-handling paths.
#[derive(Debug)]
pub enum AppError {
    /// All candidate upstreams are unavailable or unhealthy.
    NoUpstreamAvailable,
    /// The upstream returned an error or an unexpected connection failure.
    Upstream(reqwest::Error),
    /// An unexpected internal failure (invalid header value, etc.).
    Internal(anyhow::Error),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoUpstreamAvailable => write!(f, "no upstream available"),
            Self::Upstream(e) => write!(f, "upstream error: {e}"),
            Self::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Upstream(e) => Some(e),
            Self::Internal(e) => e.source(),
            Self::NoUpstreamAvailable => None,
        }
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::Upstream(e)
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::NoUpstreamAvailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    use super::AppError;

    // ── Display ────────────────────────────────────────────────────────────

    #[test]
    fn no_upstream_available_display() {
        assert_eq!(
            AppError::NoUpstreamAvailable.to_string(),
            "no upstream available"
        );
    }

    #[test]
    fn internal_error_display() {
        let err = AppError::Internal(anyhow::anyhow!("something broke"));
        assert!(err.to_string().contains("something broke"));
    }

    // ── IntoResponse status codes ─────────────────────────────────────────

    #[test]
    fn no_upstream_available_produces_503() {
        let response = AppError::NoUpstreamAvailable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn internal_error_produces_500() {
        let err = AppError::Internal(anyhow::anyhow!("internal"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ── From impls ─────────────────────────────────────────────────────────

    #[test]
    fn from_anyhow_error_creates_internal_variant() {
        let anyhow_err = anyhow::anyhow!("wrapped");
        let app_err = AppError::from(anyhow_err);
        assert!(matches!(app_err, AppError::Internal(_)));
        assert!(app_err.to_string().contains("wrapped"));
    }
}
