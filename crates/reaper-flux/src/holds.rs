//! The HelmRelease hold and release on the wire: one JSON merge patch per
//! change, guarded by a `resourceVersion` precondition. What goes into the
//! patch is decided by `reaper_core::hold`; this is only its shape.

use reaper_core::ports::HelmReleasePatch;
use serde_json::{Map, Value};

/// The merge patch body for `patch`: `metadata.resourceVersion`, the
/// annotations (`null` deletes one) and `spec.suspend` only when it changes.
#[must_use]
pub fn merge_body(resource_version: &str, patch: &HelmReleasePatch) -> Value {
    let mut metadata = Map::new();
    metadata.insert("resourceVersion".into(), resource_version.into());
    if !patch.annotations.is_empty() {
        metadata.insert(
            "annotations".into(),
            Value::Object(
                patch
                    .annotations
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().map_or(Value::Null, Value::String)))
                    .collect(),
            ),
        );
    }
    let mut body = Map::new();
    body.insert("metadata".into(), Value::Object(metadata));
    if let Some(suspend) = patch.suspend {
        let mut spec = Map::new();
        spec.insert("suspend".into(), suspend.into());
        body.insert("spec".into(), Value::Object(spec));
    }
    Value::Object(body)
}
