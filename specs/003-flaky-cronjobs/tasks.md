# Tasks: Flaky CronJobs

**Input**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/](./contracts/), [quickstart.md](./quickstart.md)

**Tests**: Required. The constitution (III) makes core logic test-first, and
FR-016 restates that rule for this feature.

- Within each story, test tasks come first and must fail before the
  implementation tasks that follow.
- Time is always a `Timestamp` parameter ("now"), never read inside the code
  under test.

Format: `- [ ] T### [P?] [US?] description with path`. `[P]` means a task can
run in parallel: it touches different files and has no unfinished
dependency. Task IDs use the 2xx range, following spec 002's 1xx.

Contracts: the files in `contracts/` are **deltas**. Every task that changes
behaviour does two more things in the same change:

- it applies its delta to the living contract in
  `specs/001-crashloop-reaper/contracts/`;
- it updates the guides that describe the behaviour (FR-015).

Cluster work runs only with `KUBE_CONTEXT=kind-dev`, never `default`
(the production cluster).

## Phase 1: Setup

- [X] T201 Check the baseline:
  - `make test` is green on `fac73bf`;
  - `cargo bench -p reaper-core --bench decide` runs.

  Record the test count and the `decide` bench median here. Every later
  phase compares against them: SC-004, and the plan's limit of no more than
  10% regression on the bench.

  **Recorded on `795f6d6`:** 297 passed, 9 ignored. `decide` bench (130
  workloads, 22 decisions, 200 runs): median 208.6 µs, p99 339.3 µs.

## Phase 2: Foundational (blocks every story)

These tasks add the types every story needs: settings, the record key,
evidence and the CRD field. No behaviour changes yet.

### Tests first

- [X] T202 [P] Write unit tests in `crates/reaper-core/src/config.rs`.
  - **Defaults.** `FailureRate::default()` is `{enabled: true, window_runs: 10, threshold: 80}`.
  - **Raw fields.** These parse:
    - `cron_jobs_failure_rate_enabled`;
    - `cron_jobs_failure_rate_window_runs`;
    - `cron_jobs_failure_rate_threshold`, which accepts both `"80%"` and `"80"`.
  - **Validation.** Each of these makes the config invalid:
    - `window_runs` outside `3..=100` (test 2 and 101);
    - `threshold` outside `51..=100` (test 50, 101, `"abc"` and `"80.5%"`).
  - **Valid edges.** 3, 100, 51 and 100 are valid.
- [X] T203 [P] Write unit tests in `crates/reaper-core/src/record.rs`:
  - **Round trip.** `RunHistory` survives `encode()` and `decode()` with the
    exact compact JSON from contracts/annotations.md, for example
    `{"since":"…","runs":[{"job":"backup-1","at":"…","ok":false},{"job":null,"at":"…","ok":true}]}`.
  - **Absent key.** A record with `run_history: None` writes no
    `reaper.boop.ninja/run-history` key.
  - **Owned keys.** `is_owned_key(RUN_HISTORY)` is true.
    `is_owned_key(FAILURE_WINDOW|FAILURE_THRESHOLD|FAILURE_RATE)` is false,
    because they are in `PERSON_KEYS`.
  - **Malformed values.** Each of these decodes as `run_history: None` plus a
    `DecodeError` naming `run-history`:
    - invalid JSON;
    - an unknown field;
    - an empty `runs`;
    - `runs` with no failure;
    - `runs` not strictly increasing by `(at, job)`.
- [X] T204 [P] Write a test in `crates/reaper-kube/src/crd.rs` that the
  generated CRD schema has `cronJobs.failureRate` with:
  - `enabled` (boolean);
  - `windowRuns` (integer, `minimum: 3`, `maximum: 100`);
  - `threshold` (string with pattern `^(5[1-9]|[6-9][0-9]|100)%?$`).

  Also test that a spec without `failureRate` maps to the core defaults.

### Implementation

- [X] T205 Add `FailureRate { enabled: bool, window_runs: u32, threshold: u8 }`
  to `CronJobs` in `crates/reaper-core/src/config.rs`. It needs:
  - `WINDOW_RUNS_RANGE: (3, 100)` and `THRESHOLD_RANGE: (51, 100)`;
  - raw fields and validation in the existing `validate()` path.

  An out-of-range value makes the config invalid (FR-009). This makes T202
  pass.
- [X] T206 Make the following changes in `crates/reaper-core/src/record.rs`.
  This makes T203 pass.
  - **Keys.** Add the constants:
    - `RUN_HISTORY = "reaper.boop.ninja/run-history"`;
    - `FAILURE_WINDOW`, `FAILURE_THRESHOLD` and `FAILURE_RATE`, appended to
      `PERSON_KEYS`, which becomes `[&str; 8]`.
  - **Types.** Add `RunHistory { since, runs: Vec<RecordedRun> }` and
    `RecordedRun { job: Option<String>, at: Timestamp, ok: bool }`, with
    serde and `deny_unknown_fields`.
  - **Record.** Add `WorkloadRecord.run_history: Option<RunHistory>`.
  - **Decode.** `decode` enforces the invariants in data-model.md: runs are
    non-empty, at least one has `ok == false`, and they are strictly
    increasing by `(at, job)`.
  - **Encode.** `encode` writes the key only when it is `Some`.
- [X] T207 Add `RateEvidence { failed: u32, window: u32, met: bool }` in
  `crates/reaper-core/src/plan.rs`. Extend
  `Evidence::JobFailures { …, rate: Option<RateEvidence> }`, and set
  `rate: None` at every existing construction site. Existing tests must
  still pass unchanged (SC-004).
- [X] T208 Add `FailureRateSpec { enabled: Option<bool>, window_runs:
  Option<u32>, threshold: Option<String> }` and
  `CronJobsSpec.failure_rate: Option<FailureRateSpec>` in
  `crates/reaper-kube/src/crd.rs`. Use schemars attributes for the range
  and pattern, and map the spec to the core raw fields. This makes T204
  pass.

**Checkpoint**: `make test` is green, and nothing writes `run-history` yet.

## Phase 3: User Story 2 — each run counted exactly once (P1)

US2 comes before US1 because US1 judges the history that US2 records.

**Goal**: `update_history` records each finished Job once, survives restarts
and Job deletion, and infers unseen successes only.

**Independent test**: the `history` unit tests in `crates/reaper-core/src/detect.rs`
(quickstart §1).

### Tests first

- [X] T210 [US2] Write unit tests for `update_history` in
  `crates/reaper-core/src/detect.rs`, with a builder for `CronJobView`
  sequences. Each test asserts the exact `Option<RunHistory>` after each
  call. Cover these cases:
  1. **No previous history, passing streak.** The result is `None`.
  2. **No previous history, failing streak** of three listed failed Jobs.
     The history is seeded with all three, oldest first, and `since` is the
     first one's finish time (research R3).
  3. **Re-listing.** The same Jobs listed again add nothing (US2-1).
  4. **Restart.** Feed a decoded-then-encoded history back in. The result is
     identical, so nothing is held in memory (US2-2).
  5. **Out of order.** Two Jobs finish in the opposite order to their start
     order. Both are recorded, by `(at, name)` (US2-3).
  6. **Deleted success.** A success deleted by its TTL before it was seen,
     where `last_successful_time` is greater than the last run. One
     `{job: None, ok: true}` is appended (US2-4).
  7. **No double counting.** A listed success at the same time as
     `last_successful_time` is recorded once, not twice.
  8. **Several deleted successes.** Several successes are missed across one
     cycle. At most one is inferred (research R4).
  9. **Deleted failure.** A failed Job deleted before it was seen is never
     added (US2-5).
  10. **Running Job.** A running Job is not added, and is added once it has
      finished (US2-6).
  11. **Before the watermark.** A Job finishing before the watermark is
      skipped.
  12. **Trim.** The window is trimmed to `window_runs`, keeping the newest.
      Lowering `window_runs` drops the oldest runs (Edge Cases).
  13. **Window clears.** Once the window holds no failures, the result is
      `None` (FR-011).
  14. **Lingering failed Job.** A failed Job listed from before the last
      success (`failedJobsHistoryLimit`) does not restart a cleared
      history.
  15. **Restore watermark.** A failure at or before `last-failed-job` does
      not seed a history (FR-012).

  Each case is one test, named `history_*`.

### Implementation

- [X] T211 [US2] Implement `pub fn update_history(prev:
  Option<RunHistory>, cron: &CronJobView, streak: &JobStreak, window_runs:
  u32) -> Option<RunHistory>` in `crates/reaper-core/src/detect.rs`, by the
  five steps in data-model.md. Reuse the `(finished_at, name)` ordering
  that `job_streak` uses. It must be total and must not panic on an empty
  `jobs`. This makes T210 pass.
- [X] T212 [US2] Add a property-style deterministic test in
  `crates/reaper-core/src/detect.rs`. It uses a fixed, seedless table of 50
  cycle sequences mixing every case in T210. For each sequence it asserts
  that no Job name appears twice in `runs` and that `runs` stays strictly
  increasing (SC-003).

**Checkpoint**: the `history_*` tests pass, and `decide` does not call
`update_history` yet.

## Phase 4: User Story 1 — a mostly-failing CronJob is paused (P1) 🎯 MVP

**Goal**: The rate rule makes a candidate through the existing lifecycle,
with the reason `<failed> of last <window> runs failed, …`.

**Independent test**: quickstart §2, `apps/k8s-reaper/tests/us9_flaky_cronjob.rs`.

### Tests first

- [X] T220 [P] [US1] Write unit tests for `rate_met` in
  `crates/reaper-core/src/detect.rs`. With window 10, threshold 80 and
  `failing_duration` 6h:
  - 8 of 10 failed, newest failed, `since` 7h ago → `met` (US1-1);
  - 6 runs → not met, because the window is not full (US1-4);
  - newest run succeeded → not met (US1-5);
  - 7 of 10 failed → not met (US1-6);
  - `since` 5h ago → not met;
  - a threshold of 100 with 10 of 10 failed → met;
  - integer arithmetic only, where `failed*100 >= threshold*window`.
- [X] T221 [P] [US1] Write unit tests for `decide_cron_job` in
  `crates/reaper-core/src/decide.rs`:
  1. **Rate only.** A CronJob whose history meets the rate rule while its
     streak does not: one failure since the last success, below
     `min_failed_runs`. The result is `candidate_plan`, with
     `evidence.rate = Some{failed: 8, window: 10, met: true}` and `since`
     equal to `history.since`.
  2. **Report mode.** The same CronJob publishes the same plan with
     `ReapPlanned` and is not suspended (US1-3).
  3. **Full mode.** The same CronJob produces the existing suspend actions
     and Flux holds (US1-2).
  4. **Window holds failures, streak passing.** The result is `Observe`,
     not `Recover`.
  5. **Window cleared, streak passing.** The result is `Recover`.
  6. **Ordering.** Candidates under both rules are sorted by `since`, and
     `apply_limits` holds the later one (FR-007).
  7. **Candidate recovers.** A candidate whose newest run then succeeds goes
     back to `Observe` (Edge Cases).
- [X] T222 [P] [US1] Write unit tests for `reason()` in
  `crates/reaper-core/src/executor.rs`:
  - **Rate only.** `8 of last 10 runs failed, last job "ingest-1" failed
    (BackoffLimitExceeded)`.
  - **Streak only.** The text is byte-for-byte the existing one.
  - **Both** (also US5-3). `8 of last 10 runs failed; 7 failed runs since
    …, last job "…" failed (…)`.

  The examples are in contracts/annotations.md.

### Implementation

- [X] T223 [US1] Implement `pub fn rate_met(h: &RunHistory, enabled: bool,
  window_runs: u32, threshold: u8, failing_duration: Duration, now:
  Timestamp) -> RateEvidence` in `crates/reaper-core/src/detect.rs`, per
  data-model.md "Rate evaluation". This makes T220 pass.
- [X] T224 [US1] Make the following changes in
  `crates/reaper-core/src/decide.rs`. This makes T221 pass.
  - **Thresholds.** Extend `Thresholds` with
    `failure_rate_enabled: bool`, `window_runs: u32` and `threshold: u8`,
    taken from the global config for now; annotations come in US3.
  - **Update the history.** In `decide_cron_job`, after the existing guards,
    decode `record.run_history` and call `update_history`.
  - **Evaluate.** Evaluate the streak as today and `rate_met` alongside it.
    Either rule met gives `candidate_plan`. Otherwise:
    - a failing streak, or a window with a failure, gives `Observe`;
    - neither gives `Recover` if there is state, else `Nothing`.
  - **Since.** `since` is the earliest of the met (or observed) rules'
    starts.
  - **Evidence.** Fill `Evidence::JobFailures.rate` whenever a history
    exists.
  - **Record.** Pass the new history to the decision so the executor can
    write it.
- [X] T225 [US1] Make the following changes in
  `crates/reaper-core/src/executor.rs`. This makes T222 pass.
  - **`reason()`.** Builds the three forms in contracts/annotations.md.
  - **`note()`.** For `JobFailuresObserved`, appends
    `; window: <failed> of <runs> failed` when a history exists
    (contracts/events.md).
  - **`desired_record()`.** For a CronJob, writes `run_history` from the
    decision.
    - It keeps the streak keys (`failing-since`, `failed-runs`,
      `last-failed-job`) only while the streak is failing.
    - It keeps `run_history` unchanged on `held_plan`
      (`reaping`/`reaped`), per research R8.
- [X] T226 [US1] Write the opt-in kind story
  `apps/k8s-reaper/tests/us9_flaky_cronjob.rs`, covering quickstart §2
  steps 1–5:
  - a minutely CronJob `flaky` in namespace `cron`, failing unless the
    minute is divisible by 5;
  - `windowRuns: 5`, `threshold: "80%"`, `failingDuration: 10m`.

  Assert, in report mode and then in full mode:
  - the `run-history` growth;
  - the `candidate` reason;
  - the `ReapPlanned` and `Reaped` notes;
  - `spec.suspend`.

  Register it like the other `us*` stories so that
  `make test-integration STORY=us9_flaky_cronjob` runs it alone.

**Checkpoint**: MVP. A flaky CronJob is paused in full mode and reported in
report mode, and every streak test still passes.

## Phase 5: User Story 3 — per-CronJob tuning and turning the rule off (P2)

**Goal**: Annotations can only raise the window or threshold, or turn the
rule off. The cluster-wide `enabled: false` turns the rule off everywhere.

**Independent test**: threshold-resolution unit tests, and quickstart §4.

### Tests first

- [X] T230 [P] [US3] Write unit tests for `Thresholds::for_workload` in
  `crates/reaper-core/src/decide.rs`, with a cluster window of 10 and a
  threshold of 80:
  - `failure-window=15` → 15 (US3-2);
  - `failure-window=5` → 10, with no `InvalidSetting` (US3-3);
  - `failure-window=2`, `=101` or `=x` → 10, plus an `InvalidSetting`;
  - `failure-threshold=90` and `="90%"` → 90;
  - `failure-threshold=60` → 80, with no `InvalidSetting`;
  - `failure-threshold=50` or `=abc` → 80, plus an `InvalidSetting`;
  - `failure-rate=off` → disabled (US3-4);
  - `failure-rate=on` or `=yes` → enabled, plus an `InvalidSetting`.
- [X] T231 [P] [US3] Write unit tests in `crates/reaper-core/src/decide.rs`:
  - **Disabled cluster-wide.** With `failureRate.enabled: false` and a
    history that would be met, the CronJob is not a candidate, and the
    streak rule is unaffected (US3-1).
  - **Rule off, no streak.** The history is cleared (`run_history: None`)
    and the plan is `Forget`.
  - **Rule off, failing streak.** The streak state is kept and only the
    history is dropped (research R8).
  - **Turned back on.** A history starts again from the current streak
    only.

### Implementation

- [X] T232 [US3] In `Thresholds::for_workload` in
  `crates/reaper-core/src/decide.rs`, resolve the three new annotations:
  - `failure-window` with the existing `count` helper, range `(3, 100)`,
    combined with `.max(global)`;
  - `failure-threshold` with a new percent helper that strips one trailing
    `%`, range `(51, 100)`, combined with `.max(global)`;
  - `failure-rate`: `off` disables the rule, and any other value goes to
    `bad()`.

  This makes T230 pass.
- [X] T233 [US3] In `decide_cron_job` in `crates/reaper-core/src/decide.rs`,
  when the rule is disabled (globally or by annotation):
  - skip `update_history`;
  - set the history to `None`;
  - fall back to streak-only behaviour.

  This makes T231 pass.
- [ ] T234 [US3] Extend `apps/k8s-reaper/tests/us9_flaky_cronjob.rs` with the
  four annotation rows of quickstart §4.

## Phase 6: User Story 4 — resuming starts a clean slate (P2)

**Goal**: Every restore clears the history, and Jobs from before the restore
are never recorded afterwards.

**Independent test**: restore unit tests, and quickstart §2 step 6.

### Tests first

- [X] T240 [P] [US4] Write unit tests in `crates/reaper-core/src/decide.rs`
  for each `RestoreKind`: `Requested`, `ManualResume`, `OptedOut` and the
  release resume. For a CronJob reaped under the rate rule:
  - the restore's `ClearReaperState` leaves `run_history: None` and
    `last-failed-job` equal to the newest failed Job;
  - feeding one new failed Job then gives a one-run history and no
    candidate (US4-1, US4-2);
  - re-listing the pre-restore failed Jobs adds nothing.
- [X] T241 [P] [US4] Write unit tests in `crates/reaper-core/src/decide.rs`
  that a history is cleared with `Forget` in each of these cases (FR-013):
  - `mode=off`;
  - `cronJobs.enabled: false`;
  - suspended by someone else.

### Implementation

- [X] T242 [US4] Make sure `ClearReaperState` in
  `crates/reaper-core/src/executor.rs` (`desired_record`) drops
  `run_history`. Make sure the `Forget` and `Recover` paths write
  `WorkloadRecord::default()` with no history. Fix whatever T240 and T241
  expose.
- [ ] T243 [US4] Add quickstart §2 step 6 (restore, then one failure gives a
  one-run history and no candidate) to
  `apps/k8s-reaper/tests/us9_flaky_cronjob.rs`.

## Phase 7: User Story 5 — operators can see which rule fired (P3)

**Goal**: Events, the `reason` annotation and the new gauges name the rule
and the counts. An unreadable history is reported.

**Independent test**: telemetry unit tests, the contracts-drift test and
quickstart §3.

### Tests first

- [X] T250 [P] [US5] Write unit tests in
  `crates/reaper-observability/src/telemetry.rs`:
  - **Window gauge.** With a history, `reaper_cronjob_window_runs{namespace,
    name, outcome="failed"|"succeeded"}` holds the counts. Both series are
    removed once the history is gone (SC-005).
  - **Rule gauge.** `reaper_cronjob_rule{namespace, name, rule}` is 1 for
    each rule met while the CronJob is `candidate`, two series when both
    rules are met. They are removed when it leaves `candidate`.
  - **Existing metrics.** The existing metric label sets are unchanged.
  - **Crash-loop seconds.** `reaper_workload_crashloop_seconds` for a
    CronJob uses the earlier of the streak start and `history.since`.
- [X] T251 [P] [US5] Write unit tests in `crates/reaper-core/src/decide.rs`:
  - an unreadable `run-history` gives an `InvalidSetting{key: run-history}`,
    so a `ConfigInvalid` Event is raised, and the history is treated as
    empty (FR-014);
  - the next write replaces the bad value.
- [X] T252 [P] [US5] Extend the contracts-drift test in
  `apps/k8s-reaper/tests/contracts.rs` so that each of these named in
  `specs/001-crashloop-reaper/contracts/{metrics,annotations}.md` must exist
  in code:
  - `reaper_cronjob_window_runs`;
  - `reaper_cronjob_rule`;
  - `run-history`;
  - `failure-window`, `failure-threshold` and `failure-rate`.

### Implementation

- [X] T253 [US5] Register and maintain the two gauges in
  `crates/reaper-observability/src/telemetry.rs`. Feed them from the cycle
  plan's `Evidence::JobFailures.rate` and from the rules met, and remove
  their series as the existing per-workload gauges are removed. This makes
  T250 pass.
- [X] T254 [US5] In `decide()` in `crates/reaper-core/src/decide.rs`, map
  `DecodeError`s for `run-history` to `InvalidSetting`, which the existing
  `ConfigInvalid` emission reports. This makes T251 pass.
- [ ] T255 [US5] Extend quickstart §3 (every run succeeds, the
  annotations, label and series are gone, and `JobFailuresRecovered` is
  emitted once) in `apps/k8s-reaper/tests/us9_flaky_cronjob.rs`.

## Phase 8: Polish and cross-cutting

- [ ] T260 [P] Apply the four deltas in `specs/003-flaky-cronjobs/contracts/`
  in place to `specs/001-crashloop-reaper/contracts/{annotations,config,events,metrics}.md`,
  so the drift test (T252) passes.
- [ ] T261 [P] Update `docs/guides/cronjobs.md` with a new section, "The
  failure-rate rule". It covers:
  - how runs are recorded;
  - the four conditions;
  - the three annotations;
  - reading `run-history` with the `jq` one-liner;
  - the known limits:
    - unseen failures are not counted;
    - CronJobs faster than the cycle interval with short TTLs are
      under-recorded (research R4);
    - `failedJobsHistoryLimit: 0` with a short TTL evades the rule.

  Also fix section 5's "one success ends it" text.
- [ ] T262 [P] Update `docs/guides/configuration.md` with the
  `cronJobs.failureRate` fields, defaults and ranges, and the upgrade note
  (run in report mode first).
- [ ] T263 [P] Update `docs/guides/monitoring.md` with the two gauges and
  the ratio PromQL from contracts/metrics.md.
- [ ] T264 [P] Update `docs/guides/how-it-works.md` with one paragraph on
  the two CronJob rules.
- [ ] T265 Regenerate the CRD with `cargo run -p k8s-reaper -- crd`. Check
  it in wherever the repo keeps the generated copy (if it keeps one), and
  note the chart follow-up in `mbround18/helm-charts` (`charts/k8s-reaper`)
  in the PR description.
- [ ] T266 Run the final checks:
  - `make test` is green, and its count is the T201 count plus the new tests;
  - every pre-existing CronJob streak test passes with unchanged assertions
    (SC-004);
  - `cargo bench -p reaper-core --bench decide` is within 10% of T201.
- [ ] T267 Run quickstart §2–§4 on `kind-dev`:
  `make test-integration STORY=us9_flaky_cronjob KUBE_CONTEXT=kind-dev`.
  Record the result here.

## Dependencies

```text
T201 → Phase 2 (T202–T208) → US2 (T210–T212) → US1 (T220–T226) ─┬→ US3 (T230–T234)
                                                                  ├→ US4 (T240–T243)
                                                                  └→ US5 (T250–T255) → Polish (T260–T267)
```

- **US2 before US1.** US1 judges the history that US2 records, so US2 must
  land first.
- **After US1.** US3, US4 and US5 each depend only on US1, and can run in
  parallel if they edit different functions. All three touch `decide.rs`, so
  merge one at a time.
- **Kind story.** The tasks T226, T234, T243 and T255 all edit
  `us9_flaky_cronjob.rs`, so they run in order.

## Parallel examples

- **Phase 2:** T202, T203 and T204 (tests in three files), then T205, T206
  and T208 alongside each other. T207 follows T206.
- **US1:** T220, T221 and T222 (detect, decide and executor tests).
- **US3, US4 and US5 tests:** T230, T240 and T250 are in different files or
  different test modules.
- **Polish:** T260–T264 are all different files.

## Implementation strategy

1. **MVP = Phase 2 + US2 + US1.** At that point a flaky CronJob is recorded
   correctly and paused. Ship it behind report mode on the dev cluster
   first.
2. **US4 next.** Without it, a restored CronJob may be re-paused on its
   first failure.
3. **US3 and US5 after that.** Then do the polish: contracts, guides and
   CRD.
4. **Release.** Cut `0.2.0`, with a release note on the rule being on by
   default. The chart CRD regeneration in `mbround18/helm-charts` ships
   with or before it.
