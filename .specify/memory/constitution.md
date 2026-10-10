# k8s-reaper Constitution

## Core Principles

### I. Agnostic Core, Thin Adapters (Hexagonal)

- All detection and decision logic MUST live in one core library crate that has no dependency on
  `kube`, `k8s-openapi`, an async runtime, the network or the clock. Inputs are plain data
  (observed workloads, pods, config, "now"); outputs are plain data (a plan of actions and the
  reasons for them).
- Everything that touches the outside world is an adapter crate behind a trait (port) defined by
  the core: the Kubernetes adapter (kube-rs), the Flux adapter, telemetry, the clock.
- Binary crates in `apps/*` only parse configuration, build adapters and wire them to the core.
  A binary MUST NOT contain decision logic.
- Rationale: the reaper's value is in deciding correctly when to stop a workload. That decision
  must be testable exhaustively without a cluster, and replaceable transports must not touch it.

### II. Safe by Default (NON-NEGOTIABLE)

- The default mode is `report`. Nothing that scales, suspends or disables reconciliation runs
  unless an operator has explicitly switched the mode to `full` at runtime.
- Report mode MUST compute the exact plan full mode would execute and publish it; it differs only
  in not executing it (dry-run equivalence). One code path decides; a mode flag gates execution.
- The reaper MUST never act on its own namespace or on an excluded namespace, MUST rate-limit
  actions, and MUST cap how many workloads it holds stopped at once.
- Every mutation MUST be recorded on the mutated object with enough detail to reverse it, and
  MUST be reversible by one documented action.
- When in doubt (unknown owner, partially available workload, ambiguous state) the reaper reports
  and does not act.

### III. Test-First for Core Logic (NON-NEGOTIABLE)

- Core logic is developed TDD: a failing test exists before the code that makes it pass.
- Tests MUST be deterministic: no wall clock, no randomness, no network, no sleeping. Time is an
  injected value.
- Adapters are covered by contract tests against recorded/fixture API objects; behaviour against
  a real API server is covered by an opt-in integration suite (kind/k3d), segmented so it runs
  separately from unit tests.

### IV. Observable by Default

- Instrumentation uses OpenTelemetry (traces and metrics). Metrics are also exposed for
  Prometheus scraping, because the target cluster alerts through Prometheus → Alertmanager.
- Every decision the reaper makes about a workload is visible as a Kubernetes Event on that
  workload and as a metric; logs are structured.
- Metric names, label sets, Event reasons and annotation keys are public contracts: changing
  them is a breaking change.

### V. Runtime Configuration and Feature Flags

- Behaviour (mode, thresholds, scope, Flux integration toggles, limits) is runtime configuration,
  changeable without rebuilding or redeploying the image.
- Compile-time flags (`cfg(debug_assertions)`, Cargo features) are allowed only for local
  development aids, never to switch production behaviour.

### VI. Simplicity

- Prefer the smallest design that meets the spec. No new service, CRD, crate or dependency
  without a stated need in the plan.
- Don't fight other controllers: if something else owns a field, integrate (Flux) or decline to
  act (other operators), never race.

## Technology and Repository Constraints

- **Language**: Rust, latest stable toolchain and latest edition, pinned in `rust-toolchain.toml`.
- **Workspace**: one Cargo workspace. `apps/*` holds binary crates only (thin entry points);
  `crates/*` holds library crates (core, adapters, shared support).
- **Kubernetes**: `kube` (kube-rs) + `k8s-openapi`. Axum + Tower only for the HTTP surface
  (health, readiness, metrics).
- **Build hygiene**: the `Makefile` has a `gc` target that runs `cargo sweep --time $(GC_DAYS)`
  (default 14) and `docker builder prune -f --max-used-space $(GC_DOCKER_KEEP)` (default 40GB).
  The main build target depends on `gc`. `gc` never fails a build: if `cargo-sweep` is missing it
  prints `cargo install cargo-sweep --locked` and continues; if Docker is not running it skips with
  a note. Images, volumes and containers are never pruned automatically.
- **Containers**: multi-stage Dockerfile using cargo-chef (planner → cook → builder) and a
  minimal, non-root runtime image.
- **Delivery**: shipped as a Helm chart in `mbround18/helm-charts` (`charts/k8s-reaper`), consumed
  by a Flux HelmRelease in `mbround18/fluxified`.
- **License**: dual-licensed `MIT OR Apache-2.0`.
- **Docs**: `README.md` short and user-facing; `CONTRIBUTING.md` detailed (setup, architecture,
  decisions); anything a user has to learn goes in `docs/guides/`.

## Development Workflow and Quality Gates

- Commit as you go: small, atomic commits. Every commit is GPG/SSH-signed; signing failures are
  fixed, never bypassed. Hooks are never skipped.
- No Claude session links in commits, PRs or issues.
- Before merge: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
  (unit + contract) pass. Integration tests run before any release.
- Any change to a public contract (annotations, labels, metrics, Events, CRD schema) updates the
  matching document under `specs/*/contracts/` and the user guide in the same change.
- A change that adds a new kind of mutation must add its reversal and its report-mode
  equivalent in the same change.

## Governance

- This constitution supersedes other practice in this repository. Plans MUST include a
  Constitution Check; violations must be justified in the plan's Complexity Tracking table or the
  design changed.
- Amendments are made by pull request that edits this file, states the version bump and why, and
  updates dependent templates or docs if they reference the changed rule.
- Versioning: MAJOR for removing or redefining a principle, MINOR for a new principle or
  materially expanded guidance, PATCH for wording.
- Runtime guidance for agents lives in `CLAUDE.md` once the code exists.

**Version**: 1.0.0 | **Ratified**: 2026-10-10 | **Last Amended**: 2026-10-10
