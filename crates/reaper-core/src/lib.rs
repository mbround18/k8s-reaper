//! Pure core of k8s-reaper.
//!
//! Everything here is plain data in, plain data out. There is no Kubernetes
//! client, no async runtime and no clock: "now" is always a parameter.
