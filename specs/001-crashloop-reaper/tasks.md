# Tasks: MVP Crashloop Reaper

**Input**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/](./contracts/), [quickstart.md](./quickstart.md)

**Tests**: Required. The constitution (III) makes core logic test-first. Within
each story, test tasks come first and must fail before the implementation
tasks that follow.

Format: `- [ ] T### [P?] [US?] description with path`. `[P]` means it can run in
parallel (different files, no unfinished dependency).

## Phase 1: Setup

- [X] T001 Create workspace `Cargo.toml` (resolver 3, edition 2024, shared `[workspace.dependencies]` and `[workspace.lints]` with clippy pedantic warnings) and `rust-toolchain.toml` (stable 1.99, rustfmt, clippy)
- [X] T002 Create empty crates `crates/reaper-core`, `crates/reaper-kube`, `crates/reaper-flux`, `crates/reaper-observability` and binary `apps/k8s-reaper` (clap skeleton with `run` and `crd` subcommands); `reaper-core` has no dependency other than `serde`, `serde_json`, `thiserror`
- [X] T003 [P] Write `Makefile`:
  - targets `build` (depends on `gc`), `test`, `test-integration` (`STORY=` filter), `lint`, `image`, `dev-cluster`, `dev-load`, `dev-run`;
  - `gc` runs `cargo sweep --time $(GC_DAYS)` (default 14), or prints `cargo install cargo-sweep --locked` when it is missing;
  - `gc` also runs `docker builder prune -f --max-used-space $(GC_DOCKER_KEEP)` (default 40GB), skipped with a note if `docker info` fails;
  - `gc` never exits non-zero.
- [X] T004 [P] Write `Dockerfile`: `cargo-chef` planner → cook → builder (release, `--locked`), runtime `gcr.io/distroless/cc-debian12:nonroot`, `ENTRYPOINT ["/k8s-reaper"]`, `USER nonroot`
- [X] T005 [P] Add `.github/workflows/ci.yml`: fmt, clippy `-D warnings`, `cargo test`, docker build (no push)
- [X] T006 [P] Add a dependency guard test `crates/reaper-core/tests/no_infra_deps.rs`. It parses `cargo metadata` and fails if `reaper-core` depends on `kube`, `k8s-openapi`, `tokio` or `chrono`'s `clock` feature.

## Phase 2: Foundational (blocks all stories)

- [X] T010 Write tests first in `crates/reaper-core/src/config.rs`: defaults match data-model; duration parsing (`90s`, `30m`, `1h30m`, invalid); range validation returns all field errors
- [X] T011 Implement `Config`, `Mode` (`Off < Report < Full`), duration parsing and validation in `crates/reaper-core/src/config.rs`
- [X] T012 [P] Implement the core input model (`Snapshot`, `WorkloadView`, `PodView`, `ContainerView`, `FluxRefs`, `HelmReleaseView`, `NamespaceView`) and test fixture builders in `crates/reaper-core/src/model.rs` and `crates/reaper-core/src/testing.rs` (`#[cfg(test)]` + `pub` behind a `testing` module for adapter tests)
- [X] T013 Write tests first, then implement the annotation codec in `crates/reaper-core/src/record.rs`. It encodes and decodes every key in contracts/annotations.md, round-trips, and rejects malformed values without panicking.
- [X] T014 [P] Define `Plan`, `Action`, `SkipReason`, `Transition`, `CyclePlan`, with stable JSON serialisation (snapshot tests via `insta`), in `crates/reaper-core/src/plan.rs`
- [X] T015 [P] Define ports `ClusterWriter`, `EventSink` and `Telemetry` (native `async fn`, no runtime) in `crates/reaper-core/src/ports.rs`, with in-memory fakes in `crates/reaper-core/src/testing.rs`
- [X] T016 [P] Define the `ReaperConfig` CustomResource (spec/status per contracts/config.md, `schemars` ranges and enums) and its conversion to `reaper_core::Config` in `crates/reaper-kube/src/crd.rs`. Wire `k8s-reaper crd` to print the YAML. Add a golden-file test `crates/reaper-kube/tests/crd_golden.rs`.
- [X] T017 [P] Set up tracing in `crates/reaper-observability/src/tracing.rs`: JSON/text logs, OTLP traces when `OTEL_EXPORTER_OTLP_ENDPOINT` is set
- [X] T018 [P] Add the Axum server in `crates/reaper-observability/src/http.rs` with `/healthz`, `/readyz` (readiness flag from a shared handle) and `/metrics`; add a unit test using `tower::ServiceExt::oneshot`

**Checkpoint**: core types, config, codec, ports and HTTP skeleton exist; `make test` is green.

## Phase 3: User Story 1 — report every long crash loop, change nothing (P1) 🎯 MVP

**Goal**: in report mode, candidates show up as Events, metrics, state labels
and a `plan` annotation. No replicas or Flux object changes.

**Independent test**: quickstart §2. Integration `apps/k8s-reaper/tests/us1_report.rs`.

### Tests first

- [X] T020 [P] [US1] `crates/reaper-core/src/detect.rs` tests:
  - FR-001 container classification: waiting CrashLoopBackOff, terminated Error/OOMKilled, init container, ImagePullBackOff (no), ready (no);
  - FR-002 owner resolution: Deployment via ReplicaSet, StatefulSet, DaemonSet, Job (ignored), bare pod (unsupported).
- [X] T021 [P] [US1] `crates/reaper-core/src/decide.rs` tests, US1 scenarios 1–5:
  - under duration → `observed`; under restarts → `observed`; both met → `candidate`;
  - flapping does not reset `crashloop-since`; recovery only after `recoveryWindow` (FR-003);
  - DaemonSet → `CannotScaleToZero`; `PartiallyAvailable`; `ManagedByController`; `ManagedByAutoscaler` (KEDA);
  - excluded namespace, own namespace, `includeNamespaces` globs → nothing.
- [X] T022 [P] [US1] Dry-run-equivalence harness in `crates/reaper-core/tests/equivalence.rs` (SC-004). For every fixture in the decide tests, it runs the executor with fake ports in `Report` and `Full` and asserts identical published plan JSON. In `Report`, the fake `ClusterWriter` must receive only `reaper.boop.ninja/*` annotation/label writes (FR-012).
- [X] T023 [P] [US1] Snapshot-mapping contract tests in `crates/reaper-kube/tests/snapshot.rs`. They use JSON fixtures in `crates/reaper-kube/tests/fixtures/` taken from scrubbed live shapes (shop-prod Deployment/ReplicaSet/Pod, sandbox crashloop, Job pod, ImagePullBackOff pod).
- [X] T024 [P] [US1] Writer contract test in `crates/reaper-kube/tests/writer.rs` using a `tower-test` mock. Annotation/label merge patches carry a `resourceVersion` precondition and field manager `k8s-reaper`.

### Implementation

- [X] T025 [US1] Implement container classification and owner resolution in `crates/reaper-core/src/detect.rs`
- [X] T026 [US1] Implement `decide()` for the observe/candidate/recover/skip paths. Ordering is deterministic (longest crash-looping first, then namespace/name), and the `would` actions are computed for every candidate (scale, kustomize, HelmRelease per FluxRefs and `flux.*` toggles, FR-027). Code goes in `crates/reaper-core/src/decide.rs`.
- [X] T027 [US1] Implement the executor's report path in `crates/reaper-core/src/executor.rs`: publish the plan annotation, state label, transition Events and telemetry; the mode gate stops before any `Action` that is not a reaper-annotation write
- [X] T028 [US1] Implement reflectors and snapshot building in `crates/reaper-kube/src/snapshot.rs`:
  - Pods, ReplicaSets, Deployments, StatefulSets, DaemonSets, Namespaces, HPAs;
  - KEDA ScaledObjects only if the CRD is discovered;
  - HelmReleases via the Flux adapter type;
  - mode labels/annotations read from namespaces and workloads.
- [X] T029 [US1] Implement `ClusterWriter` for annotation and label patches in `crates/reaper-kube/src/writer.rs`
- [X] T030 [P] [US1] Implement `EventSink` over the `events.k8s.io/v1` Recorder (reporting controller `reaper.boop.ninja/k8s-reaper`) in `crates/reaper-kube/src/events.rs`
- [X] T031 [P] [US1] Implement `Telemetry` in `crates/reaper-observability/src/telemetry.rs`: every metric in contracts/metrics.md via `prometheus-client`, mirrored to OTel meters, with stale series removed when a state clears. Add a test that the exposition contains exactly the contracted names and labels.
- [X] T032 [US1] Wire `run` in `apps/k8s-reaper/src/main.rs`:
  - clap flags/env per contracts/config.md;
  - build adapters, then the loop: wait for caches, snapshot, `decide`, executor;
  - interval from config; readiness set after first cycle;
  - built-in defaults (report mode) while ReaperConfig is absent.
- [X] T033 [US1] Add k3d dev setup and fixtures in `deploy/dev/`: `k3d.yaml`, Flux install, dev ReaperConfig with short thresholds, and the fixtures table from quickstart §1
- [X] T034 [US1] Write integration test `apps/k8s-reaper/tests/us1_report.rs` (`#[ignore]`). It covers US1 scenarios 1–5, and checks that replicas and Flux objects are unchanged after N cycles (SC-001).

**Checkpoint**: deployable in report mode. This is the MVP increment.

## Phase 4: User Story 2 — reap and hold Flux off (P2)

**Goal**: full mode scales candidates to 0 and blocks Flux, recording every step.

**Independent test**: quickstart §3. Integration `apps/k8s-reaper/tests/us2_reap.rs`.

### Tests first

- [X] T040 [P] [US2] `decide.rs` tests:
  - reap action list per case: plain, kustomize, Helm, Helm whose HelmRelease is kustomize-managed, HelmRelease already suspended, reconcile annotation already set (FR-013 to FR-015);
  - `RateLimited` and `ReapCeilingReached` with deterministic ordering (FR-017);
  - Flux toggle off on a Flux-managed workload → `ReportOnlyScope` (FR-027).
- [X] T041 [P] [US2] `executor.rs` tests with fake ports:
  - step order (record + kustomize disable → HelmRelease hold → scale → mark reaped);
  - resume after failure at each step without repeating a completed step (FR-016);
  - `reaping` with replicas > 0 continues and is not taken as a restore.
- [X] T042 [P] [US2] Flux adapter contract tests in `crates/reaper-flux/tests/holds.rs`. Using a mock API, they check the HelmRelease merge patch body: `spec.suspend` only when not already suspended, the `held-for` set is sorted and deduplicated, `suspended-by-reaper`, and the reconcile annotation on a kustomize-managed HelmRelease.

### Implementation

- [X] T043 [US2] Extend `decide()` with the `Reap` plan (remaining steps only), limits and the ceiling in `crates/reaper-core/src/decide.rs`
- [X] T044 [US2] Implement the executor's full path with per-step recording in `crates/reaper-core/src/executor.rs`
- [X] T045 [P] [US2] Implement the minimal HelmRelease v2 type and `ApiResource` in `crates/reaper-flux/src/helmrelease.rs`
- [X] T046 [US2] Implement HelmRelease hold/release patches in `crates/reaper-flux/src/holds.rs` (part of the `ClusterWriter` impl, composed in the app)
- [X] T047 [US2] Implement scale via the `scale` subresource and the kustomize reconcile annotation patch in `crates/reaper-kube/src/writer.rs`
- [X] T048 [US2] Write integration test `apps/k8s-reaper/tests/us2_reap.rs` (`#[ignore]`). It covers plain, kustomize and Helm fixtures; replicas stay 0 through three forced Flux reconciles (SC-002); `PartiallyAvailable` is untouched; and it verifies research R4's prune/force questions.

**Checkpoint**: full mode works; restore is not done yet, so do not enable full mode outside dev.

## Phase 5: User Story 3 — one-step restore (P2)

**Goal**: one annotation, or a manual scale-up, returns the workload exactly to
its pre-reap state.

**Independent test**: quickstart §4. Integration `apps/k8s-reaper/tests/us3_restore.rs`.

### Tests first

- [X] T050 [P] [US3] `decide.rs` and `executor.rs` tests:
  - restore by annotation and by manual scale-up (keeps replicas);
  - previous reconcile value restored vs. deleted (`<none>`);
  - shared HelmRelease stays held until the last workload is restored;
  - HelmRelease suspended by a person stays suspended;
  - restore on an un-reaped workload → `RestoreIgnored`;
  - restore runs in report mode and is not rate-limited (FR-018 to FR-020);
  - a crash between restore steps converges;
  - `mode=off` on a reaped workload restores first, then ignores it.
- [X] T051 [P] [US3] HelmRelease release contract test in `crates/reaper-flux/tests/holds.rs`: unsuspend only when `held-for` is empty and `suspended-by-reaper=true`; clean up `reaper.boop.ninja/*` keys

### Implementation

- [X] T052 [US3] Implement the `Restore` plan and the deleted/recreated-workload cleanup of `held-for` in `crates/reaper-core/src/decide.rs`
- [X] T053 [US3] Implement restore execution in `crates/reaper-core/src/executor.rs` and the release patch in `crates/reaper-flux/src/holds.rs`
- [X] T054 [US3] Trigger an immediate cycle on a `restore` annotation change, or a replicas change on a `reaped` workload, from the watch stream in `crates/reaper-kube/src/snapshot.rs` and `apps/k8s-reaper/src/main.rs`
- [X] T055 [US3] Write integration test `apps/k8s-reaper/tests/us3_restore.rs` (`#[ignore]`). It checks an exact pre-reap state diff after restore for each fixture (SC-003).

**Checkpoint**: full mode is safe to enable.

## Phase 6: User Story 4 — runtime control of mode and scope (P3)

**Goal**: config changes take effect without a restart; labels can lower the
mode; invalid config is refused safely.

**Independent test**: integration `apps/k8s-reaper/tests/us4_config.rs`.

### Tests first

- [X] T060 [P] [US4] `decide.rs` tests: effective mode = min(global, namespace, workload); a label can never raise it; an unknown label value is ignored and reported (FR-010)
- [X] T065 [P] [US4] `decide.rs` tests for threshold overrides (FR-034): the larger of override and global applies; a lower override is ignored; out-of-range or unparseable values are ignored and reported once
- [X] T061 [P] [US4] Config watcher tests in `crates/reaper-kube/tests/config_watch.rs`: a new valid generation becomes active; an invalid one keeps the last valid config and sets `Valid=False`; deletion falls back to defaults (FR-026)

### Implementation

- [X] T062 [US4] Implement the ReaperConfig watcher with last-valid-config retention and status/conditions/counts patching (FR-024) in `crates/reaper-kube/src/crd.rs` and `crates/reaper-kube/src/config_watch.rs`
- [X] T066 [US4] Implement threshold overrides in `crates/reaper-core/src/decide.rs` (read from the workload's person-set annotations, carried in `WorkloadView`)
- [X] T063 [US4] Emit `ConfigApplied`/`ConfigInvalid` Events and the `reaper_config_*` metrics from `apps/k8s-reaper/src/main.rs` wiring
- [X] T064 [US4] Write integration test `apps/k8s-reaper/tests/us4_config.rs` (`#[ignore]`): mode flip without restart, namespace `mode=report`, workload `mode=off`, invalid config, own namespace never touched (SC-005)

## Phase 7: User Story 5 — pause a CronJob that keeps failing (P3)

**Goal**: a CronJob whose every run has failed for `cronJobs.failingDuration`
(default 6h) with at least `cronJobs.minFailedRuns` (default 3) is suspended in
full mode and reported in report mode; one restore action resumes it.

**Independent test**: quickstart fixture `cron/failing` with the dev
thresholds; SC-008.

### Tests first

- [x] T080 [P] [US5] `detect.rs` tests: the streak starts at the first failed Job after `lastSuccessfulTime`; distinct failed Jobs are counted once via `last-failed-job`; running Jobs are ignored; a success ends the streak; Job pods still never count as crash loops
- [x] T081 [P] [US5] `decide.rs` tests:
  - candidate at exactly 6h and 3 runs; not at 5h59m, and not with 2 runs after 3 days;
  - already suspended by someone else → state cleared, never a candidate;
  - `ManagedByController`, opt-out and `ReportOnlyScope` apply, `PartiallyAvailable`/`CannotScaleToZero`/`ManagedByAutoscaler` never do;
  - shared limits and ceiling with Deployments; Flux holds as for Deployments;
  - restore annotation and manual `suspend: false` → `Restore`; `cronJobs.enabled: false` stops observation but not restore
- [x] T082 [P] [US5] Add CronJob fixtures to the equivalence harness `crates/reaper-core/tests/equivalence.rs` (report writes only reaper keys; identical plan JSON in both modes)
- [x] T083 [P] [US5] Kube contract tests: CronJob + Job fixtures (failed, succeeded, active, history-limited) in `crates/reaper-kube/tests/snapshot.rs`; the `spec.suspend` merge patch with resourceVersion precondition in `crates/reaper-kube/tests/writer.rs`

### Implementation

- [x] T084 [US5] Extend `WorkloadKind` with `CronJob`, add `CronJobView`/`JobView` to `crates/reaper-core/src/model.rs`, and the `failing-since`, `failed-runs`, `last-failed-job` keys to `crates/reaper-core/src/record.rs`
- [x] T085 [US5] Add `cronJobs` (`enabled`, `failingDuration` 10m–7d, `minFailedRuns` 1–1000) to `crates/reaper-core/src/config.rs` and the CRD in `crates/reaper-kube/src/crd.rs`; regenerate the CRD golden file
- [x] T086 [US5] Implement CronJob streak detection in `detect.rs` and the CronJob paths in `decide.rs` (`SuspendCronJob`/`ResumeCronJob` actions)
- [x] T087 [US5] Execute `SuspendCronJob`/`ResumeCronJob` through `ClusterWriter::set_cronjob_suspend` in `crates/reaper-core/src/executor.rs`; emit `JobFailuresObserved`/`JobFailuresRecovered`; `cronjob_suspend`/`cronjob_resume` metrics
- [x] T088 [US5] CronJob and Job reflectors in `crates/reaper-kube/src/snapshot.rs` and the suspend patch in `crates/reaper-kube/src/writer.rs`; trigger an immediate cycle when a reaped CronJob's `spec.suspend` turns false
- [x] T089 [US5] Add the `cron/failing` dev fixture to `deploy/dev/` and write integration test `apps/k8s-reaper/tests/us5_cronjob.rs` (`#[ignore]`) for SC-008

**Checkpoint**: SC-008 passes on k3d.

## Phase 8: Polish and delivery

- [X] T070 [P] Write `CONTRIBUTING.md`:
  - setup: toolchain, cargo-sweep, k3d, Flux CLI for dev;
  - architecture (ports and adapters diagram) and decision log (link research.md);
  - testing tiers, `make` targets, release process;
  - signed commits.
- [X] T071 [P] Write `docs/guides/getting-started.md` (install via Flux HelmRelease, stay in report mode, read the reports, flip to full)
- [x] T072 [P] Write `docs/guides/restoring-a-workload.md`: the one restore command, manual scale-up, manual CronJob resume, "restore then fix" with Flux, shared HelmReleases
- [x] T073 [P] Write `docs/guides/configuration.md`: every ReaperConfig field (including `cronJobs`), opt-out labels, examples for `shop-*`
- [X] T074 Measure memory and cycle time against the plan's goals (< 64 MiB, < 1 s for 300 pods) with a synthetic snapshot benchmark in `crates/reaper-core/benches/decide.rs`
- [x] T075 Run quickstart.md end to end on k3d and tick SC-001 to SC-005, SC-007, SC-008
  - 2026-10-10 on kind-dev: `make test` 6.1 s (SC-004, SC-007); `make test-integration`, all six stories in one run:
    us1 60 s (SC-001), us2 68 s (SC-002), us3 501 s (SC-003), us4 385 s (SC-005), us5 506 s (SC-008), us6 60 s (SC-009). SC-006 checked on the production cluster under T077.
- [X] T076 Chart PR in `mbround18/helm-charts` (`charts/k8s-reaper`): CRD from `k8s-reaper crd`, Deployment (Recreate, 1 replica, non-root, read-only fs), RBAC per contracts/config.md, Service, ServiceMonitor, PrometheusRule per contracts/metrics.md, `ReaperConfig/default` from values (`mode: report`), notifications Secret mount (`notifications.secretName`, `optional: true`, see docs/guides/notifications.md). (Outside this repo.) Merged as mbround18/helm-charts#251 (337d9b6).
- [X] T077 Fluxified overlay `overlays/services/k8s-reaper/` (namespace, HelmRelease → `charts/k8s-reaper`), then `uv run invoke build test`; deploy in report mode and check SC-006. (Outside this repo, after T076 is merged to the chart repo's `main`.) Done in fluxified d37accc; SC-006 checked on the production cluster in report mode (all 8 expected objects flagged, nothing else labelled), then switched to full mode on 2026-10-11.

## Phase 9: User Story 6 — notifications to Discord, webhooks and mail (P3)

**Goal**: optional chat/mail messages on transitions, from channel URLs in a
mounted Secret. Off, with no outbound connection, when nothing is mounted.

**Decided**: spec.md Question 6, answered 2026-10-10 (`reqwest` + `lettre`,
mounted Secret, `ReapPlanned` opt-in).

**Independent test**: spec.md US6. Integration `apps/k8s-reaper/tests/us6_notify.rs`.

### Tests first

- [X] T090 [P] [US6] `crates/reaper-core/src/notify.rs` tests: the `Notification` built from a transition carries reason, workload, state, mode and the Event note; reason filters (global and per channel) and `minInterval` de-duplication are pure functions of time and history (FR-037)
- [X] T091 [P] [US6] `crates/reaper-notify/tests/channels.rs`: URL → provider inference and `format=` override, `http://` and plain SMTP refused, `from`/`to` parsing, files starting with `.` ignored, and no error or `Debug` output that contains the URL or credentials (FR-035, FR-036, FR-039)
- [X] T092 [P] [US6] `crates/reaper-notify/tests/delivery.rs` against a local mock HTTP server: Discord, Slack and generic payload shapes match contracts/notifications.md; timeout, 429 `Retry-After`, non-retried 4xx, bounded retries, queue overflow drops oldest (FR-038, FR-040)

### Implementation

- [X] T093 [US6] Add the `Notifier` port and `notify.rs` filtering to `reaper-core` (no new dependencies); the executor hands transitions to it after writes, alongside `EventSink`
- [X] T094 [US6] Add `notifications` to `ReaperConfig.spec` and core `Config` with validation (`reasons` are known Event reasons, `minInterval` 0–24h), in `crates/reaper-kube/src/crd.rs` and `crates/reaper-core/src/config.rs`
- [X] T095 [US6] Create adapter crate `crates/reaper-notify`: channel directory loader (re-read each cycle), Discord/Slack/generic webhook senders, SMTP sender, bounded queue worker with retry/backoff; secrets wrapped in a redacting type
- [X] T096 [US6] Add `reaper_notifications_total` and `reaper_notification_channels` to `crates/reaper-observability/src/telemetry.rs` and the exposition test
- [X] T097 [US6] Wire `--notifications-dir` / `REAPER_NOTIFICATIONS_DIR` and the worker into `apps/k8s-reaper/src/main.rs`; graceful shutdown drains the queue for at most 5 s
- [X] T098 [US6] Dev: echo server and mail catcher in `deploy/dev/notify/` (e.g. a small HTTP echo and Mailpit), a sample Secret using `example.test` hosts, and integration test `apps/k8s-reaper/tests/us6_notify.rs` (`#[ignore]`) for SC-009 (mounting the Secret into the reaper waits on G02, `deploy/dev/reaper.yaml`)
- [X] T099 [P] [US6] `docs/guides/notifications.md`: creating the Secret, one example per provider, choosing reasons; and chart values (Secret name, `optional: true` mount) noted for T076

**Checkpoint**: SC-009 passes on k3d.

## Gaps to chase

Found while building US1 (2026-10-10). Each is a known shortfall against the
spec, contracts or quickstart. Close one by fixing it, or by changing the spec
with the owner's agreement and saying so in the commit.

### Dev setup and quickstart

- [X] G01 Run `make dev-cluster` end to end once; confirm the pinned versions exist and work together (Flux `v2.7.0`, `rancher/k3s:v1.36.5-k3s1`, `busybox:1.36.1`) and fix what does not. Done on an existing kind cluster instead (`make dev-kind`, k8s 1.32, Flux source-controller v1.9.5): fixtures, registry and both Flux sources work. The pins exist; the k3d path itself (`k3d cluster create`, `flux install --version=v2.7.0`) has not run. Every dev target and test now names its context (`KUBE_CONTEXT`, `REAPER_TEST_CONTEXT`) and refuses anything but `kind-*`/`k3d-*` on loopback
- [X] G02 Add the missing `deploy/dev/reaper.yaml` (namespace `k8s-reaper`, ServiceAccount, ClusterRole/Binding per contracts/config.md, Deployment with downward-API `REAPER_NAMESPACE`) so `make dev-run` works; note in `deploy/dev/README.md` that `helm` is now required on the host
- [X] G03 quickstart §1 says the dev values are "the lowest values validation allows", but `MIN_RESTARTS_RANGE` and `minFailedRuns` start at 1. Fix the sentence (or the ranges, if the owner prefers 3)
- [X] G04 Add the quickstart fixtures that are missing from `deploy/dev/fixtures/`: a crash-looping StatefulSet, an `ImagePullBackOff` Deployment (must not be a candidate) and a healthy control Deployment (plus `cron/failing`, listed in quickstart §1 but missing)
- [X] G05 Make sure the CRD is applied before `make dev-cluster` waits on the dev ReaperConfig

### Kubernetes adapter

- [X] G06 Events carry no `regarding.uid`, so `kubectl describe` may not attach them; Events about cluster-scoped objects (ReaperConfig) land in `default`. Fill the uid from the snapshot and put cluster-scoped Events in the reaper's own namespace (contracts/events.md). Done: uid filled from the watch caches (CronJob and ReaperConfig have none until those are cached). The namespace half is not possible: the API server rejects an events.k8s.io/v1 Event about a cluster-scoped object in any namespace but `default` or `kube-system` ("involvedObject.namespace: does not match event.namespace", verified by server dry-run on kind 2026-10-10), so those stay in `default`
- [X] G07 `scale` has no `resourceVersion` precondition, unlike metadata patches. Add one, with a writer contract test (needed before T044 full mode). The scale merge patch now carries `metadata.resourceVersion`; a 409 maps to `Conflict`
- [X] G08 The reaper-flux `HelmRelease` type is minimal (enough for the snapshot). Reconcile it with T045 rather than writing a second one
- [X] G09 Crash-looping pods with no supported owner are only logged. Decide with the owner whether they get a metric or Event (spec.md is silent); record the decision in spec.md Edge Cases — owner chose a metric only: `reaper_unowned_crashlooping_pods{namespace}`

### Runtime and observability

- [X] G10 OTel metrics: enable the SDK/OTLP `metrics` features and install a global meter provider in `main`, so the meters mirrored in telemetry.rs are actually exported when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (contracts/metrics.md)
- [X] G11 `/readyz` must fail when the last cycle is older than 3× the interval (contracts/config.md HTTP); today readiness is set once after the first cycle
- [X] G12 Confirm `reaper_cycle_duration_seconds` bucket boundaries and whether the metric family should declare a unit; record in contracts/metrics.md — buckets kept, unit `seconds` declared
- [X] G13 Cycles run on a timer only; watch-triggered cycles are T054. Until then, document that reaction time is up to one `cycleInterval`
- [X] G14 ReaperConfig status, conditions and `ConfigInvalid`/`ConfigApplied` Events are not written until T062/T063; an invalid config is only logged and shown in `reaper_config_valid`

### Behaviour not yet built (tracked elsewhere, listed so nothing is assumed done)

- [X] G15 Full mode is gated in the executor until T044; a `Reaping` state currently plans nothing (`Plan::Nothing`) until US2/US3 — closed by T043/T044/T052
- [X] G16 Run the `#[ignore]` integration tests (T034 and later) on k3d; none has run yet. `us1_report` and `us6_notify` pass on kind (`make dev-kind … KUBE_CONTEXT=kind-dev`); later stories add their own
- [X] G17 FR-034's reading of Question 5 (per-workload overrides that only raise thresholds) is an interpretation; get the owner's confirmation and update spec.md Owner Decisions
- [X] G18 kube's event `Recorder` folds a repeat of the same object, reason, action and type into the first Event's series and keeps its note, so a second `ConfigApplied` still reads "generation 6 is active" and a later `ConfigInvalid` keeps the first error. The `Valid` condition and `activeGeneration` are correct; fixed: `KubeEventSink` keys the series on the note too, as client-go does

### Found while documenting (2026-10-11)

Specified as `specs/002-post-rollout-hardening/`; closed there.

Each was found by writing the guides against the code; the guides describe
today's behaviour and need updating when one closes.

- [X] G19 `/healthz` never fails: `Health::set_alive` is never called, so the liveness probe cannot restart a reaper whose cycle loop has panicked or hung (contracts/config.md HTTP). Set it false when the cycle task ends or panics, and when no cycle has started for a long time (well beyond the 3× readiness window); unit test in `reaper-observability/src/http.rs`, and a test in `run.rs` that a panicking cycle turns `/healthz` 503 — closed by 002 T120–T128 (US1)
- [X] G20 `/readyz` ignores the config: contracts/config.md says it needs a valid config to be active, but `run.rs` only calls `set_ready(true)`. Decide with the owner: (a) not ready while no valid config has ever been applied (a later invalid one keeps the last good one, so stays ready), or (b) drop the clause from the contract. Recommendation (a) — closed by 002 T120–T127 (US1; owner chose (a))
- [X] G21 The ReaperConfig `Ready` condition is always written `True` (`config_watch.rs` `READY`) and never `False`, so it goes stale rather than wrong-way. Write `False` with a reason (`Stalled`, `CachesNotSynced`) when the last cycle is older than 3× the interval, matching `/readyz`; update getting-started §4.4 and monitoring.md — closed by 002 T123–T126 (US1)
- [X] G22 A failed write of the reaper's own label or annotations (record, plan, state) emits no `ReapFailed` Event and no `reaper_actions_total{result="error"}`; only `reaper_cycles_total{result="error"}` and logs show it, and no alert fires. Count it under an action (`record`) with `result="error"` and emit a Warning Event, so `ReaperActionFailed` covers it (contracts/metrics.md, contracts/events.md) — closed by 002 T130, T132 (US2)
- [X] G23 `ReaperDown` uses `up{job="k8s-reaper"}` in the contract but `up{job="<release fullname>"}` in the chart, which only agree when the release name contains `k8s-reaper`; with the ServiceMonitor off the alert fires forever. Chart: render the job from the Service name and only install `ReaperDown` when the ServiceMonitor is enabled; contract: say the job is the Service name — closed by 002 T143 and chart PR T147 (US2/US5)
- [X] G24 False `ReaperStalled`/`ReaperConfigInvalid` at start-up: `reaper_last_cycle_timestamp_seconds` and `reaper_config_valid` read 0 until the first cycle. Leave them unset until the first cycle (or set `reaper_config_valid` only once a config is read), and make the alert expressions ignore absent series; check against a slow cache sync on kind — closed by 002 T131, T133 (US2)
- [X] G25 `SkipReason::OptedOut` and `SkipReason::UnsupportedOwner` are defined but never produced: `mode=off` workloads are dropped before planning and unowned pods are never tracked (G09). Remove both from `plan.rs`, data-model.md and contracts/annotations.md, or produce them; recommendation: remove — closed by 002 T142 (US4)
- [X] G26 A manual `flux resume` of a HelmRelease the reaper suspended is not detected as a restore; the Deployment stays reaped and the HelmRelease's next reconcile fights the scale-to-zero. data-model.md covers the CronJob case (`spec.suspend: false` = manual resume) but not the HelmRelease. Treat `spec.suspend: false` on a held HelmRelease as `manual-resume` for every workload it holds; unit test in `decide.rs`, scenario in us3 — closed by 002 T136–T140 (US3)
- [X] G27 Excluding a namespace (or adding `mode=off` to a namespace) leaves workloads already reaped there at 0 with their Flux holds in place; only a per-workload `mode=off` restores (`opted-out`). Decide with the owner whether scope changes restore (as `opted-out`) or stay documented (configuration.md, how-it-works.md). Recommendation: restore, since the reaper otherwise holds objects it no longer looks at — closed by 002 T136–T140 (US3; owner chose restore)
- [X] G28 Contract drift: contracts/events.md lists `requested`, `manual-scale-up` and `manual-resume` for `Restored` but the code also writes `opted-out`; contracts/metrics.md says alerts carry namespace/kind/name, but `ReaperActionFailed` carries action/result/mode and the cycle/config/down alerts carry none; contracts/config.md mentions only `OTEL_EXPORTER_OTLP_ENDPOINT` though the per-signal variables work too, and the reaper creates no spans of its own. Bring the contracts in line with the code (or the code with them, for spans) — closed by 002 T143, T144 (US4)

### Chart repo (mbround18/helm-charts, outside this repo)

- [X] G29 `charts/k8s-reaper/README.md` uses a `helm repo add` URL that does not exist; point it at the GitRepository/clone install the guides use (docs/guides/helm-chart.md) — closed by 002 T146 (US5; the URL existed, see 002 research R11)
- [X] G30 The memory request is 32Mi, but the reaper used about 58Mi on the production cluster watching a whole cluster; raise the request to 64Mi and check the limit leaves headroom — closed by 002 T147 (US5, chart PR)
- [X] G31 The chart's sync-wave annotations are Argo CD only and do nothing under Flux; drop them or document that CRD ordering under Flux comes from `crds: CreateReplace`. The CRD header comment is lost when the CRD is regenerated from `k8s-reaper crd`; add it in the generator or the regeneration script — closed by 002 T145, T147 (US5; sync waves were not the problem, see 002 research R12–R13)

## Dependencies

- Phase 1 → Phase 2 → US1.
- US2 needs US1 (detection and the report path). US3 needs US2 (something to
  restore). US4 needs only Phase 2 and US1; it can run in parallel with
  US2/US3.
- US5 needs US2 and US3 (record, holds, restore) and the US4 config plumbing
  for `cronJobs.*`.
- Full mode must not be enabled anywhere but dev until US3 is done.
- US6 needs US1 (transitions and Events) and the US4 config plumbing; Question 6
  is answered. It can run in parallel with US2–US5.
- Gaps G01–G05 come before T075; G07 before T044; G06 and G10–G12 before T076.
- T076 → T077. Both come after US3, so the deployed build can be switched to
  full mode safely later.

## Parallel examples

- Phase 2: T012, T014, T015, T016, T017, T018 together after T011.
- US1 tests: T020, T021, T022, T023, T024 together; then T030 and T031 alongside
  T025–T029.
- US2: T040, T041, T042 together; T045 alongside T043.
- US4 (T060 to T064) alongside US2/US3 by another contributor.
- Docs T070 to T073 any time after US3.

## Implementation strategy

1. **MVP = Phases 1–3**: ship to the production cluster in report mode (T076–T077 can follow
   US1 if the chart pins `mode: report`). It answers "what is crash-looping and
   for how long" right away.
2. Add US2 and US3 together before anyone sets `mode: full`.
3. US4 hardens runtime control. US5 adds CronJobs on top of the same paths. Then flip `mode: full`, possibly starting with
   `includeNamespaces: [sandbox]`. The owner questions in spec.md are
   answered.
