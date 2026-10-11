//! Pure core of k8s-reaper.
//!
//! Everything here is plain data in, plain data out. There is no Kubernetes
//! client, no async runtime and no clock: "now" is always a parameter.

pub mod config;
pub mod decide;
pub mod detect;
pub mod executor;
pub mod glob;
pub mod model;
pub mod notify;
pub mod plan;
pub mod ports;
pub mod record;
pub mod testing;
pub mod time;

pub use time::Timestamp;
