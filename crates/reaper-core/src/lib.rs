//! Pure core of k8s-reaper.
//!
//! Everything here is plain data in, plain data out. There is no Kubernetes
//! client, no async runtime and no clock: "now" is always a parameter.
//!
//! This is the centre of the hexagon. Adapters build a [`model::Snapshot`]
//! from the cluster; [`decide::decide`] turns it and a [`config::Config`] into
//! a [`plan::CyclePlan`] (one plan per workload, identical in report and full
//! mode); [`executor::execute`] carries that plan out through the ports in
//! [`ports`] ([`ports::ClusterWriter`], [`ports::EventSink`],
//! [`ports::Telemetry`]) and returns the transitions that
//! [`notify::Throttle`] filters for the [`notify::Notifier`] port. All state
//! lives in `reaper.boop.ninja/*` annotations, encoded and decoded by
//! [`record`].
//!
//! The only dependencies are `serde`, `serde_json` and `thiserror`;
//! `tests/no_infra_deps.rs` keeps it that way. [`testing`] has fixture
//! builders and in-memory fakes of every port.

pub mod config;
pub mod config_state;
pub mod decide;
pub mod detect;
pub mod executor;
pub mod glob;
pub mod hold;
pub mod model;
pub mod notify;
pub mod plan;
pub mod ports;
pub mod record;
pub mod testing;
pub mod time;

pub use time::Timestamp;
