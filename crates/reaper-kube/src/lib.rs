//! Kubernetes adapter: snapshot building, writes, Events and the ReaperConfig CRD.
//!
//! The kube-rs side of the hexagon. It turns Kubernetes objects into the
//! core's plain-data views and the core's writes into API calls:
//!
//! - [`snapshot::Caches`]: watch-backed caches; [`snapshot::Caches::snapshot`]
//!   builds a `reaper_core::model::Snapshot`, and
//!   [`snapshot::Caches::triggered`] wakes the loop early on a watched change.
//! - [`writer::KubeWriter`]: the `ClusterWriter` port, merge patches with a
//!   `resourceVersion` precondition.
//! - [`events::KubeEventSink`]: the `EventSink` port over `events.k8s.io/v1`.
//! - [`crd`]: the `ReaperConfig` CRD type and [`crd::crd_yaml`], which
//!   `k8s-reaper crd` prints and the chart ships.
//! - [`config_watch::ConfigSource`]: reads the ReaperConfig once per cycle and
//!   writes its status.

pub mod config_watch;
pub mod crd;
pub mod events;
pub mod snapshot;
pub mod writer;
