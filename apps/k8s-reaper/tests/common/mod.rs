//! Shared by the `#[ignore]` dev-cluster tests.

use kube::config::KubeConfigOptions;
use kube::{Client, Config};

/// A client for the kube context named by `REAPER_TEST_CONTEXT`
/// (`make test-integration` sets it from `KUBE_CONTEXT`). Never the current
/// context: these tests must not reach a real cluster by accident, so the
/// context has to be a local kind or k3d one and its API server on loopback.
pub async fn dev_client() -> Client {
    let context = std::env::var("REAPER_TEST_CONTEXT")
        .expect("REAPER_TEST_CONTEXT names the dev cluster's kube context");
    assert!(
        context.starts_with("kind-") || context.starts_with("k3d-"),
        "{context} is not a kind or k3d context"
    );
    let options = KubeConfigOptions {
        context: Some(context.clone()),
        ..KubeConfigOptions::default()
    };
    let config = Config::from_kubeconfig(&options)
        .await
        .expect("kubeconfig for the dev cluster");
    let host = config.cluster_url.host().unwrap_or_default();
    assert!(
        matches!(host, "127.0.0.1" | "localhost" | "0.0.0.0" | "[::1]"),
        "{context} points at {host}, not a local cluster"
    );
    Client::try_from(config).expect("client for the dev cluster")
}
