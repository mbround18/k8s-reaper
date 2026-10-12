# Feature Specification: Flaky CronJobs

**Feature Branch**: `003-flaky-cronjobs`

**Created**: 2026-10-12

**Status**: Draft

**Input**: User description: "Catch flaky CronJobs: pause a CronJob that fails
most of its runs even when it succeeds now and then. Today one success ends
the failing streak, so a job that fails, say, 8 of its last 10 runs is never
paused. Add a failure-rate rule alongside the existing streak rule: over a
rolling window of the last N finished runs, if the share of failures meets a
threshold (and the window is full / spans a minimum duration so a new or
rarely-scheduled job is not judged on too few runs), the CronJob becomes a
candidate and goes through the same lifecycle, limits, report mode, Events,
metrics and Flux holds. The reaper must record each run's outcome itself
because Jobs are often deleted quickly, and it must count each Job exactly
once across cycles and restarts. Settings are global in ReaperConfig with
per-CronJob annotations that may only make it stricter-to-pause; the rule can
be disabled. The candidate reason names the rate. Resuming a paused CronJob
resets its run history. Existing streak behaviour is unchanged."

## Context

Spec 001 pauses a CronJob after a **failing streak**: every run failed for
at least `cronJobs.failingDuration` (default 6h) and at least
`cronJobs.minFailedRuns` times (default 3). One successful run ends the
streak and resets the count to 0 (`docs/guides/cronjobs.md`, section 5).

That rule never pauses a CronJob that succeeds now and then. A job that
fails 8 of every 10 runs starts a new streak after each lucky success and
never reaches the thresholds. It keeps starting Jobs, using resources and
paging nobody, for as long as it exists.

This spec adds a second, independent rule, the **failure-rate rule**. It
judges a CronJob by the share of failures among its recent runs. A CronJob
becomes a candidate when either rule is met. Everything after that (states,
limits, report mode, Flux holds, restore) is the existing CronJob lifecycle.

Kubernetes keeps few finished Jobs: `failedJobsHistoryLimit` and
`successfulJobsHistoryLimit` default to 1 and 3, and many charts set
`ttlSecondsAfterFinished` to minutes (the production cluster's foundryvtt
backup jobs delete theirs after 5 minutes). So the reaper cannot read a
CronJob's recent history back from the cluster; it has to record each run's
outcome as it sees it.

## User Scenarios & Testing _(mandatory)_

### User Story 1 - A mostly-failing CronJob is paused (Priority: P1)

A team ships a CronJob that fails most nights but succeeds now and then.
Nobody notices, because the streak rule resets on every success. With this
feature the reaper records each run's outcome, and once enough runs have been
seen and most of them failed, it pauses the CronJob like any other candidate
and says why.

**Why this priority**: It is the gap. Without it, flaky CronJobs run forever.

**Independent Test**: On the dev cluster, run a CronJob every minute whose
Job fails on 4 runs out of 5 (a fixed pattern, not random), with
`failureRate.windowRuns: 5`, `failureRate.threshold: 80%` and
`failingDuration: 10m`. Confirm that once 5 runs are recorded, the history
began at least 10 minutes earlier and the newest run failed, the CronJob
becomes a candidate with a reason such as `4 of last 5 runs failed`, and is
suspended in full mode.

**Acceptance Scenarios**:

1. **Given** a CronJob whose recorded window holds 10 runs, 8 of them failed,
   the newest run failed, and its history began longer ago than
   `failingDuration`, **When** a cycle runs with the defaults, **Then** the
   CronJob becomes a candidate and the reason reads `8 of last 10 runs
   failed`.
2. **Given** the same CronJob in full mode with limits allowing a reap,
   **When** the cycle runs, **Then** it is suspended through the same steps,
   Flux holds and Events as a streak reap, and the `Reaped` Event names the
   failure-rate rule.
3. **Given** the same CronJob in report mode, **When** the cycle runs,
   **Then** the plan is published with the same reason and a `ReapPlanned`
   Event, and nothing is suspended.
4. **Given** a CronJob whose window holds only 6 recorded runs, all failed
   except one, **When** a cycle runs, **Then** it is not a candidate under the
   failure-rate rule, because the window is not full.
5. **Given** a CronJob whose window is full and 8 of 10 failed, but whose
   newest run succeeded, **When** a cycle runs, **Then** it is not a
   candidate; it is re-judged when its next run finishes.
6. **Given** a CronJob whose window is full and 7 of 10 failed (below the
   default threshold), **When** a cycle runs, **Then** it is not a candidate.

---

### User Story 2 - Each run is counted exactly once, whatever Kubernetes deletes (Priority: P1)

An operator can trust the recorded history: a Job is never counted twice, a
run is not lost because the reaper restarted, and a Job cleaned up early
never makes a CronJob look worse than it is.

**Why this priority**: The rule pauses workloads. A wrong count pauses a
healthy job, and that is worse than missing a flaky one.

**Independent Test**: Unit tests in the core against sequences of observed
Job lists across cycles (Jobs appearing, finishing out of order, disappearing
by TTL or history limit, a reaper restart between cycles) asserting the
recorded window after each cycle.

**Acceptance Scenarios**:

1. **Given** a finished Job already recorded, **When** later cycles see it
   again, **Then** it is not recorded again.
2. **Given** the reaper restarts between two cycles, **When** it resumes,
   **Then** it rebuilds nothing from memory: the recorded history on the
   CronJob is complete and no Job is counted twice.
3. **Given** two Jobs of the same CronJob that finish in the opposite order
   to the one they started in, **When** a cycle sees both, **Then** both are
   recorded once, ordered by when they finished.
4. **Given** a successful Job deleted by its TTL before any cycle saw it,
   **When** the CronJob's last-successful time has moved past what the
   reaper last recorded, **Then** one success is recorded for it.
5. **Given** a failed Job deleted before any cycle saw it, **When** a cycle
   runs, **Then** nothing is recorded for it; the window may under-count
   failures but never over-counts them.
6. **Given** a Job still running, **When** a cycle runs, **Then** it is not
   recorded until it finishes.

---

### User Story 3 - Teams can tune or turn off the rule for their CronJob (Priority: P2)

An operator sets cluster-wide defaults. A team that knows its job is noisy
by design can make the rule harder to trigger for that CronJob, or switch it
off for that CronJob, but cannot make it easier to trigger than the cluster
allows.

**Why this priority**: The rule needs a safe, cluster-wide default first
(P1); per-CronJob tuning follows the pattern teams already use for the
streak thresholds.

**Independent Test**: Unit tests on threshold resolution, plus one dev-cluster
check that an annotation raising the window delays the candidate state and
one that lowers it is ignored with a Warning Event.

**Acceptance Scenarios**:

1. **Given** `cronJobs.failureRate.enabled: false` in the ReaperConfig,
   **When** a cycle runs, **Then** no CronJob becomes a candidate under the
   failure-rate rule, and the streak rule is unaffected.
2. **Given** a CronJob annotated with a larger window or a higher threshold
   than the cluster's, **When** it is judged, **Then** its own values are
   used.
3. **Given** a CronJob annotated with a smaller window or a lower threshold
   than the cluster's, **When** it is judged, **Then** the cluster's values
   are used and an Event says the annotation was ignored, as for the
   existing threshold annotations.
4. **Given** a CronJob annotated to turn the failure-rate rule off, **When**
   it is judged, **Then** only the streak rule applies to it.
5. **Given** a ReaperConfig with a failure-rate setting out of range,
   **When** it is applied, **Then** it is rejected as invalid and the last
   valid config stays in force, as for any other invalid setting.

---

### User Story 4 - Resuming starts a clean slate (Priority: P2)

A team fixes its job and resumes the CronJob. The runs from before the pause
must not get it paused again on its next failure.

**Why this priority**: Without it, a resumed CronJob with a full window of
old failures would be paused on its first new failure, which punishes a fix.

**Independent Test**: Unit test: restore a CronJob paused by the failure-rate
rule; feed one failed run; assert it is not a candidate and its window holds
one run.

**Acceptance Scenarios**:

1. **Given** a CronJob paused under either rule, **When** it is resumed by
   any of the existing restore paths, **Then** its run history is cleared,
   and Jobs that finished before the restore are never recorded afterwards.
2. **Given** a resumed CronJob, **When** its next run fails, **Then** its
   window holds one run and it is not a candidate under the failure-rate
   rule until the window is full again.

---

### User Story 5 - Operators can see which rule fired and how flaky jobs are (Priority: P3)

An operator looking at a paused CronJob, its Events or the dashboards can
tell whether it was paused for a streak or for its failure rate, and can see
the recorded rate of CronJobs that are flaky but not yet candidates.

**Why this priority**: The reaper already explains every decision
(constitution IV). This extends that to the new rule; it does not change
what gets paused.

**Independent Test**: Unit tests on Event and reason text; a metrics
contract test asserting the new label value and series.

**Acceptance Scenarios**:

1. **Given** a CronJob that is a candidate under the failure-rate rule,
   **When** the operator reads its plan, Events or the reap metrics, **Then**
   the rule is named (failure rate vs. streak) and the reason gives the
   counts.
2. **Given** a CronJob whose window holds failures but is not a candidate,
   **When** the operator lists the reaper's tracked CronJobs, **Then** it
   shows as `observed` with its recorded failures and window size readable
   from its annotations.
3. **Given** a CronJob that meets both rules in the same cycle, **When** it
   becomes a candidate, **Then** one plan is made and both rules are named.

### Edge Cases

- **Healthy CronJobs carry no history.** A CronJob is only given a run
  history when the reaper first records a failed run for it. Its history is
  removed once the window holds no failures, so a healthy CronJob carries no
  reaper annotations. A CronJob's history therefore always begins with a
  failure.
- **Fix, then a lucky window.** A team fixes a job after 8 failures; the
  next runs succeed. Because the rule only fires when the newest run failed,
  the CronJob is never paused on the strength of failures from before the
  fix while it keeps succeeding.
- **Fast schedules and the minimum duration.** For a CronJob that runs every
  minute, the last 10 runs span 10 minutes. The minimum-duration guard is
  measured from when the history began, not across the window, so a
  minutely job must have been recorded as failing for at least
  `failingDuration` before the rate rule can fire.
- **Slow schedules.** A daily CronJob needs `windowRuns` days of history
  before the rate rule can judge it (10 days with the defaults). The streak
  rule still covers a daily job that fails every night.
- **A candidate that recovers while waiting.** A candidate held back by
  `maxReapsPerCycle` or `maxReaped` is judged again each cycle. If its newest
  run succeeds in the meantime, it returns to `observed`.
- **Suspended by someone else, or `mode=off`.** Not tracked, as today: any
  recorded history is cleared without an Event.
- **`cronJobs.enabled: false`.** History is cleared along with the other
  CronJob state, as today.
- **Rule disabled after history was recorded** (cluster-wide, or for one
  CronJob by annotation). Recording stops and the history is cleared on the
  next cycle, so no stale `observed` label is left on a CronJob without a
  streak. Turning the rule back on starts a fresh history.
- **Failed Jobs deleted before a cycle sees them.** Not counted (same as the
  streak rule). The window under-counts failures; a CronJob with
  `failedJobsHistoryLimit: 0` and a short TTL can evade the rule. Documented,
  not fixed.
- **Malformed history annotation** (edited by hand). Treated as no history:
  recording starts again from the next finished run, and a Warning Event
  says the annotation was unreadable.
- **Window size lowered in the ReaperConfig.** Only the newest `windowRuns`
  runs are judged; older recorded runs are dropped on the next write.

## Requirements _(mandatory)_

### Functional Requirements

- **FR-001**: For each tracked CronJob, the reaper MUST record the outcome
  (succeeded or failed) and finish time of each of its finished runs, keeping
  at most the newest `windowRuns` runs, on the CronJob itself, so that the
  history survives reaper restarts and Job deletion.
- **FR-002**: Each finished Job MUST be recorded at most once, regardless of
  how many cycles observe it, the order in which Jobs finish, or reaper
  restarts.
- **FR-003**: A successful run whose Job was deleted before any cycle saw it
  MUST be recorded as one success when the CronJob's last-successful time
  advances past the newest success already recorded. A failed Job that was
  never seen MUST NOT be inferred.
- **FR-004**: Running Jobs MUST NOT be recorded.
- **FR-005**: A CronJob MUST become a candidate under the failure-rate rule
  only when all of these hold:
  (a) the rule is enabled for it;
  (b) its window holds `windowRuns` recorded runs;
  (c) the share of failed runs in the window is at or above `threshold`;
  (d) its history began at least `failingDuration` ago (the same setting the
  streak rule uses);
  (e) its newest recorded run failed.
- **FR-006**: The streak rule MUST behave exactly as today. A CronJob is a
  candidate when either rule is met.
- **FR-007**: A failure-rate candidate MUST go through the existing CronJob
  lifecycle unchanged: states, plan steps, limits and their ordering, report
  mode, Flux holds, Events and restore paths. For ordering among candidates,
  a failure-rate candidate is placed by when its history began.
- **FR-008**: The candidate reason MUST name the rule and the counts, in the
  form `<failed> of last <window> runs failed`, followed by the newest failed
  Job and its failure reason as the streak reason does today.
- **FR-009**: ReaperConfig MUST accept, under `cronJobs.failureRate`:
  `enabled` (default `true`), `windowRuns` (default `10`, allowed `3` to
  `100`) and `threshold` (default `80%`, allowed `51%` to `100%`). Values out
  of range make the config invalid, as for other settings.
- **FR-010**: A CronJob MAY carry annotations that raise `windowRuns` or
  `threshold` for itself, or turn the failure-rate rule off for itself.
  Values that would make it easier to trigger than the cluster's MUST be
  ignored, with a Warning Event, as the existing threshold annotations are.
- **FR-011**: A CronJob MUST be labelled `observed` while its window holds at
  least one failure or it has a failing streak, and MUST carry no run
  history once its window holds no failures.
- **FR-012**: Any restore of a CronJob MUST clear its run history and MUST
  ensure Jobs that finished before the restore are never recorded after it.
- **FR-013**: When `cronJobs.enabled` is false, the failure-rate rule is
  turned off (cluster-wide or for the CronJob), or a CronJob is suspended by
  someone else or opted out with `mode=off`, its run history MUST be cleared
  and no new runs recorded, as other CronJob state is cleared today.
- **FR-014**: An unreadable run-history annotation MUST be treated as empty,
  replaced on the next write, and reported with a Warning Event.
- **FR-015**: Events and metrics that report a reap decision for a CronJob
  MUST distinguish the failure-rate rule from the streak rule. The new
  annotation keys, label values, Event wording and metric series are public
  contracts and MUST be documented in `specs/*/contracts/` and the user
  guides in the same change.
- **FR-016**: The decision MUST be made in the core from plain inputs (the
  recorded history, the observed Jobs, the config and "now"), with
  deterministic tests, as for every other decision.

### Key Entities

- **Run history**: per CronJob, an ordered list of at most `windowRuns`
  recorded runs, newest last, plus the time the history began. Lives on the
  CronJob. Exists only while the window holds a failure.
- **Recorded run**: one finished Job's outcome (succeeded/failed), its
  finish time, and enough identity to recognise the same Job on a later
  cycle.
- **Failure-rate settings**: `enabled`, `windowRuns`, `threshold`; cluster
  defaults in ReaperConfig, stricter-only overrides per CronJob.

## Success Criteria _(mandatory)_

### Measurable Outcomes

- **SC-001**: A CronJob that fails 8 or more of its last 10 runs (defaults),
  whose newest run failed and whose history is older than `failingDuration`,
  is a candidate on the first cycle after that run is recorded.
- **SC-002**: A CronJob that fails 7 or fewer of its last 10 runs, or whose
  newest run succeeded, is never paused by the failure-rate rule.
- **SC-003**: In deterministic tests covering at least restarts, repeated
  observation, out-of-order finishes, and Jobs deleted by TTL and history
  limit, no Job is ever counted twice.
- **SC-004**: Every existing CronJob streak test passes unchanged.
- **SC-005**: A healthy CronJob (no failure in its window) carries no reaper
  annotations or labels after the cycle that records its window clear.
- **SC-006**: An operator can tell from one CronJob's Events, without reading
  logs, which rule paused it and the failure counts behind it.

## Assumptions

- The minimum-age guard reuses `cronJobs.failingDuration` instead of adding a
  setting. One knob means one place to tune "too soon to judge" for both
  rules.
- The rule is enabled by default, so upgrading a cluster in full mode can
  pause CronJobs that the streak rule never would. The defaults (10 runs,
  80%, newest run failed) are conservative, and the release notes will say
  so. Report mode shows the effect before anyone switches to full.
- The rule only fires when the newest run failed. This trades a slightly
  later pause for never pausing a CronJob right after it succeeded.
- History starts at the first failure the reaper records; successes before
  it are unknown. So a CronJob is judged on runs from its first recorded
  failure onwards, which can only make the rate look higher, and the
  full-window and minimum-age guards keep that from being premature.
- The run history is stored compactly enough that a 100-run window stays far
  below Kubernetes' annotation size limit.
- Existing Event reasons and the `Reaped`/`ReapPlanned` flow are reused; no
  new Event reason is needed for the rule itself, only new wording and a
  field naming the rule.
