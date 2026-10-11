//! Health and metrics endpoints (contracts/config.md, HTTP).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prometheus_client::registry::Registry;
pub use reaper_core::ports::Readiness;

const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// A cycle that finished this many intervals ago makes the reaper unready.
pub const STALE_CYCLES: u32 = 3;

/// No cycle started for this many intervals makes the reaper not live.
pub const DEAD_CYCLES: u32 = 10;

/// The shortest liveness window, whatever the interval.
pub const MIN_LIVENESS_WINDOW: Duration = Duration::from_mins(10);

/// The interval assumed until the loop passes one: the `cycleInterval`
/// default.
const DEFAULT_INTERVAL: Duration = Duration::from_mins(1);

/// Liveness and readiness, shared between the cycle loop, the status task
/// and the HTTP server (data-model.md, "Health"). Clones share the same state.
///
/// With `I` the interval passed with the last [`cycle_started`] or
/// [`cycle_finished`]:
///
/// - live: a cycle started (or the process started) less than
///   `max(10·I, 10m)` ago;
/// - ready: caches synced, a valid config applied at least once, and the last
///   cycle finished less than `3·I` ago.
///
/// Since `10·I > 3·I`, readiness always fails first. Every query takes `now`,
/// so tests pass their own `Instant`s.
///
/// [`cycle_started`]: Health::cycle_started
/// [`cycle_finished`]: Health::cycle_finished
#[derive(Debug, Clone)]
pub struct Health {
    inner: Arc<Inner>,
}

/// Instants are stored as nanoseconds since `base`, so they fit in atomics.
#[derive(Debug)]
struct Inner {
    base: Instant,
    cycle_started: AtomicU64,
    /// One more than the offset, so `0` means no cycle has finished.
    cycle_finished: AtomicU64,
    interval: AtomicU64,
    /// Latches: nothing clears it (owner decision Q1).
    config_applied: AtomicBool,
    caches_synced: AtomicBool,
}

impl Health {
    /// A `Health` for a process that starts now.
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    /// A `Health` for a process that started at `start`: live until the
    /// liveness window passes without a cycle start, not ready.
    #[must_use]
    pub fn starting_at(start: Instant) -> Self {
        Self {
            inner: Arc::new(Inner {
                base: start,
                cycle_started: AtomicU64::new(0),
                cycle_finished: AtomicU64::new(0),
                interval: AtomicU64::new(nanos(DEFAULT_INTERVAL)),
                config_applied: AtomicBool::new(false),
                caches_synced: AtomicBool::new(false),
            }),
        }
    }

    /// A cycle started at `now`, with the `interval` in force.
    pub fn cycle_started(&self, now: Instant, interval: Duration) {
        self.inner
            .interval
            .store(nanos(interval), Ordering::Relaxed);
        self.inner
            .cycle_started
            .store(self.offset(now), Ordering::Relaxed);
    }

    /// A cycle finished at `now`; the next one is due within `interval`.
    pub fn cycle_finished(&self, now: Instant, interval: Duration) {
        self.inner
            .interval
            .store(nanos(interval), Ordering::Relaxed);
        self.inner
            .cycle_finished
            .store(self.offset(now).saturating_add(1), Ordering::Relaxed);
    }

    /// A valid config, or the built-in defaults with no `ReaperConfig`, is in
    /// force. There is no way back: a later invalid edit keeps the last valid
    /// config, and the reaper stays ready.
    pub fn config_applied(&self) {
        self.inner.config_applied.store(true, Ordering::Relaxed);
    }

    /// The watch caches finished their initial sync.
    pub fn caches_synced(&self) {
        self.inner.caches_synced.store(true, Ordering::Relaxed);
    }

    /// Whether `/healthz` answers 200 at `now`.
    #[must_use]
    pub fn live_at(&self, now: Instant) -> bool {
        self.liveness_problem(now).is_none()
    }

    /// Whether `/readyz` answers 200 at `now`.
    #[must_use]
    pub fn ready_at(&self, now: Instant) -> bool {
        self.readiness_problem(now).is_none()
    }

    /// What the `Ready` condition says at `now`. Unlike [`ready_at`], it
    /// does not look at the config.
    ///
    /// [`ready_at`]: Health::ready_at
    #[must_use]
    pub fn readiness_at(&self, now: Instant) -> Readiness {
        if !self.inner.caches_synced.load(Ordering::Relaxed) {
            return Readiness::CachesNotSynced;
        }
        match self.since_finished(now) {
            None => Readiness::CachesNotSynced,
            Some(ago) if ago >= self.interval() * STALE_CYCLES => Readiness::Stalled,
            Some(_) => Readiness::CycleCompleted,
        }
    }

    /// Why `/healthz` fails at `now`, in one line.
    fn liveness_problem(&self, now: Instant) -> Option<String> {
        let started = self.at(self.inner.cycle_started.load(Ordering::Relaxed));
        let ago = now.saturating_duration_since(started);
        let window = (self.interval() * DEAD_CYCLES).max(MIN_LIVENESS_WINDOW);
        (ago >= window).then(|| format!("no cycle started in {}", fmt_duration(ago)))
    }

    /// Why `/readyz` fails at `now`, in one line.
    fn readiness_problem(&self, now: Instant) -> Option<String> {
        if !self.inner.caches_synced.load(Ordering::Relaxed) {
            return Some("caches not synced yet".to_owned());
        }
        if !self.inner.config_applied.load(Ordering::Relaxed) {
            return Some("no valid config applied yet".to_owned());
        }
        let Some(ago) = self.since_finished(now) else {
            return Some("no cycle finished yet".to_owned());
        };
        (ago >= self.interval() * STALE_CYCLES)
            .then(|| format!("last cycle finished {} ago", fmt_duration(ago)))
    }

    fn since_finished(&self, now: Instant) -> Option<Duration> {
        match self.inner.cycle_finished.load(Ordering::Relaxed) {
            0 => None,
            n => Some(now.saturating_duration_since(self.at(n - 1))),
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_nanos(self.inner.interval.load(Ordering::Relaxed))
    }

    fn offset(&self, t: Instant) -> u64 {
        nanos(t.saturating_duration_since(self.inner.base))
    }

    fn at(&self, offset: u64) -> Instant {
        self.inner.base + Duration::from_nanos(offset)
    }
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Whole seconds, the way Go prints a duration: `45s`, `4m12s`, `1h2m3s`.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
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

/// 200 `ok`, or 503 with the one-line reason, so `kubectl describe pod`
/// shows why a probe failed.
fn probe(problem: Option<String>) -> Response {
    match problem {
        None => (StatusCode::OK, "ok\n").into_response(),
        Some(reason) => (StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")).into_response(),
    }
}

async fn healthz(State(state): State<AppState>) -> Response {
    probe(state.health.liveness_problem(Instant::now()))
}

async fn readyz(State(state): State<AppState>) -> Response {
    probe(state.health.readiness_problem(Instant::now()))
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

    const SEC: Duration = Duration::from_secs(1);

    fn just_before(t: Instant) -> Instant {
        t.checked_sub(SEC)
            .expect("an instant after the first second")
    }
    const INTERVALS: [Duration; 2] = [Duration::from_mins(1), Duration::from_secs(15)];

    /// Caches synced and a valid config applied, started at `t0`.
    fn synced(t0: Instant) -> Health {
        let health = Health::starting_at(t0);
        health.caches_synced();
        health.config_applied();
        health
    }

    #[test]
    fn live_from_the_process_start_until_the_window() {
        let t0 = Instant::now();
        let health = Health::starting_at(t0);
        assert!(health.live_at(just_before(t0 + Duration::from_mins(10))));
        assert!(!health.live_at(t0 + Duration::from_mins(10)));
    }

    #[test]
    fn live_until_the_window_after_the_last_cycle_start() {
        // max(10·I, 10m): 10m for 60s and 15s, 20m for 120s.
        for (interval, window) in [
            (Duration::from_mins(1), Duration::from_mins(10)),
            (Duration::from_secs(15), Duration::from_mins(10)),
            (Duration::from_mins(2), Duration::from_mins(20)),
        ] {
            let t0 = Instant::now();
            let health = Health::starting_at(t0);
            health.cycle_started(t0, interval);
            assert!(health.live_at(just_before(t0 + window)), "{interval:?}");
            assert!(!health.live_at(t0 + window), "{interval:?}");
            // A new start moves the window; finishing does not.
            health.cycle_finished(t0 + window, interval);
            assert!(!health.live_at(t0 + window), "{interval:?}");
            health.cycle_started(t0 + window, interval);
            assert!(
                health.live_at(just_before(t0 + window + window)),
                "{interval:?}"
            );
        }
    }

    #[test]
    fn ready_needs_caches_config_and_a_recent_cycle() {
        for interval in INTERVALS {
            let t0 = Instant::now();
            let health = Health::starting_at(t0);
            health.cycle_finished(t0, interval);
            assert!(!health.ready_at(t0), "caches not synced");
            health.caches_synced();
            assert!(!health.ready_at(t0), "no valid config applied");
            health.config_applied();
            assert!(health.ready_at(t0));
            assert!(
                health.ready_at(just_before(t0 + interval * 3)),
                "{interval:?}"
            );
            assert!(!health.ready_at(t0 + interval * 3), "{interval:?}");
            health.cycle_finished(t0 + interval * 3, interval);
            assert!(health.ready_at(t0 + interval * 5), "{interval:?}");
        }
    }

    #[test]
    fn ready_needs_a_finished_cycle() {
        let t0 = Instant::now();
        let health = synced(t0);
        health.cycle_started(t0, Duration::from_mins(1));
        assert!(!health.ready_at(t0));
        assert_eq!(health.readiness_at(t0), Readiness::CachesNotSynced);
    }

    #[test]
    fn config_applied_latches() {
        let t0 = Instant::now();
        let interval = Duration::from_mins(1);
        let health = synced(t0);
        // There is no way to clear it: applying again, and any number of
        // cycles, leave it set.
        health.config_applied();
        for n in 0..5 {
            health.cycle_started(t0 + interval * n, interval);
            health.cycle_finished(t0 + interval * n, interval);
            assert!(health.ready_at(t0 + interval * n));
        }
        assert!(health.clone().ready_at(t0 + interval * 4));
    }

    #[test]
    fn readiness_names_the_reason() {
        for interval in INTERVALS {
            let t0 = Instant::now();
            let health = Health::starting_at(t0);
            assert_eq!(health.readiness_at(t0), Readiness::CachesNotSynced);
            health.caches_synced();
            assert_eq!(
                health.readiness_at(t0),
                Readiness::CachesNotSynced,
                "no cycle has finished yet"
            );
            health.cycle_finished(t0, interval);
            // The config shows on `Valid`, not on `Ready`.
            assert_eq!(health.readiness_at(t0), Readiness::CycleCompleted);
            assert_eq!(
                health.readiness_at(just_before(t0 + interval * 3)),
                Readiness::CycleCompleted
            );
            assert_eq!(health.readiness_at(t0 + interval * 3), Readiness::Stalled);
        }
        let t0 = Instant::now();
        let health = Health::starting_at(t0);
        health.cycle_finished(t0, Duration::from_mins(1));
        assert_eq!(
            health.readiness_at(t0),
            Readiness::CachesNotSynced,
            "finished but not synced"
        );
    }

    #[test]
    fn liveness_fails_strictly_after_readiness() {
        for secs in [10, 15, 30, 60, 61, 120, 300, 600] {
            let interval = Duration::from_secs(secs);
            let t0 = Instant::now();
            let health = synced(t0);
            health.cycle_started(t0, interval);
            health.cycle_finished(t0, interval);
            let first = |ok: &dyn Fn(Instant) -> bool| {
                (0..u32::MAX)
                    .map(|s| t0 + SEC * s)
                    .find(|&t| !ok(t))
                    .expect("fails eventually")
            };
            let unready = first(&|t| health.ready_at(t));
            let dead = first(&|t| health.live_at(t));
            assert!(unready < dead, "{interval:?}: {unready:?} vs {dead:?}");
        }
    }

    #[test]
    fn reasons_are_one_line() {
        let t0 = Instant::now();
        let health = Health::starting_at(t0);
        assert_eq!(
            health
                .liveness_problem(t0 + Duration::from_mins(10))
                .as_deref(),
            Some("no cycle started in 10m0s")
        );
        assert_eq!(health.liveness_problem(t0), None);

        health.caches_synced();
        health.cycle_finished(t0, Duration::from_mins(1));
        assert_eq!(
            health.readiness_problem(t0).as_deref(),
            Some("no valid config applied yet")
        );
        health.config_applied();
        assert_eq!(health.readiness_problem(t0), None);
        assert_eq!(
            health
                .readiness_problem(t0 + Duration::from_secs(252))
                .as_deref(),
            Some("last cycle finished 4m12s ago")
        );
    }

    #[test]
    fn durations_read_like_go() {
        assert_eq!(fmt_duration(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_duration(Duration::from_secs(252)), "4m12s");
        assert_eq!(fmt_duration(Duration::from_mins(10)), "10m0s");
        assert_eq!(fmt_duration(Duration::from_secs(3723)), "1h2m3s");
        assert_eq!(fmt_duration(Duration::from_millis(59_999)), "59s");
    }

    #[tokio::test]
    async fn healthz_is_503_with_the_reason_after_the_window() {
        let health = Health::new();
        let (status, _, _) = get(router(health, registry()), "/healthz").await;
        assert_eq!(status, StatusCode::OK);

        let long_ago = Instant::now()
            .checked_sub(Duration::from_mins(11))
            .expect("the monotonic clock is older than 11 minutes");
        let (status, content_type, body) = get(
            router(Health::starting_at(long_ago), registry()),
            "/healthz",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(content_type.starts_with("text/plain"), "{content_type}");
        assert!(body.starts_with("no cycle started in 11m"), "{body}");
        assert!(!body.trim_end().contains('\n'), "{body}");
    }

    #[tokio::test]
    async fn readyz_is_503_with_the_reason_until_ready() {
        let start = Instant::now()
            .checked_sub(Duration::from_mins(5))
            .expect("the monotonic clock is older than 5 minutes");
        let health = Health::starting_at(start);
        assert_eq!(
            get(router(health.clone(), registry()), "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        health.caches_synced();
        health.cycle_finished(Instant::now(), Duration::from_mins(1));
        let (status, _, body) = get(router(health.clone(), registry()), "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.trim_end(), "no valid config applied yet");

        health.config_applied();
        assert_eq!(
            get(router(health.clone(), registry()), "/readyz").await.0,
            StatusCode::OK
        );

        let stale = Instant::now()
            .checked_sub(Duration::from_secs(252))
            .expect("the monotonic clock is older than 5 minutes");
        health.cycle_finished(stale, Duration::from_mins(1));
        let (status, _, body) = get(router(health, registry()), "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.starts_with("last cycle finished 4m1"), "{body}");
        assert!(body.trim_end().ends_with(" ago"), "{body}");
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
