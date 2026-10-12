# Research: Flaky CronJobs

Decisions taken while planning spec 003. Each one gives what was chosen, why,
and what was rejected.

## R1. Where the run history lives

**Decision**: The history goes in one reaper-owned annotation on the CronJob,
`reaper.boop.ninja/run-history`. Its value is a JSON object:

```json
{"since":"2026-10-12T02:00:53Z","runs":[{"job":"backup-29340000","at":"2026-10-12T02:00:53Z","ok":false}]}
```

- `since` is when the history began.
- `runs` is ordered oldest first, holds at most `windowRuns` entries, and each
  entry records one finished Job.

**Rationale**:

- FR-001 needs the history to survive reaper restarts and Job deletion. The
  CronJob is the only object that lives as long as the history must.
- One key can be written atomically with the other record keys in the
  existing merge patch.
- The core already uses `serde_json` for `held-for`, so no new dependency is
  needed.
- Size: a 100-run window at about 90 bytes per entry is about 9 KiB, far below
  the 256 KiB annotation limit.
- An operator can read the counts with
  `kubectl get cronjob X -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/run-history}'`.
  The guide shows a `jq` one-liner.

**Alternatives rejected**:

- *A compact custom string* (`F1728698453:job,…`). It is smaller but needs a
  hand-written parser, which is one more format to keep valid, and it is
  harder to read.
- *Separate keys per field* (`window-failed`, `window-runs`, `run-jobs`).
  Several keys must stay consistent with each other, and the job list is still
  needed for dedupe.
- *A ConfigMap or a CRD status*. It adds a new object kind, RBAC and a
  lifecycle to manage, which constitution VI rules out.

## R2. Counting each Job once (FR-002)

**Decision**:

- **Watermark.** A finished Job is new when its `(finished_at, name)` sorts
  after the newest recorded run's `(at, job)`. This is the same ordering
  `detect::job_streak` already uses.
- **Recording.** New Jobs are appended in that order, and only Jobs that have
  finished are considered (FR-004).
- **Unseen Jobs.** A Job finishing before the watermark but never seen is
  skipped. That can only under-count, which is the safe direction.

**Rationale**:

- The history is the only state, so a reaper restart changes nothing
  (US2-2).
- Re-listing a recorded Job is a no-op (US2-1).
- Jobs that finish out of start order are ordered by finish time (US2-3).
- Comparing names alone would not do: a name drops out of a trimmed window, so
  a Job still listed would be counted again.

**Alternative rejected**: keeping a separate set of seen Job names. It grows
without bound, or needs its own trimming rule, and adds nothing the watermark
does not already give.

## R3. When a history may begin

**Decision**: A history begins only from failed Jobs that belong to the
**current failing streak**, as `detect::job_streak` computes it. That means
failures after the last success (listed or `lastSuccessfulTime`) and after
the `last-failed-job` restore watermark. Every streak failure still listed is
recorded, oldest first, and `since` is the first one's finish time.

**Rationale**:

- `failedJobsHistoryLimit` (default 1) keeps an old failed Job listed for
  days. Without this rule, a history removed when its window cleared (FR-011)
  would start again from that same old failure on the next cycle, and
  `observed` would flap.
- A cleared window always ends in successes, so any lingering failure is
  older than the last success and is not in the streak.
- The restore watermark (`last-failed-job`, already kept after a restore)
  covers FR-012 for free: a pre-restore failure is never part of a
  post-restore streak.

**Alternative rejected**: a dedicated "history cleared at" timestamp
annotation. It would be one more key that stays on healthy CronJobs, which
breaks SC-005.

## R4. Inferring an unseen success (FR-003)

**Decision**: After the listed Jobs are appended, the reaper records one
inferred success if both of these hold:

- the CronJob's `status.lastSuccessfulTime` sorts after the watermark;
- no recorded run has `ok: true` at that time.

An inferred run has `job: null`. Within one cycle a listed success at the
same time takes precedence, and the watermark comparison treats `null` as
equal to any name at the same `at`.

At most one success is inferred per cycle. Unseen failures are never
inferred.

**Rationale**:

- `lastSuccessfulTime` is the one durable trace of a deleted success, and the
  controller sets it to the Job's completion time.
- Inferring more than one success per cycle would mean guessing how many ran.

**Known limit (documented in the guide)**: Some runs can go unrecorded, on
both sides, when both of these are true:

- the CronJob runs more often than the cycle interval (default 1m);
- its Jobs are deleted faster than one interval.

Failures are never invented, so the rate can be off in either direction, but
the window still has to fill with seen runs before the rule judges it.

## R5. Annotation overrides and invalid values (FR-010, FR-014)

**Decision**: The person keys follow the existing pattern in
`Thresholds::for_workload`:

- `reaper.boop.ninja/failure-window`: an integer from 3 to 100, combined as
  `max(global)`.
- `reaper.boop.ninja/failure-threshold`: a percentage from 51% to 100%, given
  as `85` or `85%`, combined as `max(global)`.
- `reaper.boop.ninja/failure-rate`: `off`. Any other value is reported and
  ignored.

As today:

- A lower value silently resolves to the global value.
- A value that cannot be parsed or is out of range becomes an
  `InvalidSetting`, which emits `ConfigInvalid`.
- A malformed `run-history` is also reported as an `InvalidSetting` on that
  key, treated as empty, and replaced on the next write.

The spec's FR-010 and US3-3 said that a lower value raises a Warning Event.
That does not match how the existing threshold annotations behave, so the
spec was corrected in this change to match the code.

**Rationale**: These are the same semantics and the same Event as the four
existing override keys. No new Event reason is needed.

**Alternative rejected**: a new Event reason for ignored overrides. It is a
contract change that does not pay for itself, and it would be inconsistent
with the keys that already exist.

## R6. How the two rules combine in `decide`

**Decision**:

- `decide_cron_job` keeps its guards (reaped/reaping, restore, mode off,
  disabled, suspended).
- It then updates the history (R2–R4) and evaluates both rules:
  - **streak**: unchanged;
  - **rate**: FR-005 (a)–(e).
- If either rule is met, the result is `candidate_plan`. Otherwise the result
  is `Observe` if the streak is failing or the window holds a failure, and
  `Recover`/`Nothing` if neither holds.
- `Decision.since` is the earliest of `streak.since` and `history.since`
  among the rules involved.
- Evidence becomes
  `Evidence::JobFailures { …existing…, rate: Option<RateEvidence> }`, where
  `rate` is set whenever a history exists and `RateEvidence` is
  `{failed, window, met: bool}`. The streak fields stay, so streak-only
  output is byte-for-byte unchanged (SC-004).

**Rationale**:

- One plan per CronJob (US5-3).
- Ordering by `since` already exists (FR-007).
- Keeping the existing evidence fields keeps every streak test green.

## R7. Naming the rule (FR-008, FR-015)

**Decision**:

- **Reason.** The reason is built as follows:
  - Rate only: `8 of last 10 runs failed, last job "x" failed (BackoffLimitExceeded)`.
  - Both rules: the rate clause, then `; ` and the streak clause.
  - Streak only: unchanged.
  - The `Reaped` and `ReapPlanned` notes already carry the reason, so the
    rule is named in Events, the `reason` annotation and the plan JSON with no
    new Event reason.
- **`JobFailuresObserved`.** Its note gains `window: 3 of 4 failed` when a
  history exists.
- **Metrics.** Two new gauges are added, and no existing label set changes:
  - `reaper_cronjob_window_runs{namespace,name,outcome}`, where `outcome` is
    `failed` or `succeeded`. It is present while a history exists, and its
    series are removed with the history.
  - `reaper_cronjob_rule{namespace,name,rule}`, where `rule` is `streak` or
    `failure_rate`. It is 1 while the CronJob is `candidate` and that rule is
    met this cycle, so there are two series when both are met. It is not kept
    through `reaping`/`reaped`: both rules are only evaluated for an
    unpaused CronJob, and storing the rule for later would need a new
    annotation or a new field in the `plan` JSON, which would change the
    plan bytes for streak reaps (SC-004). Once reaped, the `reason`
    annotation and the `Reaped` Event name the rule.

  This replaces the spec's phrase "the reap metrics distinguish the rule":
  adding a `rule` label to `reaper_actions_total` or `reaper_workload_state`
  would break their label sets (constitution IV).

**Alternative rejected**: a `rule` label on `reaper_workload_state`. It is a
breaking change and would also change the label set of crash-loop
workloads.

## R8. Lifecycle of the history (FR-011–FR-013)

**Decision**: The history is written through `desired_record()`. It is
cleared, that is set to the record's `run_history: None`, in these cases:

- `Recover` or `Forget`. This covers mode off, CronJobs disabled, suspended
  by someone else, and the rule disabled with no streak.
- every restore (`ClearReaperState`);
- when the window holds no failures.

Disabling the rule while a streak exists keeps the streak state and drops the
history only.

`run-history` is held through `reaping` and `reaped` (the `held_plan` path
leaves it as it is) so the `Reaped` reason and the metrics stay explainable.
It is removed by the restore.

## R9. CRD and chart

**Decision**: Add `CronJobsSpec.failure_rate: Option<FailureRateSpec>` with
these fields:

- `enabled: Option<bool>`;
- `window_runs: Option<u32>`, with schemars range 3–100;
- `threshold: Option<String>`, with pattern `^(5[1-9]|[6-9][0-9]|100)%?$`.

Re-run `k8s-reaper crd`.

The chart CRD lives in `mbround18/helm-charts` (`charts/k8s-reaper/crds`),
and regenerating it is a follow-up there. It must ship before the image that
reads the field, because older CRDs prune unknown fields. That does no harm,
since the defaults apply, but an operator could not tune the rule.

**Threshold as a string**: `"80%"` matches how operators write it in the
guide. The core stores it as an integer percent (`u8`). Integer comparison
(`failed * 100 >= threshold * window`) avoids floating point.
