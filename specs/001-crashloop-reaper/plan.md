# Implementation Plan: MVP Crashloop Reaper

**Branch**: `001-crashloop-reaper` | **Date**: 2026-10-10 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-crashloop-reaper/spec.md`

## Summary

The reaper is a single-replica Rust operator.

1. Watch-backed caches feed a snapshot every 60 s, or immediately on a restore
   request.
2. A **pure core** turns `(snapshot, config, now)` into a plan per workload:
   observe, candidate, reap steps, restore steps, or a skip with a reason. The
   decision is identical in both modes.
3. An **executor** in the core runs the plan through ports. Report mode writes
   only the reaper's own annotations, labels and Events. Full mode also scales
   (via the `scale` subresource) and applies the Flux holds:
   - `kustomize.toolkit.fluxcd.io/reconcile: disabled` on kustomize-managed
     objects;
   - a reference-counted `spec.suspend` on HelmReleases.
4. All state lives in annotations on the objects it touched, written in an
   order that makes every step idempotent and every reap reversible by one
   annotation (`reaper.boop.ninja/restore`).
5. Configuration is a cluster-scoped `ReaperConfig` CR. Telemetry goes out over
   OpenTelemetry plus a Prometheus `/metrics` endpoint, which existing
   Alertmanager → Discord routing picks up.

## Technical Context

**Language/Version**: Rust stable 1.99, edition 2024 (`rust-toolchain.toml`)

**Primary Dependencies**:

- `kube` (client, runtime, derive) and `k8s-openapi` (k8s 1.36 feature), with
  rustls
- `tokio`; `serde`, `serde_json`, `schemars` (CRD schema); `thiserror`
- `clap` (flags and env)
- `axum` + `tower` (health and metrics only)
- `tracing`, `tracing-subscriber`, `tracing-opentelemetry`
- `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`
- `prometheus-client`
- `globset` (namespace globs)
- tests: `tower-test`, `insta` (plan snapshots)

**Storage**: None. State is annotations on Kubernetes objects (research R6).

**Testing**: `cargo test` (core unit + adapter contract). k3d + Flux
integration suite, `#[ignore]`d, run by `make test-integration` and segmented
per user story.

**Target Platform**: Linux container on Kubernetes ≥ 1.30 (roanoke: k3s
v1.36.5), Flux v2 with `helm.toolkit.fluxcd.io/v2`

**Project Type**: Kubernetes operator (Cargo workspace: library crates + one
thin binary)

**Performance Goals**:

- One cycle on roanoke-sized state (about 300 pods) completes in under 1 s from
  cache.
- A restore request is acted on within 5 s of the annotation landing.

**Constraints**:

- Never mutate outside the rules in the spec (FR-011, FR-012).
- At most 1 reap per cycle and 5 reaped at once by default.
- Memory under 64 MiB.
- No API calls in the core.

**Scale/Scope**: one cluster, ~30 namespaces, ~300 pods, tens of workloads;
1 replica

## Constitution Check

_Gate evaluated before research and again after design. Both pass._

- [x] **I. Agnostic core, thin adapters.**
  - `reaper-core` depends only on `serde`, `serde_json` and `thiserror`: no
    `kube`, no `tokio`, no clock.
  - Ports are traits in the core, using native `async fn`, so no runtime is
    needed: `ClusterWriter`, `EventSink`, `Telemetry`. The snapshot comes in as
    plain data.
  - The adapters are `reaper-kube`, `reaper-flux` and `reaper-observability`.
  - `apps/k8s-reaper` only wires things together.
- [x] **II. Safe by default.**
  - Default mode is `report`, including when there is no config (FR-026).
  - One `decide()` serves both modes; the executor's mode gate is the only
    branch (SC-004 tested on every case).
  - Own namespace is hard-excluded in code, not config.
  - Per-cycle limit and reaped ceiling apply.
  - Every write is recorded on the object and reversed by one annotation.
  - Unknown, partial or other-controller cases are report-only.
- [x] **III. Test-first.**
  - Core tasks put tests first (tasks.md). Time is a parameter.
  - Adapter contract tests use fixtures and a mock API server.
  - The integration suite is opt-in and split by story.
- [x] **IV. Observable.**
  - OTel traces and metrics, plus a Prometheus pull endpoint.
  - An Event on every transition.
  - Contracts are documented in `contracts/`.
- [x] **V. Runtime config.**
  - Every behaviour switch is in `ReaperConfig.spec`, including Flux toggles,
    and hot-reloads.
  - No Cargo feature changes behaviour.
- [x] **VI. Simplicity.**
  - One CRD, justified in research R8.
  - Four library crates, each a real port boundary or adapter.
  - No leader election, webhook or database in the MVP.
  - Other controllers are not raced (R7).
- [x] **Repo constraints.**
  - `apps/` and `crates/` layout.
  - Makefile `gc` with build depending on it.
  - cargo-chef Dockerfile.
  - `MIT OR Apache-2.0`.
  - Light README, heavy CONTRIBUTING, `docs/guides/`.

Post-design re-check: data-model and contracts add no crate, service or
dependency beyond the list above. Still passes.

## Project Structure

### Documentation (this feature)

```text
specs/001-crashloop-reaper/
├── spec.md  plan.md  research.md  data-model.md  quickstart.md  tasks.md
├── checklists/requirements.md
└── contracts/
    ├── annotations.md   # reaper.boop.ninja/* and Flux annotations it writes
    ├── labels.md        # state + mode labels
    ├── config.md        # ReaperConfig CRD + process flags/env
    ├── events.md        # Event reasons and when they fire
    └── metrics.md       # metric names/labels + shipped alert rules
```

### Source Code (repository root)

```text
Cargo.toml                    # workspace, resolver 3, shared deps + lints
rust-toolchain.toml           # stable 1.99
Makefile                      # build (deps: gc), test, test-integration, lint, image, gc
Dockerfile                    # cargo-chef planner → cook → builder → distroless nonroot
CONTRIBUTING.md               # setup, architecture, decisions, testing, release
docs/guides/
├── getting-started.md        # install via Flux HelmRelease, report mode first
├── restoring-a-workload.md   # the one restore action, and "restore then fix"
└── configuration.md          # ReaperConfig fields, opt-out labels

crates/
├── reaper-core/              # PURE: no kube/tokio/clock
│   └── src/
│       ├── model.rs          # Snapshot, WorkloadRef, PodView, ContainerView, FluxRefs
│       ├── config.rs         # Config, defaults, validation, duration parsing
│       ├── detect.rs         # crashloop detection, owner resolution, observation
│       ├── decide.rs         # decide(snapshot, config, now) -> CyclePlan
│       ├── plan.rs           # Plan, Action, SkipReason, Transition; stable JSON
│       ├── record.rs         # annotation codec: ReapRecord, HelmReleaseHold
│       ├── ports.rs          # ClusterWriter, EventSink, Telemetry traits
│       └── executor.rs       # run_plan(plan, mode, ports): mode gate + step order
├── reaper-kube/              # kube-rs adapter
│   └── src/
│       ├── crd.rs            # ReaperConfig CustomResource (schemars)
│       ├── snapshot.rs       # reflectors → reaper_core::Snapshot
│       ├── writer.rs         # ClusterWriter impl: merge patch, scale subresource
│       └── events.rs         # EventSink impl over events.k8s.io/v1 Recorder
├── reaper-flux/              # Flux adapter
│   └── src/
│       ├── helmrelease.rs    # minimal HelmRelease v2 type + ApiResource
│       └── holds.rs          # suspend/resume + held-for patches (ClusterWriter part)
└── reaper-observability/
    └── src/
        ├── telemetry.rs      # Telemetry impl: OTel meters + prometheus-client registry
        ├── tracing.rs        # subscriber, JSON logs, OTLP traces when configured
        └── http.rs           # axum: /healthz /readyz /metrics

apps/
└── k8s-reaper/               # binary: `run` (default) and `crd` (print CRD YAML)
    ├── src/main.rs           # clap → build adapters → loop { snapshot; decide; run_plan }
    └── tests/                # integration (k3d + Flux), #[ignore], one file per story
        ├── us1_report.rs
        ├── us2_reap.rs
        ├── us3_restore.rs
        └── us4_config.rs

deploy/dev/                   # k3d config + sample crash-looping Deployments (dev only)
```

**Structure Decision**:

- One pure core crate holds detection, decision, record encoding, port
  definitions and the runtime-agnostic executor.
- There are three adapter crates: `reaper-kube`, `reaper-flux` and
  `reaper-observability`.
- One binary composes them.
- The CRD Rust type lives in `reaper-kube`, because it needs `kube::CustomResource`.
  It converts into the core's plain `Config`.
- The Helm chart is **not** in this repository. It lives in
  `mbround18/helm-charts/charts/k8s-reaper`, with the CRD YAML generated by
  `k8s-reaper crd`.

## Deployment (outside this repo, for context)

1. **helm-charts**: `charts/k8s-reaper` contains:
   - the CRD;
   - Deployment (1 replica, Recreate, non-root, read-only root fs);
   - ServiceAccount + ClusterRole (contracts/config.md lists the rules);
   - Service + ServiceMonitor;
   - PrometheusRule (contracts/metrics.md);
   - a `ReaperConfig/default` rendered from values (`mode: report`).

   It only takes effect once merged to that repo's `main`.
2. **fluxified**:
   - `overlays/services/k8s-reaper/`: namespace, HelmRelease pointing at
     `charts/k8s-reaper`, values.
   - The namespace is excluded from the reaper by construction (own namespace).
   - Discord routing already exists; the PrometheusRule severity labels select
     it.
3. **Rollout**:
   1. Report mode for at least a week. Compare reports against SC-006.
   2. Flip `spec.mode: full` in Git.
   3. Optionally start with `includeNamespaces: [fathom-dev]`.

## Delivery order (each a signed commit)

1. Workspace scaffold, Makefile/gc, Dockerfile, CI lint/test (no behaviour).
2. US1: core detection + decide + report executor, kube snapshot + writer
   (annotations/labels only), events, metrics, http. **MVP: deployable in
   report mode.**
3. US2: reap steps, Flux adapter, limits.
4. US3: restore paths.
5. US4: config hot-reload, mode labels, include/exclude, invalid-config
   handling. Partly needed earlier: US1 runs on defaults until this lands.
6. Docs: guides, CONTRIBUTING; chart PR in helm-charts; HelmRelease in fluxified.

## Complexity Tracking

| Violation                          | Why needed                                                         | Simpler alternative rejected because                                       |
| ---------------------------------- | ------------------------------------------------------------------ | -------------------------------------------------------------------------- |
| A CRD (`ReaperConfig`)             | Validated runtime config with a status (FR-009, FR-024, FR-026)    | ConfigMap: no schema validation and no status; env: needs restarts         |
| Two metric exporters (OTLP + Prom) | House standard is OTel; the alert path is Prometheus scrape        | OTLP-only needs a collector→Prometheus hop in the alert path               |
