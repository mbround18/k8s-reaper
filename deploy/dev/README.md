# Local dev cluster

A k3d or kind cluster with Flux and the crash-looping fixtures from
[quickstart §1](../../specs/001-crashloop-reaper/quickstart.md). For local
development only.

Needs `docker`, `kubectl`, `flux` and `helm` on `PATH`, plus `k3d` (v5) or
`kind`.

Every target names its cluster through `KUBE_CONTEXT` and passes it as
`--context`; the current kube context is never used. Targets refuse a context
that does not start with `k3d-` or `kind-`, and the integration tests also
refuse an API server that is not on loopback.

## Up: new k3d cluster

```bash
make dev-cluster                 # FLUX_VERSION=v2.7.0 by default; override if needed
make image dev-load              # build the reaper image and import it into k3d
make dev-run                     # run the reaper in report mode
```

`make dev-cluster` creates the cluster from `k3d.yaml` (one server, one agent,
and a registry at `localhost:5001`) and installs Flux. Then `make
dev-fixtures`:

1. applies the ReaperConfig CRD and waits until it is established;
2. pushes the Flux payloads to the registry (`make dev-push`);
3. runs `kubectl apply -k deploy/dev`;
4. breaks the `partial/rollout` rollout with `fixtures/partial-break.yaml`.

## Up: existing kind cluster

For a kind cluster that already runs Flux (2.7 or later). The fixtures, the
reaper and the catchers get their own namespaces, and the dev ReaperConfig only
looks at the fixture namespaces.

```bash
make dev-kind image dev-load dev-run KUBE_CONTEXT=kind-<name>
```

`dev-kind` starts a registry container named `k3d-reaper-registry` on the
`kind` Docker network, published at `localhost:5001`. It does not install Flux,
so the cluster's own Flux is left as it is. Then it runs `make dev-fixtures`.
To remove all of it and keep the cluster:

```bash
make dev-clean KUBE_CONTEXT=kind-<name>
```

## Resetting `partial/rollout`

Re-running `kubectl apply -k deploy/dev` resets `partial/rollout` to healthy.
To break it again, re-apply the patch:

```bash
kubectl --context <ctx> -n partial patch deploy rollout --type=strategic --patch-file deploy/dev/fixtures/partial-break.yaml
```

## Notifications (US6)

```bash
make dev-notify                  # echo server + Mailpit in notify-dev, the Secret, reasons: [ReapPlanned]
make test-integration STORY=us6_notify   # with KUBE_CONTEXT=kind-<name> on kind
```

`notify/secret.yaml` holds two channels: `dev-echo` (generic webhook to the
echo server) and `dev-mail` (SMTP to Mailpit). Both use `insecure=true`, which
is for this dev cluster only. `reaper.yaml` mounts the Secret at
`/etc/k8s-reaper/notifications` (`optional: true`), and the reaper re-reads it
every cycle, so no restart is needed after `make dev-notify`. To read what
arrived:

```bash
kubectl --context <ctx> -n notify-dev logs deploy/echo
kubectl --context <ctx> -n notify-dev port-forward svc/mailpit 8025   # then http://localhost:8025
```

## Down

```bash
make dev-down                    # deletes the cluster and its registry
```

## Fixtures

| Fixture               | Source                                                 | Expect                    |
| --------------------- | ------------------------------------------------------ | ------------------------- |
| `plain/crashloop`     | `fixtures/plain.yaml`                                  | candidate → reaped (full) |
| `kustomize/crashloop` | `flux/kustomize`, applied by `flux-system/dev-kustomize` | + reconcile disabled    |
| `helm/crashloop`      | `flux/charts/crashloop`, via HelmRelease `helm/crashloop` | + HelmRelease suspended |
| `partial/rollout`     | `fixtures/partial.yaml` + `fixtures/partial-break.yaml` | `PartiallyAvailable`     |
| `ds/crashloop`        | `fixtures/ds.yaml`                                     | `CannotScaleToZero`       |
| `job/fail`            | `fixtures/job.yaml`                                    | ignored                   |
| `optout/crashloop`    | `fixtures/optout.yaml` (`reaper.boop.ninja/mode=off`)  | untouched                 |
| `sts/crashloop`       | `fixtures/sts.yaml`                                    | candidate → reaped (full) |
| `imagepull/missing`   | `fixtures/imagepull.yaml`                              | never a candidate         |
| `healthy/control`     | `fixtures/healthy.yaml`                                | never a candidate         |
| `cron/failing`        | `fixtures/cron.yaml`                                   | candidate → suspended (full) |

`reaperconfig.yaml` is the dev `ReaperConfig/default`. It uses report mode,
`crashLoopDuration: 5m`, `minRestarts: 3`, `cycleInterval: 15s` and
`maxReapsPerCycle: 1`, so the fixtures become candidates within about five
minutes.
