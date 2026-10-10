# Data Model: MVP Crashloop Reaper

Two layers:

- the plain-data **core model** (`reaper-core`, no Kubernetes types);
- the **persisted shapes**: the `ReaperConfig` CR and the annotations on
  workloads and HelmReleases.

Exact keys and wire formats are in [contracts/](./contracts/).

## Core model (input)

```text
Snapshot
  now: Timestamp                      # injected; never read from a clock in core
  reaper_namespace: String            # hard-excluded
  namespaces: Map<name, NamespaceView>
  workloads: Vec<WorkloadView>
  helm_releases: Map<NsName, HelmReleaseView>
  keda_targets: Set<WorkloadRef>      # empty when KEDA is not installed

NamespaceView { name, mode_override: Option<Mode> }      # from label/annotation

WorkloadRef { kind: Deployment|StatefulSet|DaemonSet, namespace, name }

WorkloadView
  ref: WorkloadRef
  spec_replicas: Option<u32>          # None for DaemonSet
  available_replicas: u32
  has_controller_owner: bool          # ownerReference with controller=true
  has_hpa: bool
  flux: FluxRefs
  mode_override: Option<Mode>         # workload label/annotation
  reaper: ReaperAnnotations           # decoded, see below
  pods: Vec<PodView>                  # pods resolved to this workload (Job pods dropped)

FluxRefs
  kustomization: Option<NsName>       # both kustomize.toolkit.fluxcd.io labels present
  helm_release: Option<NsName>        # both helm.toolkit.fluxcd.io labels present
  reconcile_annotation: Option<String># current kustomize.toolkit.fluxcd.io/reconcile

PodView { name, containers: Vec<ContainerView> }          # app + init containers
ContainerView
  name, init: bool, ready: bool, restart_count: u32
  waiting_reason: Option<String>
  last_exit: Option<{ code: i32, reason: String }>

HelmReleaseView
  ref: NsName, suspended: bool, kustomization: Option<NsName>
  reconcile_annotation: Option<String>
  hold: Option<HelmReleaseHold>       # decoded reaper annotations
```

Unsupported owners (a bare Pod, or a Pod owned by some other kind) appear as
`UnsupportedPod { namespace, pod, owner_kind }` entries, so they can be
reported (US1 scenario 5).

## Core model (output)

```text
CyclePlan { decisions: Vec<Decision> }      # deterministic order (FR-017)

Decision { workload: WorkloadRef, transition: Transition, plan: Plan }

Plan =
  | Observe { since, worst: ContainerSummary }
  | Candidate { since, worst, actions: Vec<Action> }   # what full mode will do
  | Reap { actions: Vec<Action> }                      # remaining steps only
  | Restore { kind: Requested|ManualScaleUp, actions: Vec<Action> }
  | Recover                                            # clear state after recovery window
  | Skip { reason: SkipReason, would: Vec<Action> }    # `would` kept for reporting
  | Nothing

Action =
  | RecordReap { original_replicas, reason_text, previous_reconcile: Option<Option<String>> }
  | DisableKustomizeReconcile { target: ObjectRef }    # workload or HelmRelease
  | HoldHelmRelease { hr: NsName, suspend: bool }      # suspend only if not already
  | ScaleTo { replicas: u32 }
  | MarkReaped { at }
  | ReleaseHelmRelease { hr: NsName, resume: bool }
  | RestoreKustomizeReconcile { target: ObjectRef, previous: Option<String> }
  | ClearReaperState

SkipReason = OptedOut | ReportOnlyScope | CannotScaleToZero | UnsupportedOwner
           | ManagedByController | ManagedByAutoscaler | PartiallyAvailable
           | RateLimited | ReapCeilingReached

Mode = Off < Report < Full        # effective = min(global, namespace, workload)
```

`Plan` serialises to a stable JSON form (sorted keys, no timestamps other than
those in the inputs). The `reaper.boop.ninja/plan` annotation is that JSON. The
executor in report mode publishes it; in full mode it publishes it and then
executes it. SC-004 compares the two byte for byte.

## Workload lifecycle (state label `reaper.boop.ninja/state`)

```text
            crash-looping seen
   (none) ───────────────────────► observed
     ▲                               │ duration ≥ threshold AND restarts ≥ N
     │ healthy ≥ recoveryWindow      ▼
     ├──────────────────────────── candidate ──(report mode / skip)──► stays candidate
     │                               │ full mode, not skipped, within limits
     │                               ▼
     │                            reaping   (steps 1–3 of research R6; resumable)
     │                               │ scaled to 0
     │                               ▼
     │                            reaped
     │      restore annotation       │       replicas > 0 set by someone
     └───────────── or ──────────────┴───────────────────────────────────
                 (restore steps; all reaper annotations and labels removed)
```

- `observed`/`candidate` → none also happens if the workload is deleted, scaled
  to 0 by someone else, or opted out (`mode=off`: the reaper clears its own
  annotations once, then ignores the workload).
- `reaping` with replicas > 0 means "continue reaping", **not** a manual
  scale-up. Only `reaped` treats replicas > 0 as a restore.

## Validation rules (config)

| Field                         | Rule                                         | Default                                                         |
| ----------------------------- | -------------------------------------------- | --------------------------------------------------------------- |
| `mode`                        | `report` or `full`                           | `report`                                                        |
| `detection.crashLoopDuration` | duration, 5m–7d                              | `30m`                                                           |
| `detection.minRestarts`       | integer 1–10000                              | `10`                                                            |
| `detection.recoveryWindow`    | duration, 1m–1d                              | `10m`                                                           |
| `cycleInterval`               | duration, 10s–10m                            | `60s`                                                           |
| `limits.maxReapsPerCycle`     | integer 0–50 (0 = report everything)         | `1`                                                             |
| `limits.maxReaped`            | integer 0–500                                | `5`                                                             |
| `scope.excludeNamespaces`     | list of names/globs                          | `kube-system`, `kube-public`, `kube-node-lease`, `flux-system`  |
| `scope.includeNamespaces`     | list of globs; empty = all                   | `[]`                                                            |
| `flux.kustomize`              | bool                                         | `true`                                                          |
| `flux.helmRelease`            | bool                                         | `true`                                                          |

The CRD schema enforces type, enum and range. The core re-validates and
returns `ConfigError` (a list of field errors). On error, the last valid config
stays active (FR-026).

## Relationships

- Workload 1 → 0..1 Kustomization (labels): the reaper only annotates the
  object, never the Kustomization.
- Workload 1 → 0..1 HelmRelease (labels). HelmRelease 1 → many workloads, via
  the `held-for` set.
- ReaperConfig: exactly one is used (`default`); others are ignored with an
  Event.
