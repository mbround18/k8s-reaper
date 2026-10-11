# k8s-reaper (binary)

The executable. A thin layer that parses flags and wires the adapters to
`reaper-core`; it holds no decision logic.

- `src/main.rs`: clap flags (`RunArgs`) and the subcommands `run` (default)
  and `crd` (prints the ReaperConfig CRD YAML:
  `cargo run -q -p k8s-reaper -- crd`).
- `src/run.rs`: builds the adapters and runs the cycle loop.
- `tests/`: the dev-cluster stories (`us1_report.rs` to `us6_notify.rs`),
  all `#[ignore]`. Run them with `make test-integration`, never against a
  real cluster.

See [CONTRIBUTING.md, Architecture](../../CONTRIBUTING.md#architecture) and
[Dev-cluster stories](../../CONTRIBUTING.md#dev-cluster-stories).
