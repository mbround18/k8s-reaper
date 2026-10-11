//! Health and metrics endpoints (contracts/config.md, HTTP).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prometheus_client::registry::Registry;

const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Shared liveness and readiness flags, set by the cycle loop.
///
/// A new `Health` is alive and not ready; clones share the same flags.
#[derive(Debug, Clone)]
pub struct Health {
    alive: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
}

impl Health {
    #[must_use]
    pub fn new() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::Relaxed);
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Relaxed);
    }
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct AppState {
    health: Health,
    registry: Arc<Registry>,
}

/// `/healthz`, `/readyz` and `/metrics`; anything else is 404.
pub fn router(health: Health, registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(AppState { health, registry })
}

/// Serves `router` on `addr` until `shutdown` resolves.
///
/// # Errors
///
/// Binding the listener or serving fails.
pub async fn serve(
    addr: SocketAddr,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

fn flag(up: &AtomicBool) -> StatusCode {
    if up.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn healthz(State(state): State<AppState>) -> StatusCode {
    flag(&state.health.alive)
}

async fn readyz(State(state): State<AppState>) -> StatusCode {
    flag(&state.health.ready)
}

async fn metrics(State(state): State<AppState>) -> Response {
    let mut body = String::new();
    match prometheus_client::encoding::text::encode(&mut body, &state.registry) {
        Ok(()) => ([(header::CONTENT_TYPE, OPENMETRICS)], body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use prometheus_client::metrics::counter::Counter;
    use prometheus_client::registry::Registry;
    use tower::ServiceExt;

    use super::*;

    async fn get(app: Router, path: &str) -> (StatusCode, String, String) {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    fn registry() -> Arc<Registry> {
        let mut registry = Registry::default();
        let counter = Counter::<u64>::default();
        counter.inc();
        registry.register("reaper_test", "A test counter", counter);
        Arc::new(registry)
    }

    #[tokio::test]
    async fn healthz_follows_liveness() {
        let health = Health::new();
        assert_eq!(
            get(router(health.clone(), registry()), "/healthz").await.0,
            StatusCode::OK
        );
        health.set_alive(false);
        assert_eq!(
            get(router(health, registry()), "/healthz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn readyz_starts_unready_and_follows_the_flag() {
        let health = Health::new();
        assert_eq!(
            get(router(health.clone(), registry()), "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        health.set_ready(true);
        assert_eq!(
            get(router(health.clone(), registry()), "/readyz").await.0,
            StatusCode::OK
        );
        health.set_ready(false);
        assert_eq!(
            get(router(health, registry()), "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn metrics_serves_the_registry_as_openmetrics() {
        let (status, content_type, body) = get(router(Health::new(), registry()), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            content_type.starts_with("application/openmetrics-text"),
            "{content_type}"
        );
        assert!(body.contains("reaper_test_total 1"), "{body}");
        assert!(body.ends_with("# EOF\n"), "{body}");
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        assert_eq!(
            get(router(Health::new(), registry()), "/nope").await.0,
            StatusCode::NOT_FOUND
        );
    }
}
