# Implementation Plan: Flaky CronJobs

**Branch**: `003-flaky-cronjobs` | **Date**: 2026-10-12 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/003-flaky-cronjobs/spec.md`

## Summary

Add a **failure-rate rule** for CronJobs next to the existing failing-streak
rule. The reaper records each finished run of a failing CronJob in one owned
annotation, `reaper.boop.ninja/run-history`. A CronJob becomes a candidate
when all of these hold:

- its window of the last `windowRuns` runs is full;
- at least `threshold` of those runs failed;
- the newest run failed;
- the history began at least `failingDuration` ago.

After that, the CronJob follows the existing lifecycle unchanged.

All the logic is pure functions in `reaper-core`:

- `update_history` (record, dedupe, infer, trim, clear);
- `rate_met`;
- threshold resolution.

The adapters carry one new CRD field group and two new gauges. The work adds
no crate, no dependency and no Event reason.

Decisions and alternatives are in [research.md](./research.md). The main
ones:

- **Encoding.** The history is JSON in one annotation (R1).
- **Dedupe.** Runs are ordered and deduplicated by an `(at, job)` watermark
  (R2).
- **Start.** A history only begins from the current failing streak, which
  stops it restarting from a lingering failed Job and covers restore (R3).
- **Inference.** At most one success is inferred per cycle, from
  `lastSuccessfulTime` (R4).
- **Overrides.** Override semantics copy the existing threshold keys. The
  spec's FR-010 wording was corrected to match them (R5).
- **Metrics.** The rule is named in metrics through new gauges, not new
  labels, so no label set breaks (R7).

## Technical Context

**Language/Version**: Rust, stable toolchain pinned in `rust-toolchain.toml`,
latest edition (unchanged).

**Primary Dependencies**: Existing ones only. These are `serde`/`serde_json`
(already in `reaper-core` for `held-for`), `kube`, `k8s-openapi`, `schemars`
and `prometheus-client` (adapters).

**Storage**: annotations on the CronJob (`run-history`), as for all reaper
state.

**Testing**: `cargo test` covers the core unit tests, which are
deterministic, take "now" as input and use sequences of `CronJobView`s. It
also covers the contract-drift test (`apps/k8s-reaper/tests/contracts.rs`).
The opt-in kind story `us9_flaky_cronjob` covers the cluster behaviour.

**Target Platform**: Kubernetes 1.30+ (as before), on Linux containers.

**Project Type**: a single-binary controller in a Cargo workspace:

- `apps/k8s-reaper`;
- `crates/reaper-{core,kube,flux,notify,observability}`.

**Performance Goals**: The per-cycle work per CronJob grows by O(jobs + window)
with window ≤ 100. That is negligible next to the API calls, and the existing
`decide` bench must not regress by more than 10%.

**Constraints**:

- **Size.** The annotation stays ≤ about 10 KiB at window 100.
- **Writes.** There is no extra API write per cycle: history changes ride the
  existing record merge-patch, and only when a run is added or removed.
- **Rule outputs.** Reasons, plan JSON and metrics for streak-only CronJobs
  stay byte-for-byte unchanged (SC-004).

**Scale/Scope**: the same scope as the CronJob rule today, which on the
production cluster is a few dozen CronJobs.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | Status | How |
| --------- | ------ | --- |
| I. Agnostic core | Pass | `update_history`, `rate_met` and threshold resolution live in `reaper-core` (`detect.rs`, `decide.rs`, `record.rs`, `config.rs`) and take plain data. The adapters only map the CRD field and export gauges |
| II. Safe by default | Pass | **Mode.** Report mode stays the default, and the rule feeds the same `candidate_plan`, so dry-run equivalence holds. **Limits and holds.** Existing limits, Flux holds and restore apply. **Doubt.** When in doubt the rule under-counts: unseen failures are never inferred and the window must be full. **Upgrades.** The rule is on by default, so the upgrade note in contracts/config.md tells operators to run in report mode first |
| III. Test-first | Pass | Tests are written first for `update_history` (every US2 scenario), `rate_met`, threshold resolution and the decide transitions. Time is injected, and nothing is random |
| IV. Observable | Pass | **Decisions.** Every decision keeps its Event, and the reason names the rule. **Metrics.** Two new gauges are added and no existing label set changes. **Docs.** The contract deltas are in `contracts/`, and `specs/001…/contracts/*` and the guides are edited in the same change |
| V. Runtime config | Pass | The rule is set by `cronJobs.failureRate.*` in ReaperConfig, with per-CronJob annotations. There are no compile-time switches |
| VI. Simplicity | Pass | No new crate, dependency, CRD, object kind or Event reason. There is one new owned annotation and three new person keys |

Post-design re-check: **pass**. The design artifacts add nothing beyond the
table above.

## Project Structure

### Documentation (this feature)

```text
specs/003-flaky-cronjobs/
├── plan.md              # this file
├── research.md          # R1–R9 decisions
├── data-model.md        # RunHistory, RecordedRun, FailureRate, transitions
├── quickstart.md        # dev-cluster validation (us9_flaky_cronjob)
├── contracts/
│   ├── annotations.md   # run-history, failure-window/-threshold/-rate, reason forms
│   ├── config.md        # spec.cronJobs.failureRate
│   ├── events.md        # note wording; ConfigInvalid triggers
│   └── metrics.md       # reaper_cronjob_window_runs, reaper_cronjob_rule
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks
```

### Source Code (repository root)

```text
crates/reaper-core/src/
├── record.rs      # RUN_HISTORY key; FAILURE_WINDOW/THRESHOLD/RATE person keys; RunHistory encode/decode
├── config.rs      # FailureRate {enabled, window_runs, threshold}; raw fields; ranges; validation
├── detect.rs      # update_history(), rate_met()
├── decide.rs      # Thresholds gains rate fields; decide_cron_job combines both rules; since = min
├── plan.rs        # Evidence::JobFailures gains rate: Option<RateEvidence>
└── executor.rs    # reason()/note() wording; desired_record() writes/clears run_history
crates/reaper-kube/src/
└── crd.rs         # CronJobsSpec.failure_rate: FailureRateSpec (schemars ranges/pattern)
crates/reaper-observability/src/
└── telemetry.rs   # reaper_cronjob_window_runs, reaper_cronjob_rule gauges and series removal
apps/k8s-reaper/tests/
├── contracts.rs           # drift test also covers the new metric names and keys
└── us9_flaky_cronjob.rs   # opt-in kind story (quickstart §2–4)
specs/001-crashloop-reaper/contracts/{annotations,config,events,metrics}.md   # edited in place
docs/guides/
├── cronjobs.md        # new section: the failure-rate rule, overrides, limits (R4)
├── configuration.md   # failureRate fields
└── monitoring.md      # new gauges, ratio PromQL
```

**Structure Decision**: These are the existing workspace crates with no new
crate. Decision logic goes in `reaper-core`. The `reaper-kube` and
`reaper-observability` adapters each gain only the mapping they need.

## Delivery order

1. **Config and record types.** This covers `config.rs`, `record.rs` and
   `crd.rs`, with tests for encoding, decoding and validation.
2. **`update_history`.** Write it TDD across every US2 scenario (US2).
3. **`rate_met`, threshold resolution and the `decide_cron_job` combination
   (US1, US3, US4).** All existing streak tests must stay green.
4. **Executor wording and `desired_record` (US1, US5).**
5. **Gauges and the contract-drift test (US5).**
6. **Contracts, guides and the kind story.**
7. **Follow-ups.**
   - **Release.** Cut a minor release, `0.2.0`, because the rule is a new
     default-on behaviour.
   - **Chart.** In `mbround18/helm-charts`, regenerate the chart CRD with
     `k8s-reaper crd`, bump the chart, and add a release note on the default.
     This is a separate repository, and the chart change ships before or
     with the image.

## Complexity Tracking

No constitution violations to justify.
