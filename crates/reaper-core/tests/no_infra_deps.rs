//! Constitution I: the core must not depend on a Kubernetes client, an async
//! runtime or a clock. This test reads `cargo metadata` and fails if any of
//! them appear in `reaper-core`'s normal dependency tree.

use std::collections::{BTreeSet, HashMap};
use std::process::Command;

const FORBIDDEN: &[&str] = &[
    "kube",
    "kube-client",
    "kube-runtime",
    "k8s-openapi",
    "tokio",
    "chrono",
    "jiff",
    "time",
];

#[test]
fn reaper_core_has_no_infrastructure_dependencies() {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("metadata json");

    let names: HashMap<&str, &str> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let nodes: HashMap<&str, &serde_json::Value> = meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();
    let root = names
        .iter()
        .find(|(_, n)| **n == "reaper-core")
        .map(|(id, _)| *id)
        .unwrap();

    // Walk normal (non-dev, non-build) dependencies only.
    let mut seen = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        for dep in nodes[id]["deps"].as_array().unwrap() {
            let normal = dep["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["kind"].is_null());
            let dep_id = dep["pkg"].as_str().unwrap();
            if normal && seen.insert(names[dep_id]) {
                stack.push(dep_id);
            }
        }
    }
    let bad: Vec<_> = seen.iter().filter(|n| FORBIDDEN.contains(n)).collect();
    assert!(bad.is_empty(), "reaper-core depends on {bad:?}");
}
