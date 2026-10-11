//! The minimal Flux `HelmRelease` (`helm.toolkit.fluxcd.io/v2`): metadata and
//! `spec.suspend` only. Unknown fields are ignored and never round-tripped.

use kube::CustomResource;
use kube::core::{ApiResource, GroupVersionKind};
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "helm.toolkit.fluxcd.io";
pub const VERSION: &str = "v2";
pub const KIND: &str = "HelmRelease";

#[derive(CustomResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[kube(
    group = "helm.toolkit.fluxcd.io",
    version = "v2",
    kind = "HelmRelease",
    namespaced,
    schema = "disabled"
)]
pub struct HelmReleaseSpec {
    #[serde(default)]
    pub suspend: bool,
}

#[must_use]
pub fn gvk() -> GroupVersionKind {
    GroupVersionKind::gvk(GROUP, VERSION, KIND)
}

#[must_use]
pub fn api_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(&gvk(), "helmreleases")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::Resource;

    #[test]
    fn reads_suspend_and_ignores_the_rest() {
        let hr: HelmRelease = serde_json::from_value(serde_json::json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": "web", "namespace": "apps"},
            "spec": {"suspend": true, "interval": "5m", "values": {"replicaCount": 2}}
        }))
        .unwrap();
        assert!(hr.spec.suspend);
        assert_eq!(api_resource().plural, HelmRelease::plural(&()));
        assert_eq!(api_resource().api_version, HelmRelease::api_version(&()));
    }
}
