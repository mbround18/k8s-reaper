# Research: Post-rollout hardening

Phase 0 for [plan.md](./plan.md). Each gap was checked against the code at
`7631000` and the chart merged in `mbround18/helm-charts` (`337d9b6`). Three
of the gaps as first written turned out to be wrong; R11–R13 say which and
the spec was corrected to match.

## R1. What "the cycle loop died" means for liveness (G19, FR-001/FR-002)

- **Decision**: `/healthz` fails when no cycle has **started** within
  `max(10 × cycleInterval, 10m)`. `Health` gains a `cycle_started` instant,
  set at the top of each loop iteration and at start-up. A loop that ends or
  panics needs nothing new: `main` runs `block_on(run(args))`, so the process
  exits with an error and the kubelet restarts the container (FR-001 holds
  today; a test pins it).
- **Rationale**: The real failure mode is a hang (an await that never
  returns), and a hang is only visible as time passing. Measuring from the
  *start* of a cycle, not its end, keeps a slow but progressing cycle alive.
  10× against the 3× readiness window means readiness fails first (FR-002),
  so the pod leaves the Service before it is killed.
- **Alternatives considered**:
  - A watchdog task that calls `set_alive(false)`: one more task to keep
    alive, and it would compute the same timestamp comparison.
  - Making the liveness window configurable: rejected by the spec's
    assumptions (no new ReaperConfig fields).
  - Aborting the process from a watchdog: harder to test and loses the
    probe's failure threshold as a grace period.

## R2. Readiness needs a valid config once (G20, FR-003)

- **Decision**: `Health` gains `config_applied: AtomicBool`, set the first
  time `refresh_config` puts a valid config in force. "No ReaperConfig
  object" counts as valid: the built-in defaults are in force, as spec 001
  FR-026 already says. Once set it is never cleared, so a later invalid edit
  keeps the last valid config and readiness (owner decision Q1).
  `/readyz` = caches synced ∧ `config_applied` ∧ last cycle finished within
  3 × interval.
- **Rationale**: Today `set_ready(true)` runs after every cycle
  unconditionally, so a reaper started against an invalid config is ready
  while acting on defaults it was told not to use.
- **Alternatives considered**: tracking validity of the *latest* generation
  (rejected by the owner: an invalid edit would take the pod out of
  service for no safety gain, since the last valid config still governs).

## R3. The `Ready` condition must not be written by the loop alone (G21, FR-004)

- **Decision**: Split status writing in two.
  1. The cycle loop keeps writing counts and `lastCycleTime`, and now passes
     the real readiness to `write_status` instead of a literal `true`.
  2. A small status task in `apps/k8s-reaper` ticks every `cycleInterval`,
     reads `Health`, and writes only the `Ready` condition when its reason
     changes: `CachesNotSynced` (before the first cycle), `Stalled` (last
     finish older than 3 × interval), `CycleCompleted` (otherwise).
  `config_watch::status` takes a `Readiness` value instead of a `bool`.
- **Rationale**: A stalled loop cannot report that it is stalled. The task
  shares nothing with the loop but `Health`, which is already atomics.
- **Alternatives considered**: computing the condition in the loop only
  (cannot express `Stalled`); a separate reconciler for the status
  subresource (a second controller for one field).

## R4. Failed record writes are failed actions (G22, FR-005)

- **Decision**: Add `ActionKind::Record` (`record`). In the executor, a
  failed `live.patch` of the reaper's own metadata calls
  `run.record(ActionKind::Record, false)` and emits a Warning Event with the
  new reason `RecordFailed` (action field `Record`). Only
  `WriteError::Other` counts; `Conflict` and `NotFound` stay quiet retries,
  as for every other step. Successful record writes are **not** counted
  (they happen for every workload in every transition and would swamp the
  counter).
- **Rationale**: `ReaperActionFailed` already alerts on any
  `result="error"`, so no new alert is needed (spec assumption). A Forbidden
  on labels in one namespace is exactly the case that today is silent.
- **Alternatives considered**: reuse `ReapFailed` (wrong in report mode,
  where no reap was attempted); a separate counter (needs a new alert).

## R5. Alert wiring for any release name (G23, FR-006/FR-007)

- **Decision**: No code change for FR-006: the chart already sets
  `job={{ include "k8s-reaper.fullname" . }}`, which is the Service name the
  ServiceMonitor's scrape job takes. Fix the contract text. For FR-007, wrap
  the `ReaperDown` rule in `{{- if .Values.serviceMonitor.enabled }}` and add
  a chart test.
- **Rationale**: Without the ServiceMonitor there is no `up` series for the
  job, so `absent(...)` fires forever.

## R6. No false alerts during start-up (G24, FR-008)

- **Decision**:
  - `reaper_last_cycle_timestamp_seconds` is set to the process start time
    when the metrics are built. `ReaperStalled` (`> 600` for 5m) then fires
    only after 15 minutes with no cycle since start.
  - `reaper_config_valid` becomes a label-less `Family` so the series is
    absent until `refresh_config` first sets it. `ReaperConfigInvalid`
    (`== 0`) cannot match an absent series.
  - The OTel mirrors already report nothing until first set; they are
    unchanged.
- **Rationale**: Fixing the source keeps the alert expressions in the
  contract unchanged, and any third-party rules benefit too.
- **Alternatives considered**: guarding the rules with
  `and on() reaper_cycles_total > 0` (every consumer must copy the guard);
  leaving the timestamp absent until the first cycle (then a reaper that
  never completes a cycle never fires `ReaperStalled`; `absent()` would be
  needed in a second rule).

## R7. A manual `flux resume` is a restore (G26, FR-009)

- **Decision**: In `held_plan`, after `Requested` and before the
  scale-up check: if the record is `reaped`, names a HelmRelease hold with
  `suspended_by_reaper == Some(true)`, and that HelmRelease is in the
  snapshot with `suspended == false`, the plan is
  `Restore(ManualResume)`. Every workload held through that release
  matches the same rule in the same cycle. Restore actions release the hold
  (drop the workload from `held-for`); the resume itself is skipped because
  the release is already running.
- **Rationale**: The record already says the reaper did the suspend, so
  "someone else resumed it" is unambiguous. Only `reaped` records qualify:
  during `reaping` the hold may not have landed yet.
- **Alternatives considered**: re-suspending the release (fights a person,
  against Constitution II); restoring only the workload that triggered it
  (leaves the others held behind a release that is running anyway).

## R8. Narrowing scope restores, it does not abandon (G27, FR-010)

- **Decision**: `decide` stops filtering out-of-scope workloads before
  deciding. A workload outside scope whose record is `reaping` or `reaped`
  gets `Restore(OptedOut)`; every other out-of-scope workload is dropped as
  today. The reaper's own namespace stays hard-excluded before this rule
  (nothing there can hold a record). Namespace `mode=off` already reaches
  `OptedOut` through `label_mode`; this covers `excludeNamespaces` and
  `includeNamespaces`. Global `mode: off` is unchanged: the executor writes
  nothing at all, which is its purpose as a kill switch.
- **Rationale**: Owner decision Q2. Restoring reverses only the reaper's own
  mutation and never reaps; it is the same path `mode=off` on a workload
  already takes. See the Constitution Check note in plan.md.
- **Alternatives considered**: keep holding until a restore annotation
  (the documented status quo, which the owner rejected); restore in full
  mode only (restores already run in report mode, executor FR-018).

## R9. Remove the dead skip reasons (G25, FR-011)

- **Decision**: Delete `SkipReason::OptedOut` and `SkipReason::UnsupportedOwner`
  and their strings. Opted-out workloads are not candidates (no skip), and
  pods with an unsupported owner go to `reaper_unowned_crashlooping_pods`.
  Remove them from 001's data-model and spec text and from the two guides.
- **Rationale**: No released build produced them (spec assumption); the
  plan annotation's `skip` strings are otherwise unchanged.

## R10. A test that keeps contracts and code in step (G28, FR-012–FR-015)

- **Decision**: Add `SkipReason::ALL` and `RestoreKind::ALL` const arrays,
  each guarded by an exhaustive `match` so a new variant fails to compile
  until it is added. A test in `apps/k8s-reaper/tests/contracts.rs` reads
  the 001 contracts with `include_str!` and asserts both directions: every
  string the code can produce is in the contract table, and every string in
  the table parses back. Same for `ActionKind` against metrics.md.
- **Rationale**: Uses no new dependency (no `strum`). The 001 contracts stay
  the single living contract; 002's `contracts/` are deltas applied to them
  during implementation.
- **Alternatives considered**: putting the test in `reaper-core` (it would
  reach outside its crate into `specs/`; the app crate already depends on
  everything).

## R11. The Helm repository URL works (G29 was wrong)

- **Finding**: `https://mbround18.github.io/helm-charts/index.yaml` lists
  `k8s-reaper` 0.1.0, created 2026-10-11T06:03Z. The chart README's
  `helm repo add` works.
- **Decision**: G29 becomes a docs item here (FR-016): our README and
  helm-chart guide add a `HelmRepository` and a plain `helm` install next to
  the `GitRepository` one.

## R12. The sync waves are a repo convention (G31 half was wrong)

- **Finding**: 11 of 41 charts in `mbround18/helm-charts` carry
  `argocd.argoproj.io/sync-wave`, and `tests/test_k8s_reaper.py` checks
  them.
- **Decision**: Keep them; the helm-chart guide says they are for Argo CD
  and inert under Flux (FR-018).

## R13. The CRD differs only by its header (G31 other half)

- **Finding**: The chart's CRD body is identical to `k8s-reaper crd`; only
  the six-line `# GENERATED by \`k8s-reaper crd\`` header is missing from
  the generator.
- **Decision**: `k8s-reaper crd` prints the header itself, so regeneration
  is byte-identical (FR-019). The golden test in `apps/k8s-reaper` updates
  with it.

## R14. Memory request (G30, FR-017)

- **Decision**: `resources.requests.memory: 64Mi`, limit stays `128Mi`
  (the production cluster peaks about 58Mi; the kind dev cluster about 21Mi). This is a
  chart PR, released after the reaper release that ships US1–US2.
