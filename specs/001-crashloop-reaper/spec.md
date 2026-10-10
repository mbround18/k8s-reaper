# Feature Specification: MVP Crashloop Reaper

**Feature Branch**: `001-crashloop-reaper`

**Created**: 2026-10-10

**Status**: Draft

**Input**: User description: "A Kubernetes operator that finds workloads stuck
crash-looping and, depending on mode, reports them or scales them to zero, with
Flux integration so Flux does not immediately undo the scale-down. Report mode
by default; full mode acts; one documented action restores a workload."

## Context

The target cluster (roanoke: two-node k3s v1.36, delivered by Flux from
`mbround18/fluxified`) has workloads that have been crash-looping for days. On
2026-10-10, `slam-prod` had six Deployments (`audit`, `authz`, `identity`,
`ingest-serve`, `media`, `venues`) each at about 850 restarts after three days,
and `fathom-dev/crashloop` had more than 5,000 after 19 days.

The kubelet already backs off restarts (10 s, doubling, capped at 5 minutes),
and the cap cannot be raised. So a broken workload restarts every 5 minutes
forever: it uses CPU, pulls images, fills logs, and nothing tells a person.
Autoscaler-based approaches (KEDA) were rejected. They are configured per
workload, take ownership of `replicas`, and flap: at zero replicas there are no
restarts, the metric drops, and the workload is scaled back up.

The reaper is a single cluster-wide watcher. It decides when a workload has been
broken long enough, says so where the existing alerting (Prometheus →
Alertmanager → Discord) already looks, and in `full` mode stops the workload in
a way that Flux will not immediately undo and a person can reverse in one step.

Failed **Jobs** are out of scope. They stop on their own via `backoffLimit`, and
the cluster already alerts on failed Jobs. `slam-prod/ingest-*` are examples.

## User Scenarios & Testing _(mandatory)_

### User Story 1 - See every long-running crashloop without anything changing (Priority: P1)

As the cluster operator, I want the reaper to tell me which workloads have been
crash-looping past the threshold and what it would do about each. It must not
change anything, so I can install it on a production cluster with no staging
cluster and trust it first.

**Why this priority**: This is the safe default and the first thing deployed.
On its own it removes "nobody noticed for three days": every candidate shows up
as an Event, a metric (and therefore a Discord alert), and an annotation on the
workload.

**Independent Test**: Install in report mode on a cluster with one
deliberately crash-looping Deployment. After the threshold, the Deployment
carries a Warning Event and a planned-action annotation describing the exact
scale-down and Flux changes. The candidate metric is 1. `spec.replicas` and all
Flux objects are unchanged.

**Acceptance Scenarios**:

1. **Given** report mode and a Deployment whose only pod has been in
   CrashLoopBackOff for longer than the duration threshold with more restarts
   than the restart threshold, **When** a cycle runs, **Then** the Deployment
   gets a Warning Event `ReapPlanned`, a planned-action annotation and the
   `candidate` state label, and the candidate metric counts it.
2. **Given** the same Deployment, **When** many cycles run, **Then** its
   `spec.replicas`, its Flux annotations and any HelmRelease stay exactly as
   they were.
3. **Given** a workload that crash-loops for less than the duration threshold,
   or has fewer restarts than the restart threshold, **When** a cycle runs,
   **Then** it is recorded as `observed` (crashloop start time noted) but is not
   a candidate.
4. **Given** a candidate whose pods become healthy and stay healthy for the
   recovery window, **When** a cycle runs, **Then** the reaper removes its
   annotations and label and emits `CrashLoopRecovered`.
5. **Given** a crash-looping DaemonSet, bare Pod, or Pod owned by anything other
   than a ReplicaSet→Deployment or a StatefulSet, **When** the thresholds are
   exceeded, **Then** it is reported with the reason it cannot be acted on.
   This holds in either mode.

---

### User Story 2 - Stop a broken workload and keep Flux from restarting it (Priority: P2)

As the cluster operator, once I trust the reports, I want to switch to full mode
and have the reaper scale broken Deployments and StatefulSets to zero. If Flux
manages the workload, Flux must not scale it straight back up. Everything done
must be recorded on the objects themselves.

**Why this priority**: This is the point of the tool. It depends on Story 1's
detection and is only safe after report mode has been observed.

**Independent Test**: Switch the running configuration to full mode (no
redeploy). For a Flux-kustomize-managed crash-looping Deployment, after one
cycle: `replicas` is 0, annotations record the original replicas, reason and
time, and the object carries `kustomize.toolkit.fluxcd.io/reconcile: disabled`.
Force a Flux reconcile; `replicas` stays 0.

**Acceptance Scenarios**:

1. **Given** full mode and a candidate Deployment with zero available replicas
   and no Flux labels, **When** a cycle runs, **Then** it is scaled to 0. The
   original replica count, reason and timestamp are recorded in annotations, it
   gets the `reaped` state label, and a Warning Event `Reaped` is emitted.
2. **Given** a candidate that carries the kustomize-controller labels
   (`kustomize.toolkit.fluxcd.io/name` and `/namespace`), **When** it is reaped,
   **Then** the object is also annotated
   `kustomize.toolkit.fluxcd.io/reconcile: disabled`. Any value that annotation
   had before is recorded.
3. **Given** a candidate that carries the helm-controller labels
   (`helm.toolkit.fluxcd.io/name` and `/namespace`), **When** it is reaped,
   **Then** that HelmRelease gets `spec.suspend: true`. The HelmRelease records
   which workload(s) the reaper suspended it for. If the HelmRelease is itself
   kustomize-managed, it also gets the reconcile-disabled annotation, so the
   parent Kustomization cannot flip `suspend` back.
4. **Given** full mode, **When** the plan for a workload is computed, **Then**
   it is identical to the plan report mode would have published for the same
   inputs (dry-run equivalence).
5. **Given** more candidates than the per-cycle action limit, **When** a cycle
   runs, **Then** only that many are reaped (longest-crashing first). The rest
   get `ReapSkipped` with reason `RateLimited` and are reconsidered next cycle.
6. **Given** the number of currently reaped workloads is at the configured
   ceiling, **When** further candidates appear, **Then** they are reported
   (`ReapSkipped`, reason `ReapCeilingReached`) and not acted on.
7. **Given** a candidate that still has at least one available replica (for
   example a bad rollout while old pods still serve traffic), **When** a cycle
   runs, **Then** it is reported with reason `PartiallyAvailable` and not
   scaled.

---

### User Story 3 - Bring a workload back with one action (Priority: P2)

As the person who fixed the cause (or who wants to retry), I want one
documented action that puts a reaped workload back exactly as it was: replicas
restored, Flux reconciliation re-enabled, HelmRelease resumed. I must not have
to remember what the reaper changed.

**Why this priority**: Full mode must not ship without a reliable way back.
The constitution requires every mutation to be reversible in one step.

**Independent Test**: Reap a test Deployment (Story 2), then run
`kubectl annotate deploy/<name> reaper.boop.ninja/restore=true`. Within one
cycle `replicas` equals the recorded original. The reconcile-disabled
annotation is gone (or restored to its previous value). The HelmRelease is
unsuspended if no other reaped workload still needs it suspended. All reaper
annotations and labels are removed and a `Restored` Event is emitted.

**Acceptance Scenarios**:

1. **Given** a reaped workload, **When** a person sets the restore annotation,
   **Then** the reaper reverses exactly the recorded actions and nothing else.
   It emits `Restored`, and the workload starts a fresh observation (the full
   duration threshold applies again before any re-reap).
2. **Given** a reaped workload, **When** a person scales it up by hand
   (`replicas` > 0), **Then** the reaper treats that as a restore request: it
   reverses its Flux changes and clears its state, keeping the replica count
   the person chose.
3. **Given** two reaped workloads from the same HelmRelease, **When** only one
   is restored, **Then** the HelmRelease stays suspended until the last one is
   restored.
4. **Given** the restore action on a workload the reaper did not reap, **When**
   a cycle runs, **Then** the annotation is removed and a `RestoreIgnored` Event
   explains why. Nothing else changes.
5. **Given** report mode, **When** a person sets the restore annotation on a
   previously reaped workload, **Then** the restore still runs. Restore is
   always allowed, because it returns the cluster to its declared state.

---

### User Story 4 - Control scope and mode without redeploying (Priority: P3)

As the cluster operator, I want to switch mode, change thresholds and exclude
namespaces or individual workloads at runtime from Git (the cluster is
GitOps-managed). Teams that own a namespace should be able to opt a workload
out with a label.

**Why this priority**: The defaults cover the first rollout. Fine control
matters once full mode is on.

**Independent Test**: Change the mode in the cluster configuration object from
`report` to `full`; the next cycle acts without a pod restart. Label one
Deployment `reaper.boop.ninja/mode=off`; it is never observed, annotated or
reported. Label a namespace `reaper.boop.ninja/mode=report`; workloads there are
reported but never acted on, even in full mode.

**Acceptance Scenarios**:

1. **Given** a running reaper, **When** the configuration's mode or thresholds
   change, **Then** the next cycle uses the new values with no restart.
2. **Given** a namespace in the exclude list, the reaper's own namespace, or a
   namespace or workload with mode `off`, **When** cycles run, **Then** nothing
   in it is observed, annotated or changed.
3. **Given** a namespace or workload labelled or annotated with mode `report`,
   **When** global mode is `full`, **Then** its workloads are only reported. The
   most restrictive mode always wins; a label can never raise the mode above
   the global one.
4. **Given** an invalid configuration change, **When** it is applied, **Then**
   the reaper keeps using the last valid configuration and reports the problem
   on the configuration object's status and as an Event and metric.
5. **Given** no configuration object at all, **When** the reaper starts,
   **Then** it runs in report mode with the documented defaults.

### User Story 5 - Pause a CronJob that keeps failing (Priority: P3)

As the cluster operator, I want a CronJob whose runs have all failed for hours
to be paused (`spec.suspend: true`) instead of creating a failing Job, and its
pods, on every schedule tick. Flux must not unpause it, and the way back must be
the same single restore action as for Deployments.

**Why this priority**: A CronJob that fails every run wastes resources and
buries real alerts, but each run ends by itself, so it costs less than a crash
loop. It reuses the US2 and US3 machinery (record, Flux holds, limits, restore)
and only adds detection and a different "stop" action.

**Independent Test**: In a dev cluster, with the CronJob failing duration set
to 10 minutes and the minimum failed runs set to 3, create a CronJob scheduled
every minute whose container exits 1. In report mode it becomes a candidate
after about 10 minutes and nothing changes. In full mode it gets
`spec.suspend: true` and the `reaped` state label. Running
`kubectl annotate cronjob/<name> reaper.boop.ninja/restore=true` sets
`spec.suspend: false` again and removes every reaper annotation.

**Acceptance Scenarios**:

1. **Given** a CronJob whose most recent finished Job failed, **When** a cycle
   runs, **Then** the reaper records when the failing streak began (the
   completion time of the first failed Job after the last success) and counts
   each distinct failed Job it sees. The CronJob gets the `observed` state and a
   `JobFailuresObserved` Event.
2. **Given** a failing streak of at least the failing duration (default 6
   hours) **and** at least the minimum failed runs (default 3), **When** a
   cycle runs, **Then** the CronJob becomes a candidate. In report mode the
   reaper publishes the plan and emits `ReapPlanned`. In full mode it records
   the reason, sets `spec.suspend: true`, applies the same Flux holds as US2
   scenarios 2 and 3, sets the `reaped` state, and emits `Reaped`. Jobs that are
   already running are left alone.
3. **Given** a failing CronJob, **When** one of its Jobs succeeds, **Then** the
   streak ends: the state is cleared and `JobFailuresRecovered` is emitted. No
   recovery window applies, because a success is definitive.
4. **Given** a CronJob that is already suspended by someone else, **When**
   cycles run, **Then** the reaper never pauses it, and it clears any
   observation state it had recorded.
5. **Given** a paused CronJob, **When** a person sets the restore annotation,
   or sets `spec.suspend: false` themselves, **Then** the reaper releases its
   Flux holds, clears its state and emits `Restored`. On a restore annotation
   the reaper also sets `spec.suspend: false`. The failing streak starts again
   from zero.
6. **Given** `cronJobs.enabled: false` in the configuration, **When** cycles
   run, **Then** CronJobs are not observed. CronJobs that are already paused
   can still be restored.

---

### Edge Cases

- **Operator restarts mid-observation**: the crashloop start time is stored on
  the workload, so a restart neither resets nor skips the threshold.
- **Operator restarts between steps of a reap** (for example scaled, but Flux
  annotation not yet set): each step is recorded as it completes. The next cycle
  finishes the remaining steps; it does not start over or double-apply.
- **Flapping container** (runs a few seconds, then crashes): crashloop state
  only clears after the recovery window of continuous health, not on a single
  healthy observation.
- **Init container crash-looping** (`Init:CrashLoopBackOff`): counts the same as
  an app container.
- **Image pull failures** (`ImagePullBackOff`, `ErrImagePull`): not crash loops,
  out of scope. Nothing restarts, so nothing is wasted.
- **Workload already at zero replicas**: nothing to do; never a candidate.
- **HPA on the workload**: scaling to 0 pauses the HPA (HPA does not act on a
  target at 0). Restore sets the recorded replicas and the HPA resumes. A KEDA
  ScaledObject or another autoscaler that would fight the scale-down makes the
  workload report-only (`ManagedByAutoscaler`).
- **Workload owned by another controller** (the Deployment or StatefulSet has a
  controller ownerReference, as with operator-managed databases): report-only
  (`ManagedByController`). Acting would race that controller.
- **Applied by plain `kubectl apply`** (no Flux labels, as `slam-prod` is today):
  scaled with no Flux step. The next `kubectl apply` from the owner's pipeline
  restores the declared replicas. If the workload is still broken, it is
  observed again from scratch.
- **Object deleted or recreated by its owner** while reaped: the recorded state
  goes with it. If a HelmRelease still lists that workload, the reaper drops it
  from the list and resumes the HelmRelease if nothing else needs it suspended.
- **HelmRelease already suspended by a person** before the reaper acted: the
  reaper records that it did not suspend it, and restore leaves it suspended.
- **Reconcile annotation already set by a person** (`disabled`): the previous
  value is recorded, and restore puts that value back rather than deleting it.
- **Mass failure** (for example a registry or node outage makes many workloads
  crash at once): the per-cycle limit and the reaped ceiling bound the damage.
  The remaining candidates are reported, not acted on.
- **API errors** during a cycle: that workload's step fails with
  `ReapFailed`/`RestoreFailed` and a metric. Other workloads in the cycle still
  proceed, and the step is retried next cycle.
- **Jobs and CronJobs**: their pods never count as crash loops. Standalone
  Jobs are out of scope. CronJobs are judged by their Jobs' outcomes (US5).
- **CronJob with a sparse schedule** (for example daily): the failing duration
  passes long before the minimum failed runs, so it stays `observed` until
  enough runs have failed. One bad night never pauses a daily job.
- **Failed Jobs deleted quickly** (`failedJobsHistoryLimit` defaults to 1): the
  reaper counts failed runs itself as it sees them, recording the last failed
  Job's name, so history limits do not hide the streak. A Job created and
  deleted entirely between two cycles is not counted, which only delays a
  pause.
- **A long-running Job is still active** when the CronJob is paused: it keeps
  running. Pausing only stops new runs, and the reaper never deletes Jobs.

## Requirements _(mandatory)_

### Functional Requirements

**Detection**

- **FR-001**: The reaper MUST treat a container (app or init) as crash-looping
  when it is not ready and either is waiting with reason `CrashLoopBackOff` or
  last terminated with an error. Its restart count is taken from the container
  status.
- **FR-002**: The reaper MUST resolve each crash-looping pod to its workload:
  Pod → ReplicaSet → Deployment, Pod → StatefulSet, or Pod → DaemonSet. Pods
  owned by Jobs are ignored. Bare pods and other owners are reported as
  unsupported.
- **FR-003**: The reaper MUST record when it first observed a workload
  crash-looping, persistently on the workload, and MUST NOT reset that time
  unless the workload has been healthy for the full recovery window (default 10
  minutes).
- **FR-004**: A workload MUST become a candidate only when it has been
  crash-looping for at least the duration threshold (default 30 minutes) **and**
  a crash-looping container's restart count is at least the restart threshold
  (default 10).

**Decision (identical in both modes)**

- **FR-005**: For each candidate the reaper MUST compute a plan: an ordered list
  of concrete actions (scale to 0; set or record the kustomize reconcile
  annotation; suspend a named HelmRelease), or a skip with exactly one reason
  from a fixed set (`OptedOut`, `ReportOnlyScope`, `CannotScaleToZero`,
  `UnsupportedOwner`, `ManagedByController`, `ManagedByAutoscaler`,
  `PartiallyAvailable`, `RateLimited`, `ReapCeilingReached`).
- **FR-006**: The plan MUST be a pure function of the observed objects, the
  configuration and the current time. Report mode and full mode MUST produce the
  same plan for the same inputs (dry-run equivalence).
- **FR-007**: The reaper MUST NOT act on a workload with at least one available
  replica (`PartiallyAvailable`).
- **FR-008**: DaemonSets MUST be report-only (`CannotScaleToZero`).

**Modes and scope**

- **FR-009**: Global mode MUST be `report` (default) or `full`, read at runtime
  from cluster configuration and re-read without a restart.
- **FR-010**: Namespaces and workloads MAY declare mode `off` or `report` with
  the `reaper.boop.ninja/mode` label or annotation. The effective mode MUST be
  the most restrictive of global, namespace and workload (`off` < `report` <
  `full`).
- **FR-011**: The reaper MUST NEVER observe or act on its own namespace,
  whatever the configuration says. It MUST skip namespaces in the exclude list
  (default `kube-system`, `kube-public`, `kube-node-lease`, `flux-system`). If an
  include list is set (glob patterns, e.g. `slam-*`), it MUST consider only
  matching namespaces.
- **FR-012**: In report mode the reaper MUST NOT change `replicas`, Flux
  annotations or HelmReleases. Its only writes are its own `reaper.boop.ninja/*`
  annotations and labels, and Events.

**Action (full mode)**

- **FR-013**: When reaping, the reaper MUST first record on the workload the
  original replica count, the reason (container, restart count, crashloop start)
  and the reap time. It then scales to 0 and sets the `reaped` state label.
- **FR-014**: If the workload has both kustomize-controller labels, the reaper
  MUST set `kustomize.toolkit.fluxcd.io/reconcile: disabled` on it and record
  the previous value of that annotation (or that there was none).
- **FR-015**: If the workload has both helm-controller labels, the reaper MUST
  set `spec.suspend: true` on that HelmRelease (only if it was not already
  suspended). It MUST record on the HelmRelease the set of workloads it is
  suspended for, and whether the reaper or someone else suspended it. If the
  HelmRelease carries kustomize-controller labels, FR-014 applies to the
  HelmRelease object too.
- **FR-016**: Each completed step MUST be recorded before the next begins, so an
  interrupted reap is completed, not repeated, on the next cycle.
- **FR-017**: The reaper MUST perform at most the configured number of reaps per
  cycle (default 1) and hold at most the configured number of workloads reaped
  at once (default 5). Ordering MUST be deterministic: longest crash-looping
  first, then namespace/name.

**Restore**

- **FR-018**: Setting annotation `reaper.boop.ninja/restore` (any non-empty
  value) on a reaped workload MUST make the reaper, within one cycle, reverse
  exactly the recorded actions. It scales back to the original replicas, restores
  the previous reconcile annotation value, and removes the workload from the
  HelmRelease's list (unsuspending it only when the list is empty and the reaper
  was the one that suspended it). It then removes all of its own annotations and
  labels from the workload and emits `Restored`.
- **FR-019**: A manual scale-up of a reaped workload MUST be treated as a
  restore that keeps the person's replica count.
- **FR-020**: Restore MUST work in every mode, including report, and MUST NOT
  be rate-limited.

**Reporting**

- **FR-021**: Every state change (observed, candidate/planned, reaped, skipped,
  restored, recovered, failed) MUST emit a Kubernetes Event on the workload,
  using the reasons in the Events contract.
- **FR-022**: The reaper MUST expose metrics (see Metrics contract) for
  scraping, sufficient for an alert rule per workload on "planned",
  "reaped" and "action failed". It MUST also export them over OpenTelemetry when
  an OTLP endpoint is configured.
- **FR-023**: The reaper MUST label workloads with their state
  (`reaper.boop.ninja/state=observed|candidate|reaped`) so that
  `kubectl get deploy,sts -A -l reaper.boop.ninja/state=reaped` lists everything
  it holds stopped.
- **FR-024**: The reaper MUST publish a summary on its configuration object's
  status: effective config generation, last cycle time, counts per state, and a
  validity condition.
- **FR-025**: The reaper MUST expose liveness and readiness endpoints. It is
  ready only after its caches have synced and it has a valid configuration.

**Configuration**

- **FR-026**: With no configuration object present, the reaper MUST run with the
  documented defaults (report mode). An invalid object MUST NOT replace the last
  valid configuration.
- **FR-027**: Flux integration MUST be switchable at runtime: the kustomize
  and HelmRelease steps are separately enabled (default both on). When a needed
  step is disabled, a Flux-managed workload is report-only (`ReportOnlyScope`),
  because scaling without it would just be undone.

**CronJobs**

- **FR-028**: The reaper MUST treat a CronJob as failing when its most recently
  finished Job failed (Job condition `Failed=True`). The failing streak begins
  at the completion time of the first failed Job after the CronJob's
  `status.lastSuccessfulTime`. It is recorded on the CronJob and ends when a Job
  succeeds.
- **FR-029**: The reaper MUST count distinct failed Jobs in the streak itself
  (recording the last one counted), because Job history limits delete old Jobs.
  A CronJob MUST become a candidate only when the streak has lasted at least the
  failing duration (default 6 hours) **and** at least the minimum failed runs
  have been counted (default 3).
- **FR-030**: Pausing a CronJob MUST mean recording the reason and then setting
  `spec.suspend: true`. The reaper MUST NOT delete or stop Jobs. A CronJob that
  is already suspended is never a candidate.
- **FR-031**: FR-005 to FR-017 apply to CronJobs as workloads, with "scale to
  0" replaced by "suspend". The skip reasons `CannotScaleToZero`,
  `PartiallyAvailable` and `ManagedByAutoscaler` never apply to them. Paused
  CronJobs count toward the same per-cycle limit and reaped ceiling.
- **FR-032**: The restore annotation on a paused CronJob MUST set
  `spec.suspend: false` and reverse the Flux holds as in FR-018. A person
  setting `spec.suspend: false` on a paused CronJob MUST be treated as a restore
  (the counterpart of FR-019).
- **FR-033**: CronJob handling MUST be switchable at runtime
  (`cronJobs.enabled`, default on). Turning it off stops observation but MUST
  NOT block restoring CronJobs that are already paused.

### Key Entities

- **Workload**: a Deployment, StatefulSet or DaemonSet that owns crash-looping
  pods, or a CronJob whose Jobs keep failing. Attributes: identity, desired and available replicas, Flux management
  labels, the reaper's recorded state.
- **Crashloop observation**: when the workload was first seen crash-looping,
  the worst container (name, restart count, last exit) and when it was last
  seen healthy.
- **Plan**: the decision for one workload in one cycle. Either an ordered list
  of actions, or a skip with one reason.
- **Reap record**: what was actually done to a workload: original replicas,
  reason, time, previous reconcile annotation, HelmRelease reference and
  whether the reaper suspended it. It is enough to reverse the reap.
- **HelmRelease hold**: on a HelmRelease, the set of workloads the reaper
  suspended it for and whether the suspension was the reaper's.
- **Reaper configuration**: cluster-wide mode, thresholds, scope, limits and
  Flux toggles, plus status.

## Success Criteria _(mandatory)_

### Measurable Outcomes

- **SC-001**: In report mode, every workload crash-looping past the threshold
  is visible within 2 cycles (≤ 2 minutes at defaults) as an Event, a metric
  series and a state label. Zero changes are made to replicas or Flux objects
  (verified by diffing those fields before and after a 24-hour run).
- **SC-002**: In full mode, a qualifying workload stops restarting within 2
  cycles of becoming a candidate. It stays at zero through at least 3 forced
  Flux reconciles.
- **SC-003**: One restore action returns a reaped workload to its exact
  pre-reap replicas, reconcile annotation and HelmRelease suspension state
  within 1 cycle, in 100% of the integration test scenarios.
- **SC-004**: For the same recorded inputs, the plan published in report mode
  and the plan executed in full mode are identical. This is asserted for every
  core test case.
- **SC-005**: The reaper never changes an object in an excluded namespace, an
  opted-out workload, or its own namespace (asserted by integration tests and by
  the absence of reaper write events for those in the cluster audit log).
- **SC-006**: On roanoke today, the six `slam-prod` Deployments and
  `fathom-dev/crashloop` show up as candidates in report mode. The
  `ImagePullBackOff` pod does not. The failing `ingest` Job pods are never crash
  loop candidates. If their CronJob has failed every run for 6 hours, it shows
  up as a CronJob candidate (US5).
- **SC-007**: Core decision logic is covered by deterministic tests that run in
  under 10 seconds with no cluster.
- **SC-008**: In full mode, a CronJob that has failed every run for at least the
  failing duration, with at least the minimum failed runs, is suspended within 2
  cycles and stays suspended through 3 forced Flux reconciles. A CronJob with a
  successful run inside the window is never suspended. One restore action
  returns `spec.suspend` to `false` within 1 cycle.

## Assumptions

- **Defaults**: duration threshold 30 minutes; restart threshold 10 (about 25
  minutes of kubelet backoff); recovery window 10 minutes; cycle interval 60 s;
  1 reap per cycle; at most 5 workloads reaped at once.
- **Annotation/label/API prefix** is `reaper.boop.ninja`, grouping it with the
  owner's other boop.ninja services (Question 1, answered 2026-10-10).
- **Configuration lives in a cluster-scoped custom resource** with a single
  instance named `default`. It is rendered by the Helm chart and changed through
  Git like everything else. Process settings (metrics port, log level, OTLP
  endpoint) come from flags/env.
- **One replica** of the reaper runs (Recreate strategy). Leader election is
  not in the MVP; it can be added later without a contract change.
- **"Available replica"** follows the workload's own status (`availableReplicas`
  for Deployments, `readyReplicas` for StatefulSets).
- **Jobs** are out of scope: their pods are ignored, not reported. CronJobs
  are handled by their Jobs' outcomes (US5; Question 4).
- **CronJob defaults**: failing duration 6 hours (owner's request); minimum
  failed runs 3, so one or two failures of an infrequent job never pause it.
- **Notification** happens through existing alerting: Events and metrics →
  Prometheus rules (shipped with the chart) → Alertmanager → Discord. A direct
  webhook notifier is a later feature.
- **Flux behaviour**: an in-cluster object annotated
  `kustomize.toolkit.fluxcd.io/reconcile: disabled` is skipped by
  kustomize-controller. A suspended HelmRelease is skipped by helm-controller.
  Both are documented Flux v2 behaviour.
- **Restore before fix**: while a workload is reaped and reconcile-disabled, a
  fix pushed to Git does not reach it. The user guide says: restore, then let
  Flux apply the fix (or restore after pushing it).
- **Tenant namespaces** (`slam-*`) are in scope like any other namespace,
  because they are the motivating case (Question 3, answered).
- **Delivery**: Helm chart in `mbround18/helm-charts` (`charts/k8s-reaper`),
  installed by a HelmRelease in `fluxified`. Neither is built in this feature.

## Owner Decisions

All four questions were answered on 2026-10-10.

### Question 1: API group and annotation prefix

**Answer** (2026-10-10): `reaper.boop.ninja`. Every annotation, label, Event
reporter and the CRD group use this prefix. Changing it after release is a
breaking change.

### Question 2: Act on partially available workloads?

**Answer** (2026-10-10): A, never act. A workload with an available replica is
reported but never scaled (FR-007). A stuck rollout keeps restarting, but the
reaper never causes an outage.

### Question 3: Tenant namespaces in full mode

**Answer** (2026-10-10): A, treat them like any namespace. Once full mode is on,
the `slam-*` crash-loopers are reaped. The tenant can still opt out with the
`reaper.boop.ninja/mode` namespace label.

### Question 4: CronJobs

**Answer** (2026-10-10): support CronJobs: "if the cron job is constantly
failing for 6 hours pausing it". This became US5 (FR-028 to FR-033).

The details below were filled in by default, and the owner can change them:

- "Constantly failing" means every finished run since the streak began has
  failed: no successful Job in that time.
- The streak needs at least 3 failed runs as well as 6 hours, so a job that
  runs every few hours is not paused after one or two failures.
- Pausing never kills a Job that is already running.
- Paused CronJobs share the reap limits with Deployments and StatefulSets.
- The state label, plan annotation, Flux holds and restore action are the same
  as for Deployments.
