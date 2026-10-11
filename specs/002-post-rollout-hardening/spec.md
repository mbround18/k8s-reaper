# Feature Specification: Post-Rollout Hardening

**Feature Branch**: `002-post-rollout-hardening`

**Created**: 2026-10-11

**Status**: Draft

**Input**: User description: "Hardening the reaper after its first rollout:
close gaps G19–G31 found while writing the guides. Health and readiness that
reflect reality; failed writes of the reaper's own labels and annotations are
counted, evented and alerted; alert wiring that works for any release name and
does not fire falsely at startup or with the ServiceMonitor off; remove the
unused skip reasons; a manual flux resume of a held HelmRelease counts as a
restore; excluding a namespace restores the workloads reaped there; bring
contracts in line with code; chart-repo fixes."

## Context

The reaper (spec 001) was deployed to the production cluster on 2026-10-11, in report mode and
then full mode. Writing the user guides against the code turned up thirteen
places where the code, the contracts, the chart and the docs disagree. They
are recorded as G19–G31 in `specs/001-crashloop-reaper/tasks.md` ("Found while
documenting"). None of them broke the rollout, but each one either hides a
failure from the operator or leaves a workload held by a reaper that no longer
looks at it.

This spec closes them. It adds no new capability. Each story makes an existing
promise true.

| Gap | Story |
| --- | --- |
| G19, G20, G21 | US1 |
| G22, G23, G24 | US2 |
| G26, G27 | US3 |
| G25, G28 | US4 |
| G29, G30, G31 | US5 |

## User Scenarios & Testing _(mandatory)_

### User Story 1 - Health signals that tell the truth (Priority: P1)

An operator relies on Kubernetes probes and the ReaperConfig status to know
whether the reaper is working. Today the liveness endpoint always answers
healthy, readiness ignores whether any valid config is in force, and the
ReaperConfig's `Ready` condition is always `True`, so a reaper whose cycle
loop has died looks fine everywhere except in its metrics.

**Why this priority**: A silent, dead reaper is the worst failure. It stops
reaping and stops restoring, while every signal says all is well.

**Independent Test**: On the dev cluster, kill the cycle loop (a test hook or
fault injection) and confirm the pod is restarted by its liveness probe;
start the reaper with only an invalid ReaperConfig and confirm it never becomes
ready; stall cycles and confirm `Ready` turns `False` with a reason.

**Acceptance Scenarios**:

1. **Given** a running reaper, **When** its cycle loop ends or panics,
   **Then** the liveness endpoint fails and Kubernetes restarts the pod.
2. **Given** a running reaper, **When** no cycle has started for well beyond
   the readiness window (default: 10 × `cycleInterval`, at least 10 minutes),
   **Then** the liveness endpoint fails.
3. **Given** a reaper starting with no valid ReaperConfig ever applied,
   **When** the caches have synced, **Then** the readiness endpoint still fails
   until a valid config is applied.
4. **Given** a reaper running a valid config, **When** a later edit makes the
   config invalid, **Then** the reaper keeps the last valid config and
   readiness is unaffected (the invalid edit is reported, as today).
5. **Given** a running reaper, **When** its last finished cycle is older than
   3 × `cycleInterval`, **Then** the ReaperConfig `Ready` condition becomes
   `False` with reason `Stalled`; **When** a cycle finishes again, **Then** it
   returns to `True`.

---

### User Story 2 - Every failure reaches an alert, and no alert lies (Priority: P1)

An operator trusts the shipped alerts. Today a failed write of the reaper's
own label or annotations raises no Event and no alert; `ReaperDown` only works
when the release name contains `k8s-reaper` and fires forever when the
ServiceMonitor is off; and `ReaperStalled` and `ReaperConfigInvalid` can fire
during a slow start because two metrics read 0 before the first cycle.

**Why this priority**: An alert that never fires and an alert that always
fires are both reasons for an operator to stop trusting the reaper.

**Independent Test**: Deny the reaper `patch` on Deployments in one namespace
and confirm a Warning Event, an error in the action counter and a firing
`ReaperActionFailed`; install the chart under a release name without
`k8s-reaper` and confirm `ReaperDown` fires only when the reaper is down;
delay cache sync past 5 minutes and confirm no false alert.

**Acceptance Scenarios**:

1. **Given** a workload the reaper is tracking, **When** writing its state
   label, record or plan annotation fails, **Then** a Warning Event is
   recorded on the workload, the failure is counted as a failed action, and
   `ReaperActionFailed` fires on it like any other failed action.
2. **Given** the chart installed under any release name with the
   ServiceMonitor enabled, **When** the reaper pod is down for the alert's
   `for` window, **Then** `ReaperDown` fires; **When** it is up, **Then**
   `ReaperDown` does not fire.
3. **Given** the chart with the ServiceMonitor disabled, **When** it is
   installed, **Then** no `ReaperDown` rule is installed (nothing scrapes the
   reaper, so the rule could never be right).
4. **Given** a reaper whose first cycle has not yet run, **When** Prometheus
   evaluates the rules, **Then** neither `ReaperStalled` nor
   `ReaperConfigInvalid` fires on account of start-up.
5. **Given** a reaper that has run cycles and then stalls, **When** the
   stall lasts the alert's window, **Then** `ReaperStalled` still fires as
   today.

---

### User Story 3 - The reaper lets go of what it no longer looks at (Priority: P2)

An operator may hand a workload back through Flux rather than through the
reaper, or narrow the reaper's scope. Today a manual `flux resume` of a
HelmRelease the reaper suspended is not treated as a restore, so the reaper
keeps the workload at zero and the HelmRelease fights it. Excluding a
namespace leaves the workloads already reaped there at zero, with their Flux
holds in place, forever.

**Why this priority**: Both leave a workload held by a reaper that has
stopped tracking it, which only a person reading the guides would know to
undo.

**Independent Test**: On the dev cluster in full mode, reap the Helm fixture
and run `flux resume helmrelease`; confirm it is restored within one cycle.
Reap a workload, then exclude its namespace; confirm it is restored within
one cycle.

**Acceptance Scenarios**:

1. **Given** a reaped workload whose HelmRelease the reaper suspended,
   **When** someone resumes that HelmRelease, **Then** within one cycle every
   workload held through it is restored, with a `Restored` Event whose note
   starts `manual-resume`, and the reaper's other Flux changes are undone.
2. **Given** a HelmRelease the reaper suspended for several workloads,
   **When** it is resumed, **Then** all of them are restored, not only the
   first.
3. **Given** a reaped workload in namespace N, **When** N is excluded from
   scope (or labelled `mode=off`), **Then** within one
   cycle it is restored and its Flux holds released, with a `Restored` Event
   whose note starts `opted-out`, as a per-workload `mode=off` already does.

---

### User Story 4 - The contracts say what the reaper does (Priority: P3)

A contributor or integrator reads the contracts to build on the reaper. Today
two skip reasons are defined but never produced, the `Restored` note
`opted-out` is missing from the Events contract, the alert-label contract is
wrong for four alerts, and the OpenTelemetry contract omits the per-signal
endpoint variables and implies spans the reaper never creates.

**Why this priority**: Drift costs readers time but harms no workload.

**Independent Test**: Every value the code can produce for skip reasons,
restore notes and alert labels appears in the contracts, and nothing in the
contracts is unproducible; checked by a test that enumerates the values.

**Acceptance Scenarios**:

1. **Given** the skip-reason list, **When** a contributor reads the contracts,
   **Then** every listed reason can be produced and `OptedOut` and
   `UnsupportedOwner` are gone from code, data model, contracts and guides.
2. **Given** the Events contract, **When** a workload is restored by opting
   out, **Then** its note `opted-out` is listed.
3. **Given** the metrics contract, **When** a reader looks up an alert,
   **Then** the labels listed are the ones the alert really carries.
4. **Given** the config contract, **When** a reader sets up OpenTelemetry,
   **Then** the general and per-signal endpoint variables are listed and the
   contract says which signals are exported and that the reaper creates no
   spans of its own.

---

### User Story 5 - The chart's defaults and install paths hold up (Priority: P3)

An operator installs from `mbround18/helm-charts`. The chart is published in
the Helm repository at `https://mbround18.github.io/helm-charts/` (since
2026-10-11 06:03 UTC), but this repo's guides only describe installing from a
Git clone. The memory request (32Mi) is below what the reaper uses on a real
cluster (about 58Mi on the production cluster), the chart's Argo CD sync-wave annotations
are unexplained to Flux users, and the CRD's header comment is lost whenever
the CRD is regenerated.

**Why this priority**: Installs work today; this fixes the defaults and the
docs around them.

**Independent Test**: Install from the Helm repository on a fresh kind
cluster by following the guide; regenerate the CRD and diff it against the
chart's copy.

**Acceptance Scenarios**:

1. **Given** the helm-chart guide, **When** an operator follows its Helm
   repository install word for word (with Flux or with `helm`), **Then** the
   chart installs.
2. **Given** default values, **When** the reaper runs on a cluster the size of
   the production cluster, **Then** its memory use stays below its request.
3. **Given** the chart's sync-wave annotations, **When** a Flux user reads the
   helm-chart guide, **Then** it says they are for Argo CD and have no effect
   under Flux.
4. **Given** a regenerated CRD, **When** it is compared with the chart's copy,
   **Then** they are identical, header comment included.

---

### Edge Cases

- The cycle loop panics while a reap is half done: liveness fails, the pod
  restarts, and the new pod resumes the `reaping` state as spec 001 already
  requires. Nothing is left half-held.
- The reaper is ready, then the only ReaperConfig is deleted: the reaper keeps
  its last valid config, as spec 001 requires; readiness stays true.
- A label write fails on every cycle for the same workload: one Warning Event
  series, not one Event per cycle; the failed-action counter still counts
  every attempt.
- A HelmRelease the reaper suspended is resumed and immediately suspended
  again by someone else: if a cycle saw it resumed, the reaper restores its
  workloads and does not treat the new suspend as its own hold. A resume and
  re-suspend that both happen between two cycles is not a restore (not
  detected: the reaper sees the release once per cycle).
- A HelmRelease was already suspended by someone else before the reap: the
  reaper did not record a suspend, so resuming it is not a restore.
- A namespace is excluded and then included again before the next cycle:
  nothing changes.
- The chart's ServiceMonitor is enabled but Prometheus does not select it:
  `ReaperDown` fires, which is correct; the docs say so.

## Requirements _(mandatory)_

### Functional Requirements

**Health and readiness (US1, G19–G21)**

- **FR-001**: The liveness endpoint MUST fail once the cycle loop has ended
  or panicked.
- **FR-002**: The liveness endpoint MUST fail when no cycle has started for
  10 × `cycleInterval` (and at least 10 minutes), so that a hung loop is
  restarted. This window MUST be longer than the readiness window so that
  readiness fails first.
- **FR-003**: The readiness endpoint MUST fail until a valid ReaperConfig has
  been applied at least once since start-up, in addition to its existing
  conditions (caches synced, last cycle within 3 × `cycleInterval`). A later
  invalid edit MUST NOT affect readiness while the last valid config is in
  force.
- **FR-004**: The ReaperConfig `Ready` condition MUST be `False` with reason
  `Stalled` when the last finished cycle is older than 3 × `cycleInterval`,
  and `False` with reason `CachesNotSynced` before the first cycle; it MUST
  return to `True` when a cycle finishes.

**Alerting (US2, G22–G24)**

- **FR-005**: A failed write of the reaper's own state label, record
  annotations or plan annotation MUST be recorded as a failed action (action
  `record`), with a Warning Event on the workload, so that the existing
  `ReaperActionFailed` alert covers it.
- **FR-006**: The `ReaperDown` alert MUST select the reaper by the scrape job
  the chart's ServiceMonitor produces, for any release name. (The chart
  already does this, using the Service name; the contract MUST say so.)
- **FR-007**: The chart MUST NOT install `ReaperDown` when the ServiceMonitor
  is disabled.
- **FR-008**: The last-cycle timestamp and config-valid series MUST NOT
  report 0 before the first cycle. Either they are absent until the first
  cycle, or the alert rules MUST ignore the pre-first-cycle state; in both
  cases `ReaperStalled` and `ReaperConfigInvalid` MUST NOT fire during
  start-up.

**Letting go (US3, G26–G27)**

- **FR-009**: When a HelmRelease that the reaper suspended (as recorded in a
  workload's record) is found resumed by someone else, the reaper MUST treat
  it as a manual resume and restore every workload held through it, with a
  `Restored` Event whose note starts `manual-resume`.
- **FR-010**: When a reaped workload falls out of scope because its namespace
  is excluded or opted out, the reaper MUST restore it within one cycle,
  release its Flux holds, and record a `Restored` Event whose note starts
  `opted-out`. A narrower scope MUST never leave a workload held by the
  reaper.

**Contracts (US4, G25, G28)**

- **FR-011**: The skip reasons `OptedOut` and `UnsupportedOwner` MUST be
  removed from the code, the data model, the contracts and the guides.
- **FR-012**: The Events contract MUST list every `Restored` note the code
  produces, including `opted-out`.
- **FR-013**: The metrics contract MUST list, for each alert, the labels it
  actually carries.
- **FR-014**: The config contract MUST list the OpenTelemetry endpoint
  variables the reaper honours (general and per-signal), the signals it
  exports and the transport, and MUST say that it creates no spans of its own.
- **FR-015**: A test MUST fail when the code can produce a skip reason or
  restore note that the contracts do not list.

**Chart (US5, G29–G31; in `mbround18/helm-charts`)**

- **FR-016**: This repo's README and helm-chart guide MUST describe installing
  from the Helm repository (a Flux `HelmRepository` and plain `helm`), next
  to the Git source they describe today, and both MUST work as written.
- **FR-017**: The default memory request MUST be 64Mi, with a limit that
  leaves headroom over observed use (about 58Mi on the production cluster).
- **FR-018**: The helm-chart guide MUST say that the chart's sync-wave
  annotations are an Argo CD convention of the chart repository and do
  nothing under Flux. The annotations stay (11 charts there use them, and
  the chart's tests check them).
- **FR-019**: The chart's CRD MUST be byte-identical to the output of
  `k8s-reaper crd` (header comment included), so regeneration loses nothing.

**Docs**

- **FR-020**: Every guide that describes today's behaviour of a closed gap
  (troubleshooting, monitoring, how-it-works, configuration, getting-started,
  helm-chart) MUST be updated in the same change that closes it.

### Key Entities

- **Health state**: liveness (loop alive, last cycle start) and readiness
  (caches synced, a valid config applied, last cycle finish). Exposed on the
  HTTP endpoints and mirrored in the ReaperConfig `Ready` condition.
- **Reap record**: already holds which Flux object was suspended; it is what
  makes a resumed HelmRelease recognisable as the reaper's own hold.
- **Failed action**: an existing metric and Event; gains the `record` action.

## Success Criteria _(mandatory)_

### Measurable Outcomes

- **SC-001**: A reaper whose cycle loop dies is restarted by Kubernetes
  within 10 × `cycleInterval` plus the probe's failure threshold (≤ 12
  minutes at defaults), verified on the dev cluster.
- **SC-002**: A reaper with no valid config never becomes ready, verified on
  the dev cluster; a reaper whose config turns invalid stays ready.
- **SC-003**: Over 20 restarts of the reaper on the dev cluster, including
  ones with cache sync delayed past 5 minutes, no reaper alert fires.
- **SC-004**: With label writes denied for one namespace, `ReaperActionFailed`
  fires within its `for` window plus one cycle.
- **SC-005**: `ReaperDown` fires within its `for` window when the reaper is
  scaled to zero, under a release name that does not contain `k8s-reaper`.
- **SC-006**: A manual resume of a reaper-suspended HelmRelease restores all
  its held workloads within 1 cycle.
- **SC-007**: Every skip reason and restore note the code can produce is in
  the contracts, and every one in the contracts can be produced (checked by a
  test).
- **SC-008**: On a fresh kind cluster, the helm-chart guide's Helm repository
  instructions install the chart with no edits.

## Owner Decisions

Answered 2026-10-11.

### Question 1: Readiness and config (G20)

Readiness requires a valid ReaperConfig to have been applied at least once.
A later invalid edit keeps the last valid config and does not affect
readiness (FR-003).

### Question 2: Excluding a namespace with reaped workloads (G27)

Its reaped workloads are restored as `opted-out` within one cycle, as a
per-workload `mode=off` already does (FR-010). The guides' advice to restore
before excluding becomes unnecessary and is removed.

## Assumptions

- The defaults and ranges of spec 001 stay as they are; no new ReaperConfig
  fields are added. The liveness window in FR-002 is derived from
  `cycleInterval`, not configured.
- Fixing the chart (US5) is a pull request to `mbround18/helm-charts`, merged
  separately from this repo's changes; the chart bump follows the reaper
  release that ships US1–US2.
- A failed write of the reaper's own metadata is reported the same way as a
  failed scale or Flux patch; no new alert is added.
- Removing `OptedOut` and `UnsupportedOwner` is not a breaking change: no
  released build ever produced them.
- Restores triggered by this spec (FR-009, FR-010) count
  against nothing; restores are never rate-limited, as in spec 001.
- Tests that need a cluster run on the shared kind dev cluster
  (`KUBE_CONTEXT=kind-dev`), never on the production cluster.
