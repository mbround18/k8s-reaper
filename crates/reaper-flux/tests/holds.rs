//! HelmRelease hold and release contract (T042, T051): the merge patch body
//! the reaper sends for each change (contracts/annotations.md "On
//! HelmReleases").

use reaper_core::hold;
use reaper_core::model::{NsName, WorkloadKind, WorkloadRef};
use reaper_core::testing::helm_release;
use reaper_flux::holds::merge_body;
use serde_json::json;

fn web() -> WorkloadRef {
    WorkloadRef::new(WorkloadKind::Deployment, "shop", "web")
}

fn db() -> WorkloadRef {
    WorkloadRef::new(WorkloadKind::StatefulSet, "shop", "db")
}

#[test]
fn first_hold_suspends_and_records_it() {
    let hr = helm_release("shop", "app");
    let c = hold::place(&hr, &web());
    assert_eq!(
        merge_body("7", &c.patch),
        json!({
            "metadata": {
                "resourceVersion": "7",
                "annotations": {
                    "reaper.boop.ninja/held-for": "[\"Deployment/shop/web\"]",
                    "reaper.boop.ninja/suspended-by-reaper": "true",
                },
            },
            "spec": {"suspend": true},
        })
    );
}

#[test]
fn an_already_suspended_release_is_not_suspended_again() {
    let mut hr = helm_release("shop", "app");
    hr.suspended = true;
    let c = hold::place(&hr, &web());
    let body = merge_body("7", &c.patch);
    assert_eq!(body.get("spec"), None);
    assert_eq!(
        body["metadata"]["annotations"]["reaper.boop.ninja/suspended-by-reaper"],
        "false"
    );
}

#[test]
fn held_for_is_sorted_and_deduplicated() {
    let mut hr = helm_release("shop", "app");
    for (rv, w) in [("2", web()), ("3", db()), ("4", web())] {
        let c = hold::place(&hr, &w);
        hold::apply(&mut hr, &c.patch, rv.into());
    }
    assert_eq!(
        hr.reaper_annotations["reaper.boop.ninja/held-for"],
        r#"["Deployment/shop/web","StatefulSet/shop/db"]"#
    );
    // Placing a hold that is already there writes nothing.
    assert!(hold::place(&hr, &db()).is_empty());
}

#[test]
fn a_kustomize_managed_release_gets_reconcile_disabled() {
    let mut hr = helm_release("shop", "app");
    hr.kustomization = Some(NsName::new("flux-system", "apps"));
    let c = hold::place(&hr, &web());
    let annotations = &merge_body("7", &c.patch)["metadata"]["annotations"];
    assert_eq!(
        annotations["kustomize.toolkit.fluxcd.io/reconcile"],
        "disabled"
    );
    assert_eq!(
        annotations["reaper.boop.ninja/previous-reconcile"],
        "<none>"
    );
}

#[test]
fn release_keeps_the_hold_while_another_workload_holds_it() {
    let mut hr = helm_release("shop", "app");
    for w in [web(), db()] {
        let c = hold::place(&hr, &w);
        hold::apply(&mut hr, &c.patch, "2".into());
    }
    let c = hold::release(&hr, &web());
    assert_eq!(
        merge_body("2", &c.patch),
        json!({
            "metadata": {
                "resourceVersion": "2",
                "annotations": {"reaper.boop.ninja/held-for": "[\"StatefulSet/shop/db\"]"},
            },
        })
    );
}

#[test]
fn last_release_unsuspends_restores_reconcile_and_cleans_up() {
    let mut hr = helm_release("shop", "app");
    hr.kustomization = Some(NsName::new("flux-system", "apps"));
    hr.reconcile_annotation = Some("enabled".into());
    let c = hold::place(&hr, &web());
    hold::apply(&mut hr, &c.patch, "2".into());
    let c = hold::release(&hr, &web());
    assert_eq!(
        merge_body("2", &c.patch),
        json!({
            "metadata": {
                "resourceVersion": "2",
                "annotations": {
                    "kustomize.toolkit.fluxcd.io/reconcile": "enabled",
                    "reaper.boop.ninja/held-for": null,
                    "reaper.boop.ninja/previous-reconcile": null,
                    "reaper.boop.ninja/suspended-by-reaper": null,
                },
            },
            "spec": {"suspend": false},
        })
    );
}

#[test]
fn release_never_unsuspends_what_a_person_suspended() {
    let mut hr = helm_release("shop", "app");
    hr.suspended = true;
    let c = hold::place(&hr, &web());
    hold::apply(&mut hr, &c.patch, "2".into());
    let body = merge_body("2", &hold::release(&hr, &web()).patch);
    assert_eq!(body.get("spec"), None);
    assert_eq!(
        body["metadata"]["annotations"]["reaper.boop.ninja/suspended-by-reaper"],
        serde_json::Value::Null
    );
}
