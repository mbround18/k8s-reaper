//! Observability adapter: tracing, the `Telemetry` port and the health/metrics server.
//!
//! An adapter on the edge of the hexagon:
//!
//! - [`telemetry::Metrics`]: implements the core `Telemetry` port over a
//!   `prometheus-client` registry, mirrored to OpenTelemetry meters, plus the
//!   process-level recorders the binary calls (cycles, config,
//!   notifications). Names follow `contracts/metrics.md`.
//! - [`tracing::init`]: JSON or text logs on stdout, and OTLP export when
//!   `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
//! - [`http::serve`]: the axum server for `/healthz`, `/readyz` and
//!   `/metrics`, driven by [`http::Health`].

pub mod http;
pub mod telemetry;
pub mod tracing;
