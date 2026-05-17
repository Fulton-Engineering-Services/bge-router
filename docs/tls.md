# TLS Configuration

`bge-router` exposes two independent TLS surfaces:

| Surface | What it protects | Requires `--features tls` |
|---------|-----------------|--------------------------|
| **Inbound TLS** | The router's own HTTP listener — what clients connecting to bge-router see | Yes |
| **Outbound TLS** | Connections from bge-router to upstream bge-m3 instances | No |

Both are **opt-in**. No TLS environment variables need to be set for a plain-HTTP deployment; the default behaviour is unchanged.

---

## 1. Inbound TLS (The Router's Own Listener)

### Build-time requirement

The inbound TLS listener is compiled behind the `tls` Cargo feature. It pulls in
`axum-server` (with `tls-rustls`) and `aws-lc-sys`, which requires `cmake` and a
C compiler at build time.

```bash
# Ubuntu / Debian
sudo apt-get install -y cmake

# macOS — cmake is included in Xcode Command Line Tools; or:
brew install cmake

# Build the TLS-capable binary
cargo build --release --features tls
```

The default plain-HTTP build (`cargo build`) is unaffected — it does not compile
`aws-lc-sys` and requires no C toolchain.

### Runtime configuration

Set **both** the cert and key environment variables. Setting only one is a hard
startup error:

```
BGE_ROUTER_TLS_CERT_PATH=/tls/leaf.crt
BGE_ROUTER_TLS_KEY_PATH=/tls/leaf.key
```

Both variables must point to PEM-encoded files:

- `BGE_ROUTER_TLS_CERT_PATH` — the leaf certificate (and optionally the intermediate
  chain) in PEM format.
- `BGE_ROUTER_TLS_KEY_PATH` — the matching private key in PEM format.

**If the binary was compiled without `--features tls`**, the cert/key variables are
read and validated for consistency (both-or-neither), but TLS is **not** activated —
the server starts in plain-HTTP mode and logs `mode = "plain"`. See the
[Troubleshooting](#9-troubleshooting) section.

### TLS protocol

Rustls enforces **TLS 1.2 and 1.3** with AEAD-only cipher suites. There is no
configuration knob for protocol version or cipher selection.

### Graceful shutdown

When inbound TLS is active, `axum_server::Handle` manages the shutdown lifecycle.
On `SIGINT`/`SIGTERM`, the router stops accepting new TLS connections and waits up
to **30 seconds** for in-flight requests to complete before closing existing
connections. Embedding requests typically finish in under 5 seconds under normal
load, so all requests drain cleanly within the window.

### Startup confirmation

Check the `bge-router ready` log line:

```json
{"fields":{"message":"bge-router ready","bind":"0.0.0.0:8081","mode":"tls"}}
```

`mode = "plain"` means the server started without TLS (either the feature flag is
absent or no cert/key was provided).

---

## 2. Outbound Upstream TLS (Router → bge-m3)

The outbound TLS configuration controls how bge-router connects to upstream
bge-m3 instances. It is **independent of the `tls` Cargo feature** — you can enable
outbound TLS on a plain-HTTP inbound listener.

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BGE_ROUTER_UPSTREAM_TLS` | unset | Set to `1`, `true`, or `yes` to use HTTPS for all upstream bge-m3 connections. The system CA store is used for certificate validation. |
| `BGE_ROUTER_UPSTREAM_CA_BUNDLE` | unset | Path to a CA bundle PEM to trust for upstream connections. Use when bge-m3 instances present self-signed or internal CA certificates. Must be used together with `BGE_ROUTER_UPSTREAM_TLS=1`. |

Setting `BGE_ROUTER_UPSTREAM_CA_BUNDLE` without `BGE_ROUTER_UPSTREAM_TLS=1` logs a
startup `WARN` — the bundle is loaded into the reqwest client but all upstream
connections still use `http://`.

### What "upstream TLS" covers

The `UpstreamScheme` type (`Http`/`Https`) is threaded through the entire request
lifecycle:

- **DNS discovery** — addresses are resolved and typed; the scheme is applied when
  building request URLs.
- **Health polling** — `GET /health` on each upstream uses `http://` or `https://`
  based on the configured scheme.
- **Hedged-race proxy path** — both the GPU and CPU sides of the race use the same
  scheme.
- **Sequential-timeout proxy path** — control-plane routes (`/health`, `/v1/models`)
  also use the same scheme.

---

## 3. Certificate Provisioning

### AWS ECS with a shared internal CA (recommended)

In production ECS deployments, a CDK entrypoint preamble generates a leaf
certificate at container start from a Secrets Manager secret (`LOCKBOX_TLS_CA_JSON`).
It writes three files:

| File | Content |
|------|---------|
| `/tls/leaf.crt` | Signed leaf certificate for this task |
| `/tls/leaf.key` | Matching private key |
| `/tls/ca.crt` | CA bundle for validating peer bge-m3 certificates |

The CDK task definition injects the matching environment variables automatically.

### Local development and testing

Generate a self-signed certificate with `openssl`:

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout /tmp/leaf.key \
  -out /tmp/leaf.crt \
  -subj "/CN=localhost"
```

Then set the environment variables:

```bash
export BGE_ROUTER_TLS_CERT_PATH=/tmp/leaf.crt
export BGE_ROUTER_TLS_KEY_PATH=/tmp/leaf.key
cargo run --features tls
```

---

## 4. Typical Deployment Configurations

### Plain HTTP everywhere (default)

No TLS environment variables. Binary compiled without `--features tls`. No changes
from the baseline deployment.

```bash
cargo build --release
```

### Full TLS — inbound and outbound with a shared internal CA

```bash
BGE_ROUTER_TLS_CERT_PATH=/tls/leaf.crt
BGE_ROUTER_TLS_KEY_PATH=/tls/leaf.key
BGE_ROUTER_UPSTREAM_TLS=1
BGE_ROUTER_UPSTREAM_CA_BUNDLE=/tls/ca.crt
```

The binary **must** be compiled with `--features tls` for inbound TLS to activate.

### Outbound TLS only — upstreams use publicly-trusted CA certs

No `--features tls` needed. The inbound listener remains plain HTTP.

```bash
BGE_ROUTER_UPSTREAM_TLS=1
# BGE_ROUTER_UPSTREAM_CA_BUNDLE is not needed — system CA store validates
```

Use this pattern when bge-m3 instances are fronted by an ALB or a service mesh
that terminates TLS with an ACM certificate.

### Inbound TLS only — outbound plain HTTP

```bash
BGE_ROUTER_TLS_CERT_PATH=/tls/leaf.crt
BGE_ROUTER_TLS_KEY_PATH=/tls/leaf.key
# BGE_ROUTER_UPSTREAM_TLS not set — upstream connections remain HTTP
```

Binary must be compiled with `--features tls`.

---

## 5. Health Checks with TLS

### `/router/health` (the router's own diagnostic endpoint)

The existing `/dev/tcp` TCP-level health check works when inbound TLS is active
because it operates at the TCP connection layer, not the HTTP layer:

```bash
exec 3<>/dev/tcp/127.0.0.1/8081 \
  && printf "GET /router/health HTTP/1.0\r\nHost: localhost\r\n\r\n" >&3 \
  && read -t5 s <&3 \
  && [[ $s == *200* ]] || exit 1
```

> **Note:** The raw TCP check sends a plaintext HTTP request over a TLS socket.
> Modern TLS stacks return a TLS alert (`bad_record_mac` or `decode_error`) to a
> plaintext request, which the shell read treats as a non-200 response. Use the
> `curl` form below for a reliable TLS health check.

For curl-based checks when inbound TLS is active:

```bash
# Skip cert verification (e.g. self-signed leaf)
curl -sfk https://127.0.0.1:8081/router/health

# Verify with a specific CA bundle
curl -sf --cacert /tls/ca.crt https://127.0.0.1:8081/router/health
```

### Upstream health polling

When `BGE_ROUTER_UPSTREAM_TLS=1`, the router's internal health poller automatically
uses `https://` when querying each upstream's `/health` endpoint. No additional
configuration is required.

---

## 6. Environment Variable Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `BGE_ROUTER_TLS_CERT_PATH` | unset | Path to the TLS certificate PEM for the inbound listener. Requires `--features tls` at build time. Must be set together with `BGE_ROUTER_TLS_KEY_PATH`, or both must be absent. |
| `BGE_ROUTER_TLS_KEY_PATH` | unset | Path to the TLS private key PEM for the inbound listener. Must be set together with `BGE_ROUTER_TLS_CERT_PATH`, or both must be absent. |
| `BGE_ROUTER_UPSTREAM_TLS` | unset | Set to `1`, `true`, or `yes` to use HTTPS for all upstream bge-m3 connections. Does **not** require `--features tls`. |
| `BGE_ROUTER_UPSTREAM_CA_BUNDLE` | unset | Path to a CA bundle PEM for validating upstream bge-m3 certificates. Used together with `BGE_ROUTER_UPSTREAM_TLS`. A startup `WARN` is logged if this is set without `UPSTREAM_TLS`. |

---

## 7. Build and CI

The `tls` feature is **off by default**. The plain-HTTP Docker images and standard
build pipeline are unaffected.

CI runs a dedicated `test-tls` job on every push:

1. Installs `cmake` (required by `aws-lc-sys`).
2. Runs `cargo clippy --all-targets --features tls -- -D warnings`.
3. Runs `cargo nextest run --features tls --no-tests=warn`.

To replicate the CI check locally:

```bash
# Install cmake if not already present
which cmake || sudo apt-get install -y cmake   # Ubuntu
which cmake || brew install cmake              # macOS

cargo clippy --all-targets --features tls -- -D warnings
cargo nextest run --features tls --no-tests=warn
```

---

## 8. Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `TLS misconfiguration: BGE_ROUTER_TLS_CERT_PATH and BGE_ROUTER_TLS_KEY_PATH must both be set or both be absent` at startup | Only one of CERT/KEY is set | Set both variables, or unset both |
| `TLS config error: …` at startup | Invalid PEM, mismatched cert/key pair, or file not readable | Verify file paths and check that the certificate and key belong to the same key pair |
| Router starts in `mode = "plain"` despite CERT/KEY variables being set | Binary compiled without `--features tls` | Rebuild: `cargo build --release --features tls` |
| Upstream health checks fail after setting `BGE_ROUTER_UPSTREAM_TLS=1` | Upstream bge-m3 instances not yet serving HTTPS | Ensure bge-m3 is also compiled and configured with TLS; or unset `UPSTREAM_TLS` to keep connections plain HTTP |
| `CA bundle set but upstream TLS not enabled` warning in logs | `BGE_ROUTER_UPSTREAM_CA_BUNDLE` is set without `BGE_ROUTER_UPSTREAM_TLS=1` | Add `BGE_ROUTER_UPSTREAM_TLS=1`, or remove `UPSTREAM_CA_BUNDLE` if it is unneeded |
| `cmake: command not found` during `cargo build --features tls` | cmake is not installed | `sudo apt-get install -y cmake` (Linux) or `brew install cmake` (macOS) |
| `CERTIFICATE_VERIFY_FAILED` errors in upstream health logs | System CA store does not trust the bge-m3 TLS certificate | Set `BGE_ROUTER_UPSTREAM_CA_BUNDLE` to the matching CA bundle PEM |
