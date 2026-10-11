# Contributing to k8s-reaper

This is the detailed guide for working on k8s-reaper: setup, architecture,
tests, the `make` targets, releases and commits. The [README](README.md) stays
short and user-facing. Anything a user has to learn goes in
[`docs/guides/`](docs/guides/).

## Developer setup

### Toolchain

- **Rust 1.99**, edition 2024. `rust-toolchain.toml` pins the channel and the
  `rustfmt` and `clippy` components, so `rustup` installs them on first use.
- **cargo-sweep** (optional). `make gc` uses it to clear stale `target/`
  output. If it is missing, `make gc` prints a note and continues. Install it
  with:

  ```sh
  cargo install cargo-sweep --locked
  ```

- **Docker**, to build the image and to run a dev cluster.

That is enough for `make build` and `make test`.

### Dev cluster tools

The integration stories run against a local cluster with Flux. They need, on
`PATH`:

- `kubectl`;
- `kind` or `k3d` (v5);
- the Flux CLI (`flux`), 2.7 or later;
- `helm`, to package and push the fixture chart.

### Dev cluster

[`deploy/dev/README.md`](deploy/dev/README.md) has the full walk-through and
the fixture table. In short:

```sh
# A new k3d cluster with Flux, a registry and the fixtures
make dev-cluster
make image dev-load dev-run

# Or an existing kind cluster that already runs Flux
make dev-kind image dev-load dev-run KUBE_CONTEXT=kind-<name>
```

- `make dev-cluster` creates a k3d cluster from `deploy/dev/k3d.yaml`,
  installs Flux (`FLUX_VERSION`, default `v2.7.0`) and runs `make
  dev-fixtures`.
- `make dev-kind` does not install Flux. It starts a registry container on the
  `kind` network at `localhost:5001`, then runs `make dev-fixtures`.
- `make dev-fixtures` applies the ReaperConfig CRD (printed by `k8s-reaper
  crd`), pushes the Flux payloads (`make dev-push`), applies `deploy/dev` and
  breaks the `partial/rollout` fixture.
- `make image` builds `k8s-reaper:dev`. `make dev-load` loads it into the
  cluster. `make dev-run` applies `deploy/dev/reaper.yaml` and restarts the
  reaper in the `k8s-reaper` namespace, in report mode.
- `make dev-push` re-pushes the Flux fixture payloads after you change them.
- `make dev-clean` removes what `dev-kind` added and keeps the cluster.
  `make dev-down` deletes the k3d cluster and its registry.

### The dev guard

Every dev target names its cluster through `KUBE_CONTEXT` and passes it to
`kubectl` and `flux` as `--context`. The current kube context is never used.
The default is `k3d-k8s-reaper-dev`, the cluster `make dev-cluster` creates.
On kind, pass `KUBE_CONTEXT=kind-<name>` to every command.

There are two checks, so a dev command cannot reach a real cluster by
accident:

- The `dev-guard` Make target refuses any `KUBE_CONTEXT` that does not start
  with `kind-` or `k3d-`. Most dev targets and `test-integration` depend on
  it.
- The integration tests (`apps/k8s-reaper/tests/common/mod.rs`) repeat that
  check, and also refuse a context whose API server is not on loopback
  (`127.0.0.1`, `localhost`, `0.0.0.0` or `[::1]`).

A kind or k3d name alone is not proof of a local cluster. The loopback check
catches a context named `kind-…` that points somewhere else.

## Architecture

k8s-reaper uses ports and adapters (hexagonal architecture). One pure core
decides what to do. Adapters connect it to Kubernetes, Flux, telemetry and
notification channels. The binary wires them together.

```text
                         apps/k8s-reaper
          (clap flags, run loop, wiring; no decision logic)
                               |
      +-------------+----------+----------+-------------------+
      |             |                     |                   |
 reaper-kube   reaper-flux     reaper-observability     reaper-notify
 watch caches  HelmRelease     metrics (OTel +          Discord, Slack,
 -> Snapshot,  type, hold      Prometheus), tracing,    webhooks, SMTP
 ClusterWriter, patches        /healthz /readyz
 EventSink,                    /metrics, Telemetry
 ReaperConfig CRD
      |             |                     |                   |
      +-------------+----------+----------+-------------------+
                               |  depend on
                               v
                          reaper-core
     model, config, detect, decide, plan, record, hold, executor,
     ports (ClusterWriter, EventSink, Telemetry), notify model
     pure: no I/O, no Kubernetes types, no async runtime, no clock
```

### reaper-core

`crates/reaper-core` is plain data in, plain data out.

- It depends only on `serde`, `serde_json` and `thiserror`.
- It has no Kubernetes client or types, no async runtime and no clock. Time is
  `Timestamp(i64)` (`time.rs`), and "now" is always a parameter, carried in
  the snapshot.
- Ports are traits in `ports.rs`: `ClusterWriter`, `EventSink` and
  `Telemetry`. They use native `async fn` in traits, so the core needs no
  runtime.
- `testing.rs` has fixture builders and fakes (`FakeWriter`, `FakeEvents`,
  `FakeTelemetry`) for tests.

`crates/reaper-core/tests/no_infra_deps.rs` enforces this. It reads `cargo
metadata` and fails if `kube`, `k8s-openapi`, `tokio`, `chrono`, `jiff` or
`time` show up in the core's dependency tree.

### Adapters

- **reaper-kube**: the kube-rs adapter. Watch-backed caches turned into a
  core `Snapshot`, the `ClusterWriter` (merge patches with a `resourceVersion`
  precondition, the `scale` subresource), the `EventSink` over
  `events.k8s.io/v1`, the `ReaperConfig` CRD type and the config watch.
- **reaper-flux**: a minimal HelmRelease v2 type and the HelmRelease hold
  patches. The hold logic itself is pure and lives in `reaper-core`
  (`hold.rs`).
- **reaper-observability**: the `Telemetry` impl (OpenTelemetry meters over
  OTLP, plus a `prometheus-client` registry), the tracing subscriber with JSON
  logs, and the axum server for `/healthz`, `/readyz` and `/metrics`.
- **reaper-notify**: reads channel URLs from the mounted Secret directory and
  delivers notifications to Discord, Slack, generic webhooks and SMTP from a
  background worker. A slow channel never delays a cycle.

### apps/k8s-reaper

The binary is a thin layer. It has two subcommands: `run` (the default) and
`crd`, which prints the CRD YAML. `src/run.rs` builds the adapters and runs
the loop. It holds no decision logic.

### One cycle

```text
 watch caches --snapshot(now)--> Snapshot
                                    |
            ReaperConfig ------> decide(snapshot, config)
                                    |   detect: crash loops, failing CronJobs,
                                    |   owners, how long
                                    v
                                CyclePlan   (one plan per workload)
                                    |
                     execute(snapshot, plan, mode, ports)
                                    |   mode gate, step order
              +---------------------+----------------------+
              v                     v                      v
     ClusterWriter            EventSink               Telemetry
     annotations, labels,     Kubernetes Events       metrics
     scale, CronJob suspend,
     Flux holds
                                    |
                     outcome.notifications --> throttle --> reaper-notify
```

1. The kube adapter takes a snapshot from its caches, stamped with "now".
2. The binary reads the ReaperConfig and keeps the last valid one active.
3. `decide()` runs detection and returns a plan per workload: observe,
   candidate, reap steps, restore steps, or a skip with a reason. The decision
   is the same in report and full mode.
4. `execute()` runs the plan through the ports. The mode gate is the only
   branch between modes: report mode writes only the reaper's own
   annotations, labels and Events; full mode also scales, suspends CronJobs
   and applies the Flux holds.
5. The transitions the executor returns are throttled and handed to the
   notifier. The binary records cycle metrics and readiness, and writes the
   ReaperConfig status.

A cycle runs every `cycleInterval`, or sooner when a watched change asks for
one (for example, a restore request), but never within 2 s of the last one.

### Decisions

The reasoning behind the design is written down once. Read it there, not here:

- [`specs/001-crashloop-reaper/research.md`](specs/001-crashloop-reaper/research.md):
  the decision log (R1 to R13), with alternatives considered.
- [Owner Decisions](specs/001-crashloop-reaper/spec.md#owner-decisions) in
  `spec.md`: the answers the owner gave to open questions.
- [`plan.md`](specs/001-crashloop-reaper/plan.md): the constitution check and
  complexity tracking.
- [`contracts/`](specs/001-crashloop-reaper/contracts/): the public contracts
  (annotations, labels, config, Events, metrics, notifications).
- [`.specify/memory/constitution.md`](.specify/memory/constitution.md): the
  project rules.

A change to a public contract updates the matching file in `contracts/`, and
the user guide, in the same change. A change that adds a new kind of mutation
adds its reversal and its report-mode equivalent in the same change.

## Testing

There are two tiers: the fast suite, which runs everywhere, and the dev-cluster
stories, which you run on purpose.

### Unit and contract tests

```sh
make test
```

This runs `make lint` first, then `cargo test --workspace --locked`.
`make lint` runs:

- `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets --locked -- -D warnings`.

What the suite covers:

- **Core unit tests.** Table-driven tests in `reaper-core`, built from the
  `testing.rs` builders, with time as a parameter. Write these first when you
  change core behaviour.
- **Report mode is a dry run.** `crates/reaper-core/tests/equivalence.rs`
  checks that report and full mode publish the same plan, and that report
  mode writes nothing but `reaper.boop.ninja/*` metadata (SC-004).
- **Plan JSON snapshots.** `plan.rs` uses `insta` inline snapshots for the
  published plan JSON. If you change the shape on purpose, update the inline
  snapshot and review the diff (`cargo insta review` if you have
  `cargo-insta`).
- **Adapter contract tests.**
  - `reaper-kube/tests/snapshot.rs` maps recorded objects in
    `crates/reaper-kube/tests/fixtures/` to the core's views, with no API
    server. The fixtures were taken from live objects and scrubbed: uids are
    fake (`00000000-0000-4000-8000-…`). Keep it that way when you add one.
  - `writer.rs`, `events.rs` and `config_watch.rs` assert the HTTP requests
    against a `tower-test` mock API server.
  - `crd_golden.rs` compares the CRD YAML to `tests/golden/crd.yaml`. The
    chart ships that YAML, so any change must show up as a diff. Regenerate
    with `UPDATE_GOLDEN=1 cargo test -p reaper-kube --test crd_golden` and
    review it.
  - `reaper-flux/tests/holds.rs` covers the HelmRelease hold patches.
  - `reaper-notify/tests/` covers channel URL parsing and delivery against a
    local server.

CI (`.github/workflows/ci.yml`) runs the same fmt, clippy and test commands on
pushes to `main` and on pull requests, and builds the image without pushing
it.

### Clippy

The workspace turns on `clippy::pedantic` as warnings, and `-D warnings` turns
those into errors. It allows `module_name_repetitions`,
`missing_errors_doc` and `missing_panics_doc`. `unsafe_code` is forbidden.

`doc_markdown` (part of pedantic) wants identifiers in doc comments in
backticks. Kubernetes and Flux kind names such as `HelmRelease`,
`StatefulSet`, `CronJob` and `CrashLoopBackOff` are listed in
`clippy.toml` under `doc-valid-idents`, so prose can name them plainly. Add a
new one there rather than sprinkling backticks.

### Dev-cluster stories

These tests run against a live dev cluster, take minutes and change fixture
state. Each is `#[ignore]`, so `cargo test` skips them.
`make test-integration` runs them with `--ignored --test-threads=1`: one at a
time, because they share the fixtures and the ReaperConfig.

You need a dev cluster with the fixtures and the reaper running (see
[Dev cluster](#dev-cluster)). Then:

```sh
make test-integration STORY=us2_reap KUBE_CONTEXT=kind-<name>
make test-integration KUBE_CONTEXT=kind-<name>      # every story
```

`make test-integration` passes `KUBE_CONTEXT` to the tests as
`REAPER_TEST_CONTEXT`. On k3d the default context works, so `KUBE_CONTEXT` can
be left out.

| Story | File | What it checks |
| ----- | ---- | -------------- |
| US1 | `us1_report.rs` | Report mode publishes plans on the fixtures and changes nothing (SC-001). Reads only. |
| US2 | `us2_reap.rs` | Full mode reaps the plain, kustomize and helm fixtures, and forced Flux reconciles do not undo it (SC-002). Restores them at the end. |
| US3 | `us3_restore.rs` | One restore annotation puts back exactly what the reap changed, Flux holds included (SC-003). |
| US4 | `us4_config.rs` | Config changes apply without a restart: namespace and workload mode labels, the reaper's own namespace is never reaped (SC-005). |
| US5 | `us5_cronjob.rs` | A CronJob whose runs keep failing is suspended, a flaky one is not, and restore resumes it, also under Flux (SC-008). |
| US6 | `us6_notify.rs` | Report-mode candidates are notified once to the echo webhook and Mailpit (SC-009). Needs `make dev-notify` first. Reads only. |

`tests/common/mod.rs` builds the client and runs the guard.
`tests/full/mod.rs` holds the helpers the full-mode stories share: switching
the dev ReaperConfig to full mode and back, and waiting on fixtures. The
stories put the ReaperConfig back when they finish. If one is interrupted,
re-apply `deploy/dev` (`kubectl apply -k deploy/dev`) to reset it.

Run the stories before a release, and after any change to the kube or Flux
adapters.

## Make targets

`make help` lists them:

| Target | What it does |
| ------ | ------------ |
| `help` | List targets. |
| `build` | Build the workspace. Runs `gc` first. |
| `test` | fmt check, clippy, unit and contract tests. |
| `lint` | fmt check and clippy with `-D warnings`. |
| `test-integration` | Dev-cluster integration suite. `STORY=us1_report` runs one story. |
| `image` | Build the container image (`IMAGE`, default `k8s-reaper:dev`). |
| `gc` | Sweep stale `target/` output and cap the BuildKit cache. |
| `dev-cluster` | New k3d cluster with Flux, then `dev-fixtures`. |
| `dev-kind` | Existing kind cluster that already runs Flux: registry, then `dev-fixtures`. |
| `dev-registry` | Registry for the Flux payloads on the kind network (k3d makes its own). |
| `dev-fixtures` | CRD, Flux payloads and the `deploy/dev` fixtures, on a cluster with Flux. |
| `dev-push` | Push the Flux fixture payloads (kustomize dir, crashloop chart) to the dev registry. |
| `dev-down` | Delete the k3d dev cluster and its registry. |
| `dev-clean` | Remove what `dev-kind` added to a shared cluster. Keeps the cluster. |
| `dev-load` | Load the image into the dev cluster. |
| `dev-run` | Run the reaper in-cluster (namespace `k8s-reaper`, report mode). |
| `dev-notify` | Notification catchers (echo, Mailpit), the channel Secret, and `ReapPlanned` notifications. |

Variables you can override:

| Variable | Default | Used by |
| -------- | ------- | ------- |
| `KUBE_CONTEXT` | `k3d-$(CLUSTER)` | every dev target |
| `CLUSTER` | `k8s-reaper-dev` | k3d targets |
| `STORY` | empty (all) | `test-integration` |
| `IMAGE` | `k8s-reaper:dev` | `image`, `dev-load` |
| `FLUX_VERSION` | `v2.7.0` | `dev-cluster` |
| `DEV_REGISTRY` | `localhost:5001` | `dev-push` |
| `GC_DAYS` | `14` | `gc` |
| `GC_DOCKER_KEEP` | `40GB` | `gc` |

### Build hygiene

`make gc` keeps `target/` and the Docker build cache from growing without
bound. `make build` runs it first. It never fails a build:

- With `cargo-sweep` installed, it runs `cargo sweep --time $(GC_DAYS)`.
  Without it, it prints the install command and skips.
- With Docker running, it runs `docker builder prune -f --max-used-space
  $(GC_DOCKER_KEEP)`, which drops the least recently used build cache first.
  Without Docker, it skips with a note.

It never prunes images, volumes or containers. On an old checkout, preview the
first sweep with `cargo sweep --time 14 --dry-run`.

## Release process

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`: the CI checks,
then the shared `mbround18/gh-reusable` Docker release, which pushes
`mbround18/k8s-reaper` to Docker Hub and GHCR. It needs the repository
variable `DOCKER_USERNAME` and the secret `DOCKER_TOKEN`. The chart is
published from its own repository.

- **Image.** The `Dockerfile` is multi-stage: cargo-chef planner, cook and
  builder stages, then `gcr.io/distroless/cc-debian13:nonroot`, running as
  `USER 65532:65532`. The runtime's Debian release matches the builder's, so
  glibc matches. `make image` builds it locally. CI builds it on every push
  and pull request; only a release tag pushes it. The image is `linux/amd64`
  only.
- **Chart.** The Helm chart is not in this repository. It lives in
  `mbround18/helm-charts` as `charts/k8s-reaper`. Its CRD comes from
  `k8s-reaper crd`, so a CRD change here (and its golden file) needs a
  matching chart change.
- **Deployment.** A Flux HelmRelease overlay in `mbround18/fluxified`
  (`overlays/services/k8s-reaper/`) installs the chart. It takes effect once
  the chart change is merged to that repository's `main`.

Before a release:

1. `make test` passes.
2. The dev-cluster stories pass.
3. The image builds.
4. Any contract change is reflected in `specs/*/contracts/`, the guides and
   the chart.

New deployments start in report mode. Switch `spec.mode` to `full` in Git
only after the reports look right.

## Commits

- **Every commit is signed** with GPG or SSH. Fix a signing failure. Never
  fall back to an unsigned commit or `--no-gpg-sign`.
- **Hooks are never skipped.** If a hook fails, fix what it reported. No
  `--no-verify`.
- **Small, atomic commits.** One focused change per commit. Commit as you go.
  Keep unrelated cleanup out of a fix.
- **No force-pushes**, and no session links to AI tools in commits, PRs or
  issues.

### Spec-kit workflow

Features are specified with [spec-kit](https://github.com/github/spec-kit)
under `specs/<NNN-feature>/`:

- `spec.md`: user stories, requirements, success criteria, owner decisions;
- `plan.md`, `research.md`, `data-model.md`, `quickstart.md`;
- `contracts/`: the public contracts;
- `tasks.md`: numbered tasks (`T080`) and gaps (`G18`), grouped by user story.

Tick a task in `tasks.md` when it is done, and name its ID in the commit
subject.

### Subject style

Conventional-commit type and scope, a lower-case summary of what is now true,
and the task, gap or story IDs in parentheses:

```text
feat(core): suspend a CronJob that keeps failing (T080-T082, T084, T086, T087, US5)
feat(kube): watch CronJobs and their Jobs (T083, T088, US5)
fix(kube): a new Event note is a new Event, not a stale series (G18)
test: CronJob suspend and restore on the dev cluster (T089, SC-008)
docs(guides): notifications setup (T099)
```

Types in use: `feat`, `fix`, `test`, `docs`. Scopes are the crate or area:
`core`, `kube`, `flux`, `notify`, `observability`, `app`, `dev`, `image`,
`spec`, `tasks`, `guides`.

## License

k8s-reaper is dual-licensed under [MIT](LICENSE-MIT) or
[Apache 2.0](LICENSE-APACHE), at your option (`MIT OR Apache-2.0`). By
contributing, you agree that your contributions are licensed the same way.
