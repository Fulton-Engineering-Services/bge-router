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

//! Tracing initialization with a `bge_module` tag on every JSON log line.
//!
//! In `CloudWatch` / non-TTY environments the router emits one JSON object per
//! log event. Every JSON event begins with a `"bge_module"` attribute whose
//! value is the compile-time constant `"router"`, allowing operators to
//! distinguish router lines from embedding-server lines in mixed log streams.
//!
//! The human-readable `text` / `pretty` formats used during local dev are
//! left unchanged.

use std::fmt;

use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::registry::LookupSpan;

/// Compile-time identifier for this binary in log output.
pub const BGE_MODULE: &str = "router";

/// Wraps a JSON [`FormatEvent`] so the rendered object always begins with a
/// `"bge_module"` key.
///
/// The wrapper renders the inner formatter into a temporary `String`, then
/// rewrites the leading `{` to `{"bge_module":"router",`. All other keys, span
/// context, and trailing newline produced by the inner formatter are preserved
/// verbatim, so existing `CloudWatch` Insights queries keep working.
pub struct PrependModule<F> {
    inner: F,
}

impl<F> PrependModule<F> {
    /// Wrap `inner` so its rendered events are prefixed with the module key.
    pub fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<S, N, F> FormatEvent<S, N> for PrependModule<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut buf = String::new();
        self.inner.format_event(ctx, Writer::new(&mut buf), event)?;

        let trailing_newline = buf.ends_with('\n');
        let body = if trailing_newline {
            &buf[..buf.len() - 1]
        } else {
            &buf[..]
        };

        if let Some(rest) = body.strip_prefix('{') {
            if let Some(rest) = rest.strip_prefix('}') {
                // Inner produced an empty object `{}` — emit `{"bge_module":"…"}`
                // followed by whatever (if anything) trailed the close brace.
                write!(writer, "{{\"bge_module\":\"{BGE_MODULE}\"}}{rest}")?;
            } else {
                write!(writer, "{{\"bge_module\":\"{BGE_MODULE}\",{rest}")?;
            }
        } else {
            // Inner formatter did not produce a JSON object. Pass the body
            // through unchanged rather than corrupting it.
            writer.write_str(body)?;
        }

        if trailing_newline {
            writer.write_char('\n')?;
        }
        Ok(())
    }
}

/// Initialize the global tracing subscriber.
///
/// Reads `RUST_LOG` for the filter directive (defaulting to `info`) and
/// `BGE_ROUTER_LOG_FORMAT` for the format selection: `json`, `text`/`pretty`,
/// or auto-detect via the stdout TTY check. JSON events carry a leading
/// `"bge_module"` attribute; the human formats are unchanged.
///
/// Calling this twice is harmless — the second call no-ops via `try_init`.
pub fn init() {
    let env_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let log_format = std::env::var("BGE_ROUTER_LOG_FORMAT").ok();
    // JSON by default in non-TTY environments (Docker / Fargate / CloudWatch).
    // Force pretty with BGE_ROUTER_LOG_FORMAT=text or BGE_ROUTER_LOG_FORMAT=pretty.
    // Force JSON with BGE_ROUTER_LOG_FORMAT=json.
    let want_json = match log_format.as_deref() {
        Some("text" | "pretty") => false,
        Some("json") => true,
        _ => !std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    if want_json {
        let inner = tracing_subscriber::fmt::format()
            .json()
            .with_current_span(true);
        tracing_subscriber::fmt()
            .event_format(PrependModule::new(inner))
            .fmt_fields(tracing_subscriber::fmt::format::JsonFields::new())
            .with_env_filter(env_filter)
            .try_init()
            .ok();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .try_init()
            .ok();
    }
}

#[cfg(test)]
mod tests;
