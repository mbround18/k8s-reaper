//! The reference-counted hold on a HelmRelease (research.md R5, R6;
//! contracts/annotations.md "On HelmReleases" and "Restore semantics").
//!
//! Pure: given what the HelmRelease looks like now, compute the one merge
//! patch that places or releases the hold for one workload.

use std::collections::BTreeMap;

use crate::model::{HelmReleaseView, WorkloadRef};
use crate::ports::HelmReleasePatch;
use crate::record::{
    FLUX_RECONCILE, FLUX_RECONCILE_DISABLED, HelmReleaseHold, MetadataPatch, PreviousReconcile,
    is_owned_key,
};

/// One patch on a HelmRelease and what it means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HoldChange {
    pub patch: HelmReleasePatch,
    /// The hold after the patch.
    pub hold: HelmReleaseHold,
    /// `held-for` went from empty to non-empty (`HelmReleaseHeld`).
    pub held: bool,
    /// `held-for` became empty and the hold was cleaned up
    /// (`HelmReleaseReleased`).
    pub released: bool,
}

impl HoldChange {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patch.suspend.is_none() && self.patch.annotations.is_empty()
    }
}

/// Adds `workload` to the hold. Suspends the HelmRelease only if it is not
/// already suspended, and disables kustomize reconcile on a kustomize-managed
/// HelmRelease, recording the previous value once.
#[must_use]
pub fn place(hr: &HelmReleaseView, workload: &WorkloadRef) -> HoldChange {
    let (mut hold, _) = HelmReleaseHold::decode(&hr.reaper_annotations);
    let first = hold.held_for.is_empty();
    hold.held_for.insert(workload.clone());

    let mut suspend = None;
    if hold.suspended_by_reaper.is_none() {
        hold.suspended_by_reaper = Some(!hr.suspended);
        if !hr.suspended {
            suspend = Some(true);
        }
    }

    let mut reconcile = None;
    if hr.kustomization.is_some()
        && hr.reconcile_annotation.as_deref() != Some(FLUX_RECONCILE_DISABLED)
    {
        if hold.previous_reconcile.is_none() {
            hold.previous_reconcile = Some(PreviousReconcile::from_current(
                hr.reconcile_annotation.as_deref(),
            ));
        }
        reconcile = Some(Some(FLUX_RECONCILE_DISABLED.to_owned()));
    }

    let mut annotations =
        MetadataPatch::between(&hr.reaper_annotations, None, &hold.encode(), None).annotations;
    if let Some(v) = reconcile {
        annotations.insert(FLUX_RECONCILE.to_owned(), v);
    }
    HoldChange {
        patch: HelmReleasePatch {
            suspend,
            annotations,
        },
        hold,
        held: first,
        released: false,
    }
}

/// Removes `workload` from the hold. When the set becomes empty, unsuspends
/// only if the reaper suspended it, puts the reconcile annotation back, and
/// deletes every `reaper.boop.ninja/*` key. An empty result means there was
/// nothing to release.
#[must_use]
pub fn release(hr: &HelmReleaseView, workload: &WorkloadRef) -> HoldChange {
    let (mut hold, _) = HelmReleaseHold::decode(&hr.reaper_annotations);
    hold.held_for.remove(workload);
    if !hold.held_for.is_empty() {
        let annotations =
            MetadataPatch::between(&hr.reaper_annotations, None, &hold.encode(), None).annotations;
        return HoldChange {
            patch: HelmReleasePatch {
                suspend: None,
                annotations,
            },
            hold,
            held: false,
            released: false,
        };
    }

    let suspend = (hold.suspended_by_reaper == Some(true) && hr.suspended).then_some(false);
    let mut annotations: BTreeMap<String, Option<String>> = hr
        .reaper_annotations
        .keys()
        .filter(|k| is_owned_key(k))
        .map(|k| (k.clone(), None))
        .collect();
    if let Some(previous) = &hold.previous_reconcile {
        let restored = previous.restored_value();
        if hr.reconcile_annotation != restored {
            annotations.insert(FLUX_RECONCILE.to_owned(), restored);
        }
    }
    let released = !annotations.is_empty() || suspend.is_some();
    HoldChange {
        patch: HelmReleasePatch {
            suspend,
            annotations,
        },
        hold: HelmReleaseHold::default(),
        held: false,
        released,
    }
}

/// The view after `patch` was written and the server returned
/// `resource_version`.
pub fn apply(hr: &mut HelmReleaseView, patch: &HelmReleasePatch, resource_version: String) {
    hr.resource_version = resource_version;
    if let Some(s) = patch.suspend {
        hr.suspended = s;
    }
    for (k, v) in &patch.annotations {
        if k == FLUX_RECONCILE {
            hr.reconcile_annotation.clone_from(v);
        } else if let Some(v) = v {
            hr.reaper_annotations.insert(k.clone(), v.clone());
        } else {
            hr.reaper_annotations.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NsName, WorkloadKind};
    use crate::record::{HELD_FOR, PREVIOUS_RECONCILE, SUSPENDED_BY_REAPER};
    use crate::testing::helm_release;

    fn web() -> WorkloadRef {
        WorkloadRef::new(WorkloadKind::Deployment, "helm", "web")
    }

    fn db() -> WorkloadRef {
        WorkloadRef::new(WorkloadKind::StatefulSet, "helm", "db")
    }

    #[allow(clippy::unnecessary_wraps)]
    fn set(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn first_hold_suspends_and_records_it() {
        let hr = helm_release("helm", "app");
        let c = place(&hr, &web());
        assert!(c.held);
        assert_eq!(c.patch.suspend, Some(true));
        assert_eq!(
            c.patch.annotations,
            BTreeMap::from([
                (HELD_FOR.to_owned(), set(r#"["Deployment/helm/web"]"#)),
                (SUSPENDED_BY_REAPER.to_owned(), set("true")),
            ])
        );
    }

    #[test]
    fn already_suspended_is_left_alone() {
        let mut hr = helm_release("helm", "app");
        hr.suspended = true;
        let c = place(&hr, &web());
        assert_eq!(c.patch.suspend, None);
        assert_eq!(c.patch.annotations[SUSPENDED_BY_REAPER], set("false"));
    }

    #[test]
    fn kustomize_managed_release_gets_reconcile_disabled() {
        let mut hr = helm_release("helm", "app");
        hr.kustomization = Some(NsName::new("flux-system", "apps"));
        hr.reconcile_annotation = Some("enabled".into());
        let c = place(&hr, &web());
        assert_eq!(c.patch.annotations[FLUX_RECONCILE], set("disabled"));
        assert_eq!(c.patch.annotations[PREVIOUS_RECONCILE], set("enabled"));

        hr.reconcile_annotation = None;
        let c = place(&hr, &web());
        assert_eq!(c.patch.annotations[PREVIOUS_RECONCILE], set("<none>"));
    }

    #[test]
    fn second_workload_only_extends_the_set() {
        let mut hr = helm_release("helm", "app");
        let first = place(&hr, &web());
        apply(&mut hr, &first.patch, "2".into());
        assert!(hr.suspended);
        let c = place(&hr, &db());
        assert!(!c.held);
        assert_eq!(c.patch.suspend, None);
        assert_eq!(
            c.patch.annotations,
            BTreeMap::from([(
                HELD_FOR.to_owned(),
                set(r#"["Deployment/helm/web","StatefulSet/helm/db"]"#)
            )])
        );
        // Placing the same hold again writes nothing.
        apply(&mut hr, &c.patch, "3".into());
        assert!(place(&hr, &db()).is_empty());
    }

    #[test]
    fn release_keeps_the_hold_while_others_remain() {
        let mut hr = helm_release("helm", "app");
        for w in [web(), db()] {
            let c = place(&hr, &w);
            apply(&mut hr, &c.patch, "2".into());
        }
        let c = release(&hr, &web());
        assert!(!c.released);
        assert_eq!(c.patch.suspend, None);
        assert_eq!(
            c.patch.annotations,
            BTreeMap::from([(HELD_FOR.to_owned(), set(r#"["StatefulSet/helm/db"]"#))])
        );
    }

    #[test]
    fn last_release_unsuspends_and_cleans_up() {
        let mut hr = helm_release("helm", "app");
        hr.kustomization = Some(NsName::new("flux-system", "apps"));
        let c = place(&hr, &web());
        apply(&mut hr, &c.patch, "2".into());
        let c = release(&hr, &web());
        assert!(c.released);
        assert_eq!(c.patch.suspend, Some(false));
        assert_eq!(
            c.patch.annotations,
            BTreeMap::from([
                (FLUX_RECONCILE.to_owned(), None),
                (HELD_FOR.to_owned(), None),
                (PREVIOUS_RECONCILE.to_owned(), None),
                (SUSPENDED_BY_REAPER.to_owned(), None),
            ])
        );
        apply(&mut hr, &c.patch, "3".into());
        assert!(!hr.suspended);
        assert!(hr.reaper_annotations.is_empty());
        assert!(release(&hr, &web()).is_empty());
    }

    #[test]
    fn release_does_not_unsuspend_what_a_person_suspended() {
        let mut hr = helm_release("helm", "app");
        hr.suspended = true;
        let c = place(&hr, &web());
        apply(&mut hr, &c.patch, "2".into());
        let c = release(&hr, &web());
        assert!(c.released);
        assert_eq!(c.patch.suspend, None);
    }
}
