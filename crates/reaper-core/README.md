# reaper-core

The pure core of k8s-reaper: plain data in, plain data out. It detects crash
loops and failing CronJobs, decides one plan per workload, and executes plans
through ports (`ClusterWriter`, `EventSink`, `Telemetry`, `Notifier`).

- No Kubernetes client or types, no async runtime, no clock. "Now" is a
  parameter. Depends only on `serde`, `serde_json` and `thiserror`;
  `tests/no_infra_deps.rs` enforces it.
- Entry points: `decide::decide`, `executor::execute`, `config::Config::from_raw`.
- `testing` has fixture builders and in-memory fakes of every port.

Tests: `cargo test -p reaper-core`. Benchmark: `cargo bench -p reaper-core`.

See [CONTRIBUTING.md, Architecture](../../CONTRIBUTING.md#architecture).
