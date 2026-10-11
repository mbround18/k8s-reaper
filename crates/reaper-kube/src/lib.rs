//! Kubernetes adapter: snapshot building, writes, Events and the ReaperConfig CRD.

pub mod config_watch;
pub mod crd;
pub mod events;
pub mod snapshot;
pub mod writer;
