# Tasks: Post-rollout hardening

**Input**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/](./contracts/), [quickstart.md](./quickstart.md)

**Tests**: Required. The constitution (III) makes core logic test-first. Within
each story, test tasks come first and must fail before the implementation
tasks that follow. Time is always a parameter (`Instant`/`Timestamp` passed
in), never read inside the code under test.

Format: `- [ ] T### [P?] [US?] description with path`. `[P]` means it can run
in parallel (different files, no unfinished dependency).

Contracts: the files in `contracts/` are **deltas**. Every task that changes
behaviour applies its delta to the living contract in
`specs/001-crashloop-reaper/contracts/` in the same change, and updates the
guides that describe it (FR-020).

Cluster work runs only with `KUBE_CONTEXT=kind-fathom-dev`, never `default`
(roanoke).

## Phase 1: Setup

- [X] T101 Check the baseline: `make test` is green on `744a865`, and `cargo bench -p reaper-core --bench decide` passes. Record the test count (252 passed, 6 ignored at `7631000`).

## Phase 2: Foundational (blocks US2, US3 and US4)

These change the core enums that several stories match on. Doing them first
keeps the parallel stories from editing the same lines.

- [X] T110 Add `SkipReason::ALL: [SkipReason; 9]` and `RestoreKind::ALL: [RestoreKind; 4]` to `crates/reaper-core/src/plan.rs`. Each is guarded by a `const fn` with an exhaustive `match`, so a new variant fails to compile until it is listed. Add a unit test that `as_str` round-trips for every entry. (T142 shrinks `SkipReason::ALL` to 7.)
- [X] T111 Add `ActionKind::Record` (`"record"`) and `ActionKind::ALL: [ActionKind; 10]` to `crates/reaper-core/src/ports.rs`, with the same exhaustive guard. Update every `match` on `ActionKind` (the telemetry label in `crates/reaper-observability/src/telemetry.rs` and the fakes in `crates/reaper-core/src/testing.rs`).
- [X] T112 Add `Transition::RecordFailed` to `crates/reaper-core/src/plan.rs`: a Warning, action field `Record`, the reason string `RecordFailed`. Update every `match` on `Transition`, including the Event mapping in `crates/reaper-kube/src/events.rs` and notification filtering in `crates/reaper-notify` (not notified by default, like `ReapFailed`).

**Checkpoint**: `make test` is green; nothing emits the new variants yet.

## Phase 3: User Story 1 — health signals that tell the truth (P1) 🎯 MVP

**Goal**: `/healthz` fails on a hung loop. `/readyz` waits for a valid config.
The `Ready` condition says `Stalled` or `CachesNotSynced` when that is the case.

**Independent test**: quickstart §1. Integration `apps/k8s-reaper/tests/us7_health.rs`.

### Tests first

- [X] T120 [P] [US1] Unit tests in `crates/reaper-observability/src/http.rs`, with injected `Instant`s and `I = 60s` and `15s`:
  - `live(now)` is true until `now - cycle_started ≥ max(10·I, 10m)`, then false (FR-002);
  - `ready(now)` needs `caches_synced ∧ config_applied ∧ cycle_finished ≠ none ∧ now - cycle_finished < 3·I` (FR-003);
  - `config_applied` latches: there is no API to clear it;
  - `readiness(now)` is `CachesNotSynced` before the first finished cycle, `Stalled` at `≥ 3·I`, otherwise `CycleCompleted`;
  - for every `I`, liveness fails strictly after readiness;
  - `/healthz` and `/readyz` return 503 with the one-line reasons in contracts/config.md (`no cycle started in …`, `no valid config applied yet`, `last cycle finished … ago`), using `tower::ServiceExt::oneshot`.
- [X] T121 [P] [US1] Unit tests in `crates/reaper-kube/src/config_watch.rs` (`status(...)`): `Readiness::CycleCompleted` → `Ready=True/CycleCompleted`; `Stalled` → `False/Stalled`; `CachesNotSynced` → `False/CachesNotSynced`. Assert that `NotReady` is never written. Update `crates/reaper-kube/tests/config_watch.rs` to match.
- [X] T122 [P] [US1] Write a test that pins FR-001 in `apps/k8s-reaper/src/main.rs` or `run.rs` tests: the error from `run` propagates out of `main`, so it exits non-zero. Keep `block_on(run(args))` and do not swallow the error.

### Implementation

- [X] T123 [US1] Rework `Health` in `crates/reaper-observability/src/http.rs` per data-model.md "Health":
  - fields `cycle_started` (initially the process start), `cycle_finished: Option<Instant>`, `config_applied` (latching), `caches_synced`;
  - remove `alive`, `set_alive`, `set_ready`;
  - methods `cycle_started(now)`, `cycle_finished(now, interval)`, `config_applied()`, `caches_synced()`, `live_at(now)`, `ready_at(now)`, `readiness_at(now) -> Readiness`;
  - the interval used by `live_at` is the one passed with the last `cycle_started` or `cycle_finished` call (store it).
  - Put `Readiness` in `reaper-observability`, and have `reaper-kube` map it to the condition. If that would add a crate dependency edge that does not exist today, define `Readiness` in `reaper-core::ports` instead (it is plain data).
- [X] T124 [US1] In `crates/reaper-kube/src/config_watch.rs`, change `status(...)` and `write_status(...)` to take `Readiness` instead of `ready: bool`. Add `write_ready(readiness)`, which re-writes the last written status with only `Ready` changed and is a no-op when the reason has not changed. Keep the last written status in the `ConfigWatch` behind an `Arc<Mutex<…>>`, so a clone can be handed to another task.
- [X] T125 [US1] Wire it in `apps/k8s-reaper/src/run.rs`:
  - call `health.caches_synced()` after the reflectors' initial sync;
  - call `health.cycle_started(Instant::now(), interval)` at the top of each loop iteration;
  - in `refresh_config`, call `health.config_applied()` when a valid config, or the built-in defaults with no object, is in force;
  - drop `health.set_ready(true)`;
  - pass `health.readiness_at(Instant::now())` to `write_status`.
- [X] T126 [US1] Add the status task in `apps/k8s-reaper/src/run.rs`. It is a `tokio::spawn`ed loop that every `cycleInterval` (read from a `watch` channel the loop updates) calls `configs.write_ready(health.readiness_at(now))`. Errors are logged at `warn`, and the task never exits the process. It must not share a lock with the cycle loop beyond `Health`'s atomics and the status mutex.
- [X] T127 [US1] Add the debug-only hang hook `apps/k8s-reaper/src/dev_hang.rs`. Behind `#[cfg(debug_assertions)]`, `REAPER_DEV_HANG_AFTER=<n>` makes cycle `n+1` await forever (`std::future::pending`). It is absent from release builds; add a `cfg` test that the module is not compiled in release.
- [X] T128 [US1] Write the integration test `apps/k8s-reaper/tests/us7_health.rs` (`#[ignore]`, uses `common::dev_client`), covering quickstart §1 steps 1–3:
  - an invalid config at start gives 503 and `CachesNotSynced`, then it becomes ready once fixed;
  - an invalid edit while running stays ready;
  - with the hang hook, `Stalled` appears within 3·I, and `restartCount` goes up within 12 min.

  Step 3 needs a debug image. Add `make image-debug` to `Makefile` (a `--build-arg PROFILE=dev` on the existing Dockerfile builder stage) only if the Dockerfile can take it without a new stage. Otherwise the test is skipped with a message saying how to build one.
- [X] T129 [US1] Docs (FR-020):
  - in `docs/guides/monitoring.md` and `docs/guides/troubleshooting.md`, the probes and the `Ready` reasons;
  - apply contracts/config.md HTTP and `status.conditions` to `specs/001-crashloop-reaper/contracts/config.md`.

**Checkpoint**: T120–T122 pass; `make test` is green; US1 is independently shippable.

## Phase 4: User Story 2 — every failure reaches an alert, and no alert lies (P1)

**Goal**: failed metadata writes are counted and evented; two gauges stop tripping
alerts at start-up; and (chart, US5 PR) `ReaperDown` only exists with the ServiceMonitor.

**Independent test**: quickstart §2–§3.

### Tests first

- [X] T130 [P] [US2] Executor tests in `crates/reaper-core/src/executor.rs`. With the fake writer failing the metadata patch:
  - with `WriteError::Other("forbidden")`, the telemetry fake records `(Record, error)`, one `RecordFailed` Warning Event is emitted on the workload, and the state does not advance;
  - with `Conflict` or `NotFound`, nothing is recorded and no Event is emitted;
  - a successful metadata patch records nothing for `Record`;
  - this holds in `Report` and `Full`.

  Extend the existing `failed_write_skips_the_event_and_reports_the_failure` test rather than duplicating it.
- [X] T131 [P] [US2] Telemetry tests in `crates/reaper-observability/src/telemetry.rs`:
  - right after `Metrics::new(start)`, the Prometheus text has `reaper_last_cycle_timestamp_seconds <start>` (not 0) and no `reaper_config_valid` line;
  - after `config_valid(true)` it shows `1`;
  - test `startup_series_do_not_alert`: given the start-up text, the two alert conditions (`time() - ts > 600` at `start + 5m`; `config_valid == 0`) are false.
  - `reaper_actions_total{action="record",result="error",mode="report"}` renders.

### Implementation

- [X] T132 [US2] In `crates/reaper-core/src/executor.rs`, at the metadata patch (`if let Err(error) = live.patch(writer, &patch).await`), match the error:
  - `WriteError::Other` → `run.record(ActionKind::Record, false)` and `run.emit(workload, &Transition::RecordFailed, <error text ≤ 1 KB>, live.state())`, then `run.fail(...)` as today;
  - other errors keep today's path.

  A repeat failure on the next cycle must continue the same Event series, which needs the same note, so the note is the error text only and has no timestamp.
- [X] T133 [US2] In `crates/reaper-observability/src/telemetry.rs`:
  - `Metrics::new` takes the process start as a Unix time and initialises `last_cycle` to it;
  - turn `config_valid` into a label-less `Family<Vec<(String, String)>, Gauge>`, so it is absent until first set;
  - the OTel mirrors are unchanged;
  - pass the start time from `apps/k8s-reaper/src/run.rs`.

  Done as `Metrics::register(.., started: SystemTime)` (there is no `Metrics::new`). `config_valid` is a small `UnsetGauge` that reports itself empty until set, not a `Family` with an empty label set: the `Family` renders `reaper_config_valid{} 1`, which the quickstart §2 grep misses.
- [X] T134 [US2] Add the dev-only RBAC `deploy/dev/rbac-deny-patch.yaml`: a ClusterRole identical to the dev one minus `patch` on `deployments`, with a comment on how to apply and revert it. Add a section to `deploy/dev/README.md`.
- [X] T135 [US2] Docs:
  - in `docs/guides/monitoring.md`: the `record` action, the `RecordFailed` runbook, and the start-up behaviour of both series;
  - in `docs/guides/troubleshooting.md`: "label writes are failing";
  - ~~apply contracts/metrics.md (the Metrics table, and the Alert rules with the labels column) to `specs/001-crashloop-reaper/contracts/metrics.md`;~~ done under T143;
  - ~~apply the `RecordFailed` row to `specs/001-crashloop-reaper/contracts/events.md`.~~ done under T143.

## Phase 5: User Story 3 — the reaper lets go of what it no longer looks at (P2)

**Goal**: a manual `flux resume` of a reaper-suspended HelmRelease, and a
namespace leaving scope, both restore.

**Independent test**: quickstart §5. Integration `apps/k8s-reaper/tests/us8_letting_go.rs`.

### Tests first

- [X] T136 [P] [US3] `crates/reaper-core/src/decide.rs` tests for data-model.md rule #4 (FR-009):
  - two workloads held through one HelmRelease whose hold has `suspended_by_reaper = Some(true)` and whose view is `suspended = false`: both get `Restore(ManualResume)` in one `decide`;
  - their restore actions drop them from `held-for` and contain **no** HelmRelease resume;
  - the last one releases the hold, and `HelmReleaseReleased` follows in the executor;
  - `suspended_by_reaper = Some(false)` (it was already suspended by someone else), resumed: no restore (spec edge case);
  - record `reaping` with the HelmRelease not suspended: no restore; the reap continues;
  - the HelmRelease missing from the snapshot: no restore.

  Add an `insta` snapshot of the plan JSON.
- [X] T137 [P] [US3] `crates/reaper-core/src/decide.rs` tests for rule #1 (FR-010):
  - a `reaped` workload in a namespace matched by `excludeNamespaces` gets `Restore(OptedOut)` with the Kustomization and HelmRelease releases;
  - the same with `includeNamespaces` set and not matching;
  - a `reaping` record gets the same;
  - an out-of-scope workload with no record, or with `observed`/`candidate`, produces no decision at all (no Event, no label write);
  - the reaper's own namespace with a forged `reaped` record produces no decision;
  - global `Mode::Off`: the executor writes nothing;
  - restores are not counted toward `maxReapsPerCycle` or `maxReaped`.
- [X] T138 [P] [US3] Equivalence: add both new fixtures to `crates/reaper-core/tests/equivalence.rs`. Restores run identically in `Report` and `Full`.

### Implementation

- [X] T139 [US3] Implement in `crates/reaper-core/src/decide.rs`:
  - in `decide`, replace the `.filter(in_scope)` with a branch:
    - own namespace → skip;
    - out of scope with a `reaping` or `reaped` record → `held_plan` with a forced `RestoreKind::OptedOut`;
    - out of scope otherwise → skip;
  - `group_pods` and `unsupported` counting keep using `in_scope`;
  - in `held_plan`, add rule #4 between `Requested` and the CronJob or scale-up checks. It looks up the HelmRelease named by the workload's Flux refs in `snap` and its hold (`crates/reaper-core/src/hold.rs`);
  - `restore_actions` already sets `resume` only when `suspended_by_reaper == Some(true)`. `hold.rs:103` resumes only when the release is still suspended, so a running release is not resumed again. Confirm this with the T136 test, and fix it there if not.

  Update the doc comments on `held_plan` and `namespace_in_scope`.
- [X] T140 [US3] Write the integration test `apps/k8s-reaper/tests/us8_letting_go.rs` (`#[ignore]`), covering quickstart §5:
  - `helm/crashloop` is reaped, then `flux resume`, then `Restored manual-resume` arrives within 1 cycle, followed by `HelmReleaseReleased`;
  - `plain/crashloop` is reaped, then `plain` is added to `excludeNamespaces`, then `Restored opted-out` arrives within 1 cycle and the Kustomization annotation is removed;
  - it restores the dev ReaperConfig in a drop guard.

  Use `kubectl`-free kube API calls, as us3_restore does.
- [X] T141 [US3] Docs:
  - in `docs/guides/flux.md` and `docs/guides/restoring-a-workload.md`: a manual `flux resume` now counts as a restore;
  - in `docs/guides/configuration.md` and `docs/guides/how-it-works.md`: excluding a namespace restores its reaped workloads; remove the "restore before excluding" advice;
  - in `docs/guides/troubleshooting.md`: remove the two "documented but unhandled" entries;
  - apply the restore-notes table to `specs/001-crashloop-reaper/contracts/events.md`.

## Phase 6: User Story 4 — the contracts say what the reaper does (P3)

**Goal**: the dead skip reasons are gone, the contract text is right, and a test
keeps the code and the contracts in step.

**Independent test**: quickstart §6.

- [X] T142 [US4] Remove `SkipReason::OptedOut` and `SkipReason::UnsupportedOwner` (and their `as_str` arms) from `crates/reaper-core/src/plan.rs`. Shrink `SkipReason::ALL` to 7, and fix the test at `crates/reaper-observability/src/telemetry.rs` (~line 776) that uses `OptedOut`. In docs:
  - `specs/001-crashloop-reaper/data-model.md` line 93;
  - `specs/001-crashloop-reaper/spec.md` lines 382–383 (add a "removed in 002" note rather than rewriting history);
  - `docs/guides/troubleshooting.md` ~162;
  - `docs/guides/how-it-works.md` ~197.
- [X] T143 [US4] Apply the remaining contract deltas to `specs/001-crashloop-reaper/contracts/` (also took T135's two contract bullets: the whole metrics.md delta and the `RecordFailed` row; the OpenTelemetry delta was corrected for counters losing `_total` over OTLP and for `RUST_LOG` filtering spans):
  - `events.md`: the "Restore notes" and "Skip reasons" tables exactly as in contracts/events.md;
  - `metrics.md`: `ReaperDown` job text;
  - `config.md`: the OpenTelemetry section (FR-014). Verify every row against `crates/reaper-observability/src/tracing.rs`, including that no `#[instrument]` or `span!` exists in `crates/` or `apps/`, and correct the delta wherever the code differs.
- [X] T144 [US4] Write the contracts test `apps/k8s-reaper/tests/contracts.rs` (not ignored). It reads `specs/001-crashloop-reaper/contracts/events.md` and `metrics.md` with `include_str!` and parses the first backticked cell of each row under the "Skip reasons" and "Restore notes" headings, plus the `action ∈ …` list in metrics.md. It then asserts set equality with `SkipReason::ALL`, `RestoreKind::ALL` and `ActionKind::ALL` (`as_str`), and its failure message names the missing or extra strings (FR-015, SC-007). Add `reaper-core` as a dev-dependency only if it is not already a dependency of the app.

## Phase 7: User Story 5 — the chart's defaults and install paths hold up (P3)

**Goal**: install from the Helm repository is documented; the chart has a 64Mi request,
a guarded `ReaperDown` and a regenerable CRD.

**Independent test**: quickstart §4 and §7.

- [X] T145 [P] [US5] `k8s-reaper crd` prints the six-line `# GENERATED by \`k8s-reaper crd\`…` header that the chart's copy carries, copied verbatim from `/home/mbruno/development/helm-charts-k8s-reaper/charts/k8s-reaper/crds/reaperconfigs.reaper.boop.ninja.yaml`. Implement it in `apps/k8s-reaper/src/main.rs`, or `crates/reaper-kube/src/crd.rs` if the YAML is built there. Update the golden file in `crates/reaper-kube/tests/golden/`, then check that `cargo run -q -p k8s-reaper -- crd | diff - <chart crd>` is empty (FR-019).
- [X] T146 [P] [US5] Docs (FR-016, FR-018):
  - `README.md` Install: add a Flux `HelmRepository` (`https://mbround18.github.io/helm-charts/`) plus `HelmRelease` with `chart: k8s-reaper` and `version: 0.1.x`, and a plain `helm repo add mbround18 … && helm install k8s-reaper mbround18/k8s-reaper -n k8s-reaper --create-namespace`. Keep the GitRepository form;
  - `docs/guides/helm-chart.md`: a new "Install from the Helm repository" section with both forms; a note that the `argocd.argoproj.io/sync-wave` annotations are the chart repository's Argo CD convention and inert under Flux; and that `ReaperDown` fires when the ServiceMonitor is enabled but not selected by Prometheus (spec edge case);
  - `docs/guides/getting-started.md`: point at the new section.
- [X] T147 [US5] (committed aca6b30 on fix/k8s-reaper-hardening, not pushed) Chart PR in the worktree `/home/mbruno/development/helm-charts-k8s-reaper`, on a new branch `fix/k8s-reaper-hardening` from `origin/main`:
  - `charts/k8s-reaper/values.yaml`: `resources.requests.memory: 64Mi`, limit `128Mi`;
  - `templates/prometheusrule.yaml`: wrap the `ReaperDown` rule in `{{- if .Values.serviceMonitor.enabled }}`;
  - regenerate `crds/` from T145's output;
  - `tests/test_k8s_reaper.py`: a test that `ReaperDown` is absent with `serviceMonitor.enabled=false`, and that under release name `other-name` it selects `job="other-name-k8s-reaper"`;
  - bump the chart's patch version per that repo's rules, and run its test suite.

  Commit signed. **Do not push or open the PR without the owner's go-ahead.**

## Phase 8: Polish & cross-cutting

- [X] T150 `make test`, `make lint` and `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-D warnings` are all green; `cargo bench -p reaper-core --bench decide` still passes. Record the test count.
  - 2026-10-11 at `9a859b8`: 297 passed, 0 failed, 9 ignored (dev-cluster stories); lint and strict `cargo doc` clean; bench median 0.18 ms, p99 0.36 ms over 200 runs.
- [X] T151 Run the integration suite on kind with `KUBE_CONTEXT=kind-fathom-dev`: `make image dev-load dev-run`, then `STORY=us7_health`, `STORY=us8_letting_go`, and a regression run of `us1_report` and `us3_restore`. Record the results here.
  - 2026-10-11 at `9a859b8` on `kind-fathom-dev`: `us7_health` 2/2 on `make image-debug` with `REAPER_TEST_DEBUG_IMAGE=1`, hung-loop step included (1173 s); on the release image `us8_letting_go` 1/1 (215 s), `us1_report` 1/1 (405 s), `us3_restore` 1/1 (140 s).
- [X] T152 Run the start-up loop in quickstart §2 (20 restarts) and §3 (deny patch) on kind, and record the outcome against SC-003 and SC-004.
  - SC-003: 20/20 restarts read `reaper_last_cycle_timestamp_seconds` as the new process's start time (never 0) and `reaper_config_valid` 1. No Prometheus on the shared cluster, so `ALERTS` was not checked; `telemetry::startup_series_do_not_alert` covers the expressions.
  - SC-004: with the deny patch, a re-created `plain/crashloop` got a `RecordFailed` Warning (Forbidden) within 15 s and `reaper_actions_total{action="record",result="error",mode="report"} 1`. ClusterRole reverted; `can-i patch deployments` is `yes` again.
- [X] T153 Run a link check over `README.md`, `CONTRIBUTING.md` and `docs/guides/*.md`. Grep the guides for `OptedOut`, `UnsupportedOwner`, `NotReady` and "restore before excluding"; there should be no matches.
  - 2026-10-11: no broken relative links or anchors in 14 files (external URLs not checked); no stale terms.
- [X] T154 Tick G19–G31 in `specs/001-crashloop-reaper/tasks.md`, each pointing at the 002 task that closed it.

## Dependencies

- Phase 1 → Phase 2 → stories.
- **US1** (T120–T129) depends only on Phase 1. It touches `http.rs`, `config_watch.rs` and `run.rs`.
- **US2** (T130–T135) needs T111 and T112. Its only file shared with US1 is `run.rs` (T133 passes the start time), so merge US1 first.
- **US3** (T136–T141) needs T110. It touches only `decide.rs`, `hold.rs` and the tests, so it runs in parallel with US1 and US2.
- **US4** (T142–T144) needs T110, and T143 goes after T141, since both edit `contracts/events.md`. T144 goes after T142 and T143.
- **US5**: T145 and T146 are independent of everything. T147 needs T145.
- Polish needs all stories.

## Parallel execution

After Phase 2, three agents can work at once, each in its own worktree and
each committing signed:

1. **US1**: T120 → T121 → T122 → T123 → T124 → T125 → T126 → T127 → T128 → T129.
2. **US3**: T136 → T137 → T138 → T139 → T140 → T141.
3. **US5** (T145, T146, T147) and US4 T142.

Then:

4. **US2**: T130 → T135, on top of US1.
5. **US4**: T143 → T144, on top of US3.

## Implementation strategy

- **MVP**: Phase 2 plus US1. Health that tells the truth is what makes the
  roanoke deployment self-healing.
- Then US2, so every failure alerts, and US3, so narrowing the scope is safe.
- US4 and US5 are cleanup. The chart PR waits for a reaper release that ships
  US1 and US2.
- Each story lands as its own signed commits on the feature branch. Merging to
  `main`, tagging and pushing the chart PR all need the owner's go-ahead.
