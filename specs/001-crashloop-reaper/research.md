# Research: MVP Crashloop Reaper

Each entry: decision, why, what else was considered. Facts about roanoke were
checked read-only on 2026-10-10.

## R1 — What the target cluster actually looks like

- **Findings**:
  - k3s `v1.36.5+k3s1`, two nodes. Flux Kustomizations: `flux-system/roanoke`,
    `flux-system/slam-tenant`, and `slam-flux/slam-{core,dev,prod}`.
  - The six crash-looping `slam-prod` Deployments are owned Pod → ReplicaSet →
    Deployment, `replicas: 1`, with about 850 restarts in 3 days. Their field
    managers are `kubectl-client-side-apply` and `kubectl-rollout`. They carry
    **no** `kustomize.toolkit.fluxcd.io/*` labels today, so for them the reaper
    only scales; there is no Flux step.
  - `fathom-dev/crashloop` (a deliberate sandbox crash-looper, more than 5,000
    restarts) is a ready-made test target. `fathom-dev/bad-image` is in
    `ImagePullBackOff`, a useful negative case.
  - `slam-prod/ingest-*` pods are owned by Jobs (CronJob) and stay out of scope.
- **Consequence**: the plan supports three cases from day one: no Flux, Flux
  kustomize, and Flux Helm. The live crash-loopers exercise only the first.

## R2 — Shape of the controller: periodic level-triggered cycle

- **Decision**: Keep watch-backed caches (kube-runtime reflectors) of Pods,
  ReplicaSets, Deployments, StatefulSets, DaemonSets, Namespaces, HPAs,
  HelmReleases and the `ReaperConfig`. Every `cycleInterval` (default 60 s),
  take a snapshot, run the pure core `decide(snapshot, config, now)`, and
  execute the resulting plan. A change to a `reaper.boop.ninja/restore`
  annotation, or to replicas on a reaped workload, triggers an immediate cycle.
- **Rationale**: Rate limiting, the reaped ceiling and deterministic ordering
  (FR-017) are properties of the **whole** cycle, not of one object. A
  per-object `Controller::reconcile` would need shared mutable counters and
  would make ordering depend on event timing. A snapshot also makes the core a
  pure function (constitution I and III).
- **Alternatives**: kube-runtime `Controller` per Deployment. Rejected for the
  reasons above. Polling with `list` calls every cycle was rejected as wasteful;
  reflectors give the same snapshot from a cache.

## R3 — Detecting "crash-looping" and how long

- **Decision**:
  - A container is crash-looping if it is not ready, and either
    `state.waiting.reason == CrashLoopBackOff` or
    `state.terminated`/`lastState.terminated` has a non-zero exit code or reason
    `Error`/`OOMKilled`. This covers both the backoff wait and the brief
    "Error" phase between restarts (`fathom-dev/crashloop` shows `Error` in
    `kubectl get pods`).
  - The workload's crashloop start is persisted as annotation
    `reaper.boop.ninja/crashloop-since` on the workload. Its first healthy
    observation is persisted as `reaper.boop.ninja/healthy-since`. State is
    cleared only when `now - healthy-since >= recoveryWindow`.
- **Rationale**: Kubernetes does not record when CrashLoopBackOff began, and pod
  conditions flap with each restart. Restart count alone does not give a
  duration. A persisted first-seen time survives operator restarts (edge case)
  and is visible to people. Pods are not annotated because they are replaced on
  every rollout.
- **Alternatives**:
  - In-memory first-seen: lost on restart.
  - Inferring the duration from the backoff schedule: fragile and kubelet
    version dependent.
  - Pod age: wrong for pods that ran fine for weeks, then started crashing.

## R4 — Kustomize-managed workloads

- **Decision**: If both `kustomize.toolkit.fluxcd.io/name` and
  `kustomize.toolkit.fluxcd.io/namespace` labels are present, set
  `kustomize.toolkit.fluxcd.io/reconcile: disabled` on the in-cluster object
  **before** scaling, in the same merge patch that writes the reap record.
- **Rationale**: kustomize-controller server-side applies with force. Without
  this, the next reconcile (at most the Kustomization's `interval`) takes
  `spec.replicas` back. With the annotation the controller skips the object
  entirely. Writing the annotation and the record in one patch makes the step
  atomic.
- **Known effect**: while disabled, fixes pushed to Git do not reach the
  object. That is why restore exists and the guide says "restore, then fix".
- **To verify in integration**: that the Kustomization does not prune the
  disabled object, and that a Kustomization with `force: true` still honours
  the annotation.

## R5 — HelmRelease-managed workloads

- **Decision**: If both `helm.toolkit.fluxcd.io/name` and
  `helm.toolkit.fluxcd.io/namespace` labels are present, merge-patch the
  HelmRelease (`helm.toolkit.fluxcd.io/v2`):
  - set `spec.suspend: true`, but only if it is not already true;
  - add the workload to annotation `reaper.boop.ninja/held-for`;
  - set `reaper.boop.ninja/suspended-by-reaper`;
  - and if the HelmRelease itself has kustomize labels, set the reconcile
    annotation as in R4 and record its previous value in
    `reaper.boop.ninja/previous-reconcile`.

  All of this is one patch, made before the scale.
- **Rationale**:
  - helm-controller with drift detection, or any upgrade, re-renders
    `replicas`. Suspension stops both.
  - The HelmRelease is normally applied from Git by a Kustomization. If Git
    states `suspend: false`, kustomize-controller would flip it back, hence
    the reconcile annotation on the HelmRelease too.
  - Several workloads can share one release, so it is a reference-counted hold.
- **Alternatives**:
  - Patching values (`replicaCount: 0`) on the HelmRelease: chart-specific,
    and it rewrites the owner's desired state.
  - Suspending the parent Kustomization: far too wide.
- **Trade-off accepted**: suspending a HelmRelease freezes every resource in
  that release, not just the reaped workload. That is recorded in the Event
  message and the user guide.
- **Types**: Flux publishes no Rust types. The Flux adapter declares a minimal
  `HelmRelease` struct (metadata + `spec.suspend`) and talks to the API through
  `kube::Api<DynamicObject>`/typed `Api` with an `ApiResource`. Only the fields
  it reads are modelled; it never round-trips the whole spec.

## R6 — Order of writes, idempotency and restore

- **Decision**: A reap runs as idempotent steps. Each step is detectable from
  object state, so an interrupted reap resumes instead of repeating:
  1. Patch the workload with `original-replicas`, `reason`, `plan`, state label
     `reaping`, and (if kustomize-managed) the reconcile annotation plus
     `previous-reconcile`.
  2. If Helm-managed, place the HelmRelease hold (R5) and set
     `reaper.boop.ninja/helmrelease` on the workload.
  3. Scale to 0 through the `scale` subresource.
  4. Set `reaped-at` and state label `reaped`; emit `Reaped`.

  A restore (annotation, or manual scale-up while `reaped`) runs:
  1. Release the HelmRelease hold. Unsuspend only when `held-for` is empty and
     `suspended-by-reaper` is `true`, and put the HelmRelease's reconcile
     annotation back.
  2. Scale to `original-replicas` (skipped for a manual scale-up).
  3. In one patch, put the workload's reconcile annotation back (delete it if
     the recorded previous value was none) and remove every `reaper.boop.ninja/*`
     annotation and label. Emit `Restored`.
- **Rationale**: Flux is blocked before replicas change, and replicas come back
  before Flux is re-enabled. A crash between restore steps 2 and 3 leaves a
  `reaped` workload with replicas > 0. That is exactly the manual scale-up case,
  so the next cycle finishes the cleanup.
- **Concurrency**: patches carry `metadata.resourceVersion` preconditions. A
  conflict fails the step, which is retried next cycle against fresh state.
  Field manager is `k8s-reaper`.
- **Alternatives**: Store reap records in a CR or ConfigMap. Rejected: two
  sources of truth, and the owner asked for annotations on the object.

## R7 — Autoscalers and other controllers

- **Decision**:
  - HPA targeting the workload: allowed. The HPA stops acting while the target
    is at 0, and restore re-establishes the replica count.
  - KEDA `ScaledObject` targeting it (looked up only if the CRD is installed):
    `ManagedByAutoscaler`, report-only.
  - Deployment or StatefulSet with a controller `ownerReference`:
    `ManagedByController`, report-only.
- **Rationale**: constitution VI, never race another controller.

## R8 — Configuration: a cluster-scoped CRD

- **Decision**: CRD `ReaperConfig` (`reaper.boop.ninja/v1alpha1`, cluster-scoped),
  watched by name (`default`). The spec holds behaviour; the status holds the
  summary required by FR-024. Process concerns (bind address, log format, OTLP
  endpoint, own namespace) are flags/env. The CRD YAML is generated from the
  Rust type (`k8s-reaper crd` subcommand) and committed to the chart.
- **Rationale**:
  - Schema validation at admission, so typos are refused before they reach the
    reaper.
  - A status subresource for `kubectl get reaperconfig`.
  - Flux applies it from Git like any object, which gives runtime flags
    (constitution V).
- **Alternatives**:
  - ConfigMap: no schema, no status.
  - Env only: changes need a pod restart, against FR-009.
  - Per-namespace CRs: more than the MVP needs; namespace labels cover opt-out.

## R9 — Telemetry and the HTTP surface

- **Decision**:
  - The core defines a `Telemetry` port (record state, action result, cycle).
  - The `reaper-observability` crate implements it twice:
    1. OpenTelemetry metrics and traces (`opentelemetry`, `opentelemetry_sdk`,
       `opentelemetry-otlp`), enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set
       (Alloy in-cluster can receive OTLP).
    2. A Prometheus text endpoint using `prometheus-client`.
  - `tracing` with `tracing-opentelemetry` for spans; JSON logs.
  - Axum + Tower serve `/healthz`, `/readyz` and `/metrics` on `:8080`.
- **Rationale**: Discord alerts on roanoke come from kube-prometheus-stack
  scraping ServiceMonitors, so a pull endpoint is required. OTel is the house
  standard. A port keeps both behind one call site. The Rust
  `opentelemetry-prometheus` bridge has been unmaintained/deprecated at times,
  so it is not depended on.
- **Alternatives**: OTLP only, with a collector converting to Prometheus.
  Rejected: it adds a moving part to the alert path.

## R10 — Events

- **Decision**:
  - Use `events.k8s.io/v1` through `kube::runtime::events::Recorder`, with
    reporting controller `reaper.boop.ninja/k8s-reaper`.
  - Emit on state **transitions** and on a change of skip reason, not every
    cycle.
- **Rationale**: Events stay readable and the API server is not spammed. The
  Recorder already aggregates repeats into series.

## R11 — Toolchain, workspace and packaging

- **Decision**:
  - Rust stable 1.99 and edition 2024, pinned in `rust-toolchain.toml`.
  - Cargo workspace with `resolver = "3"`.
  - `kube` + `k8s-openapi` (feature `v1_36`, or the newest feature the
    published crate offers), using `rustls` with no OpenSSL.
  - Multi-stage Dockerfile: `lukemathwalker/cargo-chef` planner → cook →
    builder, then `gcr.io/distroless/cc-debian12:nonroot`.
  - Makefile `gc` per the constitution.
- **Rationale**: house rules. With rustls the runtime image needs no libssl.
- **Alternatives**: musl static + `scratch`. A valid later optimisation, but
  musl's allocator hurts and the size win is small.

## R12 — Testing strategy

- **Decision**:
  1. **Core unit tests** (TDD): table-driven, built from small fixture
     builders, with time as a parameter. Each test asserts the plan, and
     (SC-004) that report mode publishes byte-identical plan JSON.
  2. **Adapter contract tests**: snapshot mapping from recorded JSON objects
     (including the live `slam-prod` shapes, scrubbed), and patch bodies
     asserted against a `tower-test` mock API server.
  3. **Integration**: k3d with Flux installed, `#[ignore]`d and run by
     `make test-integration`, segmented per user story
     (`--test us1_report`, …). It covers SC-002, SC-003, SC-005, R4 and R5.
- **Rationale**: constitution III. Integration runs separately because it
  needs Docker and takes minutes.

## R13 — Fit with fluxified's guardrails

- The reaper's ServiceAccount is not a `slam-*` identity, so
  `tests/test_rbac_guardrails.py` and `overlays/security/rbac-guard/` need no
  `approved-bindings.yaml` entry. Its ClusterRole binding will still appear in
  the cluster audit trail like any cluster-scoped RBAC.
- Its patches to `slam-*` workloads will show up in the audit log under its
  ServiceAccount. That is the intended trail, and it is what SC-005 checks
  against.
