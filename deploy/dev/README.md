# Local dev cluster

A k3d cluster with Flux and the crash-looping fixtures from
[quickstart §1](../../specs/001-crashloop-reaper/quickstart.md). For local
development only.

Needs `docker`, `k3d` (v5), `kubectl`, `flux` and `helm` on `PATH`.

## Up

```bash
make dev-cluster                 # FLUX_VERSION=v2.7.0 by default; override if needed
make image dev-load              # build the reaper image and import it into k3d
make dev-run                     # run the reaper in report mode
```

`make dev-cluster` creates the cluster from `k3d.yaml` (one server, one agent,
and a registry at `localhost:5001`). It then installs Flux, applies the
ReaperConfig CRD, and pushes the Flux payloads to the registry
(`make dev-push`). After that it runs `kubectl apply -k deploy/dev` and breaks
the `partial/rollout` rollout with `fixtures/partial-break.yaml`.

Re-running `kubectl apply -k deploy/dev` resets `partial/rollout` to healthy.
To break it again, re-apply the patch:

```bash
kubectl -n partial patch deploy rollout --type=strategic --patch-file deploy/dev/fixtures/partial-break.yaml
```

## Notifications (US6)

```bash
make dev-notify                  # echo server + Mailpit in notify-dev, the Secret, reasons: [ReapPlanned]
make test-integration STORY=us6_notify
```

`notify/secret.yaml` holds two channels: `dev-echo` (generic webhook to the
echo server) and `dev-mail` (SMTP to Mailpit). Both use `insecure=true`, which
is for this dev cluster only. The reaper's Deployment must mount the Secret at
`/etc/k8s-reaper/notifications` (`optional: true`). To read what arrived:

```bash
kubectl -n notify-dev logs deploy/echo
kubectl -n notify-dev port-forward svc/mailpit 8025   # then http://localhost:8025
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

`reaperconfig.yaml` is the dev `ReaperConfig/default`. It uses report mode,
`crashLoopDuration: 5m`, `minRestarts: 3`, `cycleInterval: 15s` and
`maxReapsPerCycle: 1`, so the fixtures become candidates within about five
minutes.
