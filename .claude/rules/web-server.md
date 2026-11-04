---
description: "Axum web-server and HTTP API conventions"
paths:
  - "**/src/routes/**"
  - "**/src/handlers/**"
  - "**/src/api/**"
  - "**/src/main.rs"
---

# Web Server (Axum)

- Use `axum` for creating any web servers or HTTP APIs.
  - Keep request handlers async, returning `Result<Response, AppError>` to centralize error handling.
  - Use layered extractors and shared state structs instead of global mutable data.
  - Add `tower` middleware (timeouts, tracing, compression) for observability and resilience.
  - Offload CPU-bound work to `tokio::task::spawn_blocking` or background services to avoid blocking the reactor.
