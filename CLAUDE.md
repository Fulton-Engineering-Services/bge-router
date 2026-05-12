# CLAUDE.md — bge-router

Axum HTTP server that transparently proxies BGE-M3 embedding requests between GPU and CPU upstream pools, discovered via DNS (AWS Cloud Map compatible).

## Use Cases

- Unified `bge-m3.codekeeper.internal` endpoint that routes to GPU when warm, CPU as fallback
- Transparent to callers — same API as `bge-m3-embedding-server` (`/v1/embeddings`, `/v1/sparse-embeddings`, `/v1/embeddings:both`)
- Scale-to-zero GPU support: router keeps serving via CPU while GPU cold-starts
- Companion to `bge-m3-embedding-server` in the bge-gpu-burst-pool architecture

## Build & Test Commands

```bash
cargo build
cargo nextest run --no-tests=warn
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check     # supply chain audit
hawkeye check        # license headers (.rs files only)
```

## Run Locally

```bash
BGE_ROUTER_GPU_DNS=localhost BGE_ROUTER_CPU_DNS=localhost \
  cargo run
```

Hit the router's own health endpoint to verify startup:
```bash
curl http://localhost:8081/router/health | jq .
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/embeddings` | Proxied to best upstream (transparent) |
| `POST` | `/v1/sparse-embeddings` | Proxied to best upstream (transparent) |
| `POST` | `/v1/embeddings:both` | Proxied to best upstream (transparent) |
| `GET` | `/health` | Proxied to selected upstream's `/health` |
| `GET` | `/v1/models` | Proxied to selected upstream's `/v1/models` |
| `GET` | `/router/health` | Router's own health: upstream pool snapshot |

### `/router/health` Response

```json
{
  "status": "ok",
  "gpu_upstreams": [
    { "addr": "10.0.1.5:8081", "pool_type": "gpu", "status": "ok",
      "queue_depth": 0, "live_workers": 1, "last_seen_secs_ago": 2.1 }
  ],
  "cpu_upstreams": [
    { "addr": "10.0.2.8:8081", "pool_type": "cpu", "status": "ok",
      "queue_depth": 0, "live_workers": 8, "last_seen_secs_ago": 1.4 }
  ]
}
```

Status is `"ok"` when at least one upstream is healthy; `"degraded"` otherwise.
HTTP 503 when all pools are empty or unhealthy.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BGE_ROUTER_BIND` | `0.0.0.0:8081` | TCP bind address |
| `BGE_ROUTER_GPU_DNS` | `bge-m3-gpu.codekeeper.internal` | DNS name for GPU upstreams |
| `BGE_ROUTER_CPU_DNS` | `bge-m3-cpu.codekeeper.internal` | DNS name for CPU upstreams |
| `BGE_ROUTER_DNS_REFRESH_SECS` | `30` | How often to re-resolve both DNS names |
| `BGE_ROUTER_HEALTH_POLL_SECS` | `5` | How often to poll each upstream's `/health` |
| `BGE_ROUTER_FALLBACK_BUDGET_MS` | `1000` | Max ms to wait before trying fallback pool |
| `BGE_ROUTER_HEARTBEAT_SECS` | `60` | Heartbeat log interval (`0` = disable) |
| `BGE_ROUTER_LOG_FORMAT` | auto | `json` (non-TTY default), `text`, `pretty` |
| `RUST_LOG` | `info` | Standard tracing filter |

## Architecture

**No worker pool needed** — the router is stateless and horizontally scalable. Two Fargate tasks run in parallel for HA.

**Upstream discovery:**
- DNS refresh every `BGE_ROUTER_DNS_REFRESH_SECS` seconds via `tokio::net::lookup_host`
- New addresses start as `Unknown`; disappeared addresses are removed
- Each address that resolves from both names gets typed as `Gpu` or `Cpu`

**Health polling:**
- Every `BGE_ROUTER_HEALTH_POLL_SECS`, GET `/health` on all known upstreams concurrently
- Parses bge-m3 health response: `status`, `workers.live`, `queue_depth`
- Snapshot updated atomically via `arc-swap`

**Routing policy (in priority order):**
1. GPU upstream with `status=Ok`, lowest `queue_depth`
2. CPU upstream with `status=Ok`, lowest `queue_depth`
3. 503 if no healthy upstream

**Fallback:**
- Try GPU primary
- If connection refused or 5xx within `BGE_ROUTER_FALLBACK_BUDGET_MS`: try CPU fallback
- If response bytes already streaming to client: log WARN, never retry mid-stream

**Request body buffering:** The request body is buffered once (required for fallback retry). Response body is streamed without intermediate buffering.

## Source Layout Conventions

Follows bge-m3-embedding-server conventions exactly. No `mod.rs` files (use `foo.rs + foo/` layout). Parent module files are facades: `mod` declarations and `pub use` re-exports only.

### File-size targets

- Leaf source files: 100–400 lines, hard ceiling ~500 lines.
- `#[cfg(test)] mod tests;` beyond ~150 test lines → sibling file.

### Module layout

```
src/
  main.rs          — 20-40 lines, entry point
  lib.rs           — pub mod declarations only
  config.rs        — Config struct, from_env()
  state.rs         — AppState
  error.rs         — AppError enum, IntoResponse impl
  metrics.rs       — periodic heartbeat logger
  upstream.rs      — facade for upstream sub-modules
  upstream/
    discovery.rs   — DNS refresh task
    health.rs      — per-upstream /health poller
    snapshot.rs    — PoolSnapshot, UpstreamInfo, PoolType, UpstreamStatus
  router.rs        — facade
  router/
    policy.rs      — pick best upstream from snapshot
    proxy.rs       — zero-copy streaming proxy
    fallback.rs    — GPU→CPU fallback with budget
  handler.rs       — facade
  handler/
    proxy.rs       — Axum handler: buffer body, call fallback::route
    health.rs      — GET /router/health
  bootstrap.rs     — facade
  bootstrap/
    router.rs      — builds Axum Router with routes + middleware
    server.rs      — TCP bind, graceful shutdown
```

## Releasing

The Release workflow creates git tags automatically. **Do not create tags locally.**
To release: bump version in `Cargo.toml`, commit, push to `main`.

## Docker

```bash
docker build -t bge-router .
docker run --rm -p 8081:8081 \
  -e BGE_ROUTER_GPU_DNS=bge-m3-gpu.codekeeper.internal \
  -e BGE_ROUTER_CPU_DNS=bge-m3-cpu.codekeeper.internal \
  bge-router
```

## Gotchas

- **Request body is buffered** — required for fallback retry. Upstream response is streamed.
- **No model loading** — the router itself is stateless and starts in <1 second.
- **DNS names must resolve to port 8081** — the router appends `:8081` to each resolved address.
- **`/router/health` vs `/health`** — `/health` is proxied to the upstream; `/router/health` is the router's own diagnostic endpoint.
- **Always run `cargo fmt --all` before pushing** — CI fails `cargo fmt --all --check`.
