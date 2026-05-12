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

//! Generic `X-*` header collection utilities.
//!
//! HTTP headers whose names begin with `X-` carry caller-supplied metadata.
//! This module collects all such headers from an incoming request into a
//! sorted map that can be embedded as a JSON object in structured log events.

use std::collections::BTreeMap;
use std::fmt;

use axum::http::HeaderMap;
use serde::Serialize;

/// A sorted map of `X-*` HTTP request headers.
///
/// Keys are lowercase-normalized header names (e.g. `"x-request-id"`).
/// Values are UTF-8 decoded header values; headers with non-UTF-8 values
/// are silently skipped.
///
/// Serializes as a plain JSON object via [`serde::Serialize`] and as a
/// compact JSON string via [`fmt::Display`], making it suitable for both
/// the `text`/`pretty` and JSON tracing log formats.
#[derive(Default, Serialize)]
#[serde(transparent)]
pub struct XHeaders(pub BTreeMap<String, String>);

impl XHeaders {
    /// Returns `true` when no `X-*` headers were present in the request.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for XHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Compact JSON — works for both text and JSON tracing formats.
        // BTreeMap<String, String> serialization is infallible.
        match serde_json::to_string(&self.0) {
            Ok(s) => f.write_str(&s),
            Err(_) => f.write_str("{}"),
        }
    }
}

/// Collects all headers whose name starts with `x-` (case-insensitive) into
/// an [`XHeaders`] map.
///
/// Header names are stored in their lowercase-normalized form (axum's
/// [`HeaderMap`] already lowercases all names in HTTP/1.1 and HTTP/2).
/// Headers whose values are not valid UTF-8 are silently skipped.
#[must_use]
pub fn collect_x_headers(headers: &HeaderMap) -> XHeaders {
    let mut map = BTreeMap::new();
    for (name, value) in headers {
        let name_str = name.as_str();
        if name_str.starts_with("x-") {
            if let Ok(val) = value.to_str() {
                map.insert(name_str.to_owned(), val.to_owned());
            }
        }
    }
    XHeaders(map)
}

#[cfg(test)]
mod tests {
    use axum::http::{header, HeaderValue};

    use super::*;

    #[test]
    fn collects_x_prefix_headers_and_skips_standard() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static("x-foo"),
            HeaderValue::from_static("bar"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let result = collect_x_headers(&headers);
        assert!(!result.is_empty());
        assert_eq!(result.0.get("x-foo").map(String::as_str), Some("bar"));
        assert_eq!(result.0.len(), 1, "content-type must be excluded");
    }

    #[test]
    fn empty_headers_produces_empty_xheaders() {
        let result = collect_x_headers(&HeaderMap::new());
        assert!(result.is_empty());
    }

    #[test]
    fn headers_with_no_x_prefix_produces_empty_xheaders() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok"),
        );
        let result = collect_x_headers(&headers);
        assert!(result.is_empty());
    }

    #[test]
    fn display_produces_compact_json_object() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static("x-project"),
            HeaderValue::from_static("my-proj"),
        );
        headers.insert(
            axum::http::HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("abc123"),
        );
        let xh = collect_x_headers(&headers);
        // BTreeMap ordering: x-project < x-request-id
        assert_eq!(
            xh.to_string(),
            r#"{"x-project":"my-proj","x-request-id":"abc123"}"#
        );
    }

    #[test]
    fn empty_xheaders_displays_as_empty_json_object() {
        let xh = XHeaders::default();
        assert_eq!(xh.to_string(), "{}");
    }
}
