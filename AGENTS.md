# AGENTS.md — bge-router

Full project context is in [`CLAUDE.md`](CLAUDE.md). This file adds cloud-agent-specific
notes that differ from local development.

## Cloud Environment

The Cursor cloud environment (`.cursor/Dockerfile`) provides:
- Rust stable with `rustfmt` and `clippy`
- `cargo-nextest`, `cargo-deny`, `hawkeye`

No ONNX Runtime or model files are required — bge-router is a pure-Rust stateless proxy
with no ML dependencies.

## Build & Test Commands (Cloud)

Standard cargo commands — no feature flags required:

```bash
# Build
cargo build

# Test
cargo nextest run --no-tests=warn

# Lint
cargo clippy --all-targets -- -D warnings

# Format check
cargo fmt --all --check

# Supply-chain audit
cargo deny check

# License headers
hawkeye check
```

## Running the Server

The router requires two DNS names to start. For local smoke-testing in the cloud environment,
point both to `localhost` (the router will start cleanly; upstreams will simply resolve to
nothing and the pool will be empty):

```bash
BGE_ROUTER_GPU_DNS=localhost BGE_ROUTER_CPU_DNS=localhost cargo run
```

Verify it came up:

```bash
curl http://localhost:8081/router/health | jq .
```

The router starts in under one second — there is no model loading or startup probe.

## Key Notes

- **No `--features` flags needed** — there are no optional Cargo features in this crate.
- **No model downloads** — the router is stateless; it holds no ML weights.
- **`cargo fmt --all` before pushing** — CI rejects any formatting drift.
- Do not create git tags manually; see `CLAUDE.md` for the release workflow.
