# Implementation Plan: Post-rollout hardening

**Branch**: `002-post-rollout-hardening` | **Date**: 2026-10-11 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/002-post-rollout-hardening/spec.md`

## Summary

Close gaps G19–G31, which were found while writing the guides after the first
rollout to the production cluster. The work is small changes inside the existing crates; it
adds no crate, dependency or CRD field.

1. **Health that tells the truth (US1).**
   - `Health` tracks when a cycle last started, whether a valid config was
     ever applied, and when a cycle last finished.
   - `/healthz` fails after `max(10 × interval, 10m)` with no cycle start.
   - `/readyz` also needs an applied config.
   - A status task writes `Ready=False` (`Stalled`, `CachesNotSynced`) even
     when the loop is hung.
2. **Alerts that fire when they should, and only then (US2).**
   - Failed writes of the reaper's own metadata count as action `record`,
     with a `RecordFailed` Event.
   - The start-up values of two gauges stop tripping alerts.
   - The chart installs `ReaperDown` only with its ServiceMonitor.
3. **Letting go (US3).** `decide` gains two restore triggers:
   - a reaper-suspended HelmRelease found running → `ManualResume`;
   - a reaped workload that falls out of scope → `OptedOut`.
4. **Contracts (US4).**
   - Remove two dead skip reasons.
   - Correct the contract text.
   - Add a test that fails when code and contracts drift apart.
5. **Chart (US5).** In `mbround18/helm-charts`:
   - memory request 64Mi;
   - a `serviceMonitor.enabled` guard on `ReaperDown`;
   - the CRD header.

   Here: `k8s-reaper crd` prints the header, and the guides document the
   Helm repository install.

Decisions, with alternatives, are in [research.md](./research.md). Three gaps
as first written were wrong (R11–R13), and the spec was corrected to match.

## Technical Context

**Language/Version**: Rust stable 1.99, edition 2024 (unchanged from 001)

**Primary Dependencies**: The same as 001, with no additions: `kube`,
`k8s-openapi`, `tokio`, `axum` + `tower`, `prometheus-client`, `opentelemetry*`,
`tracing*`, `serde*`, `thiserror`, `clap`, and `insta` for tests.

**Storage**: None. State stays in annotations (001 R6).

**Testing**:

- `cargo test`: unit tests in core and adapters, and the new contracts test.
- Integration tests run on `kind-dev`, `#[ignore]`d and split by
  story: `us7_health`, `us8_letting_go`.
- The chart's Python tests run in `mbround18/helm-charts`.

**Target Platform**: Kubernetes ≥ 1.30 (production: k3s v1.36.5) with Flux v2

**Project Type**: Kubernetes operator (Cargo workspace)

**Performance Goals**: Unchanged. The two new `decide` rules are O(workloads)
lookups in maps the snapshot already holds, and the `decide` bench (< 1 s,
median 0.18 ms) must still pass.

**Constraints**:

- `reaper-core` stays pure.
- No new ReaperConfig fields.
- Restores are never rate-limited.
- Dev testing never touches the production cluster or the `default` context.

**Scale/Scope**: One cluster with about 300 pods. Six gap groups across five
crates, plus one chart PR.

## Constitution Check

_Gate evaluated before research and again after design. Both pass, with one
note under II._

- [x] **I. Agnostic core, thin adapters.**
  - The new restore rules and `ActionKind::Record` live in `reaper-core`,
    still with no kube, tokio or clock.
  - Health, status and the gauge fixes stay in `reaper-observability`,
    `reaper-kube` and the app.
- [x] **II. Safe by default.**
  - Nothing new reaps.
  - G27 restores workloads in a namespace that has just been excluded. This
    is an action in an excluded namespace, but it only reverses the reaper's
    own recorded mutation. It is the path `mode=off` on a workload already
    takes, and it was chosen by the owner (Q2). Its justification is in
    Complexity Tracking.
  - The reaper's own namespace stays hard-excluded before the new rule.
  - Global `mode: off` still writes nothing (R8).
- [x] **III. Test-first.**
  - Every rule gets a failing core test first (R7, R8, R4), and health gets
    unit tests with an injected `Instant`.
  - The contracts test (R10) is deterministic.
- [x] **IV. Observable.** The public contract changes are additive or are
  corrections, listed in [contracts/](./contracts/):
  - action value `record`;
  - Event reason `RecordFailed`;
  - `Ready` reasons `Stalled` and `CachesNotSynced`;
  - the start-up behaviour of two series.

  The removed skip reasons were never emitted.
- [x] **V. Runtime config.** Nothing new is configured. The liveness window
  derives from `cycleInterval`.
- [x] **VI. Simplicity.**
  - One new task (status) and two `decide` rules.
  - No new crates, dependencies or CRD fields.
- [x] **Repo constraints.** Same layout. Signed commits. The guides are
  updated in the same change that closes each gap (FR-020).

Post-design re-check: data-model.md and contracts/ add no crate, service or
dependency. Still passes.

## Project Structure

### Documentation (this feature)

```text
specs/002-post-rollout-hardening/
├── spec.md  plan.md  research.md  data-model.md  quickstart.md
├── checklists/requirements.md
└── contracts/               # deltas, applied to specs/001-crashloop-reaper/contracts/
    ├── config.md            # HTTP probes, Ready condition reasons, OTel env
    ├── metrics.md           # action `record`, start-up series, alert labels, ReaperDown
    └── events.md            # RecordFailed, Restored notes, skip reasons table
```

### Source Code (files this feature touches)

```text
crates/reaper-core/src/
├── plan.rs            # drop two SkipReasons; SkipReason::ALL, RestoreKind::ALL
├── ports.rs           # ActionKind::Record (+ ALL)
├── decide.rs          # out-of-scope reaped → Restore(OptedOut); resumed HR → ManualResume
└── executor.rs        # failed metadata patch → record(Record, false) + RecordFailed Event
crates/reaper-observability/src/
├── http.rs            # Health: cycle_started, config_applied; liveness window
└── telemetry.rs       # last_cycle starts at process start; config_valid absent until set
crates/reaper-kube/src/
└── config_watch.rs    # status() takes Readiness {Ready, Stalled, CachesNotSynced}
apps/k8s-reaper/
├── src/run.rs         # cycle_started, config_applied, real readiness, status task
├── src/main.rs        # `crd` prints the generated header
├── src/dev_hang.rs    # REAPER_DEV_HANG_AFTER, #[cfg(debug_assertions)] only (quickstart §1)
└── tests/
    ├── contracts.rs   # code ↔ contracts, both directions (new)
    ├── us7_health.rs  # SC-001–SC-003 on kind (new)
    └── us8_letting_go.rs  # SC-006, FR-010 on kind (new)
deploy/dev/rbac-deny-patch.yaml  # dev-only ClusterRole without deployments patch (quickstart §3)
docs/guides/           # troubleshooting, monitoring, how-it-works, configuration,
                       # getting-started, helm-chart (FR-016, FR-018, FR-020)
README.md              # Helm repository install

# mbround18/helm-charts (separate PR)
charts/k8s-reaper/values.yaml                 # requests.memory 64Mi
charts/k8s-reaper/templates/prometheusrule.yaml  # ReaperDown only with serviceMonitor.enabled
charts/k8s-reaper/crds/                       # regenerated, header included
tests/test_k8s_reaper.py                      # guard test
```

**Structure Decision**: The existing workspace, with no new crates. The
integration tests continue the per-story numbering of 001 (`us7`, `us8`) so
`make test-integration STORY=…` keeps working.

## Complexity Tracking

| Deviation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| Restoring workloads in a namespace that is now excluded (G27; Constitution II says the reaper never acts on excluded namespaces) | An excluded namespace would otherwise keep workloads that the reaper scaled to zero, with Flux held, and nothing would ever let them go | Leaving them held until someone annotates them: this is the current behaviour, which the owner rejected (Q2). The restore only undoes the reaper's own recorded changes, the same as `mode=off` on a workload |
