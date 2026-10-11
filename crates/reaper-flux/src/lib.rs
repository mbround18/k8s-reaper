//! Flux adapter: the minimal HelmRelease type and the hold/release patches.
//!
//! An adapter on the edge of the hexagon, used by `reaper-kube`.
//! [`helmrelease::HelmRelease`] is a minimal `helm.toolkit.fluxcd.io/v2`
//! type (metadata and `spec.suspend` only) that the kube adapter watches;
//! [`helmrelease::api_resource`] names it for dynamic API calls.
//! [`holds::merge_body`] shapes the JSON merge patch that places or releases
//! a hold, with its `resourceVersion` precondition.
//!
//! What goes into a hold is decided by `reaper_core::hold`, which is pure;
//! this crate only puts it on the wire.

pub mod helmrelease;
pub mod holds;
