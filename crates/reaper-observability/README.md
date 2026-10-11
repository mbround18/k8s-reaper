# reaper-observability

Logs, traces, metrics and the health server.

- `telemetry`: `Metrics`, the `Telemetry` port over `prometheus-client`,
  mirrored to OpenTelemetry meters. Names and labels follow
  `specs/001-crashloop-reaper/contracts/metrics.md`; a test checks them.
- `tracing`: `init()`, JSON or text logs filtered by `RUST_LOG`, and OTLP
  export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
- `http`: the axum server for `/healthz`, `/readyz` and `/metrics`.

See [CONTRIBUTING.md, Architecture](../../CONTRIBUTING.md#architecture).
