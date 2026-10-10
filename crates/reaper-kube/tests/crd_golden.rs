//! The CRD YAML is a published contract (the chart ships it). Any change to it
//! must show up as a diff to `tests/golden/crd.yaml`.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p reaper-kube --test crd_golden`.

use std::path::Path;

#[test]
fn crd_yaml_matches_golden_file() {
    let actual = reaper_kube::crd::crd_yaml();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/crd.yaml");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).unwrap();
    }
    let expected =
        std::fs::read_to_string(&path).expect("golden file missing; run with UPDATE_GOLDEN=1");
    assert!(
        actual == expected,
        "CRD YAML differs from {}; rerun with UPDATE_GOLDEN=1 and review the diff",
        path.display()
    );
}

#[test]
fn crd_identity_matches_contract() {
    let crd: serde_json::Value = serde_yaml_ng::from_str(&reaper_kube::crd::crd_yaml()).unwrap();
    assert_eq!(crd["metadata"]["name"], "reaperconfigs.reaper.boop.ninja");
    assert_eq!(crd["spec"]["scope"], "Cluster");
    assert_eq!(crd["spec"]["names"]["shortNames"][0], "rc");
    let version = &crd["spec"]["versions"][0];
    assert_eq!(version["name"], "v1alpha1");
    assert!(version["subresources"]["status"].is_object());
    let columns: Vec<&str> = version["additionalPrinterColumns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["Mode", "Valid", "Candidates", "Reaped", "Age"]);
    let spec = &version["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"];
    assert_eq!(spec["mode"]["enum"], serde_json::json!(["report", "full"]));
    let bound =
        |block: &str, field: &str, key: &str| spec[block]["properties"][field][key].as_f64();
    assert_eq!(bound("detection", "minRestarts", "minimum"), Some(1.0));
    assert_eq!(bound("detection", "minRestarts", "maximum"), Some(10000.0));
    assert_eq!(bound("cronJobs", "minFailedRuns", "maximum"), Some(1000.0));
    assert!(spec["cronJobs"]["properties"]["failingDuration"]["pattern"].is_string());
}
