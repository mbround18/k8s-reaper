//! `k8s-reaper run`: wire the adapters to the core and run the cycle loop.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use kube::{Api, Client};
use prometheus_client::registry::Registry;
use reaper_core::Timestamp;
use reaper_core::config::Config;
use reaper_core::decide::decide;
use reaper_core::executor::{Outcome, execute};
use reaper_core::notify::{Notifier as _, Throttle};
use reaper_core::plan::CyclePlan;
use reaper_kube::crd::ReaperConfig;
use reaper_kube::events::KubeEventSink;
use reaper_kube::snapshot::Caches;
use reaper_kube::writer::KubeWriter;
use reaper_notify::{ChannelError, ChannelSet, NotifyHandle};
use reaper_observability::http::{self, Health};
use reaper_observability::telemetry::{CycleResult, Metrics};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::RunArgs;

const GIT_SHA: &str = match option_env!("GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

pub fn main(args: RunArgs) -> anyhow::Result<()> {
    let _tracing = reaper_observability::tracing::init(args.log_format)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    let namespace = args
        .namespace
        .clone()
        .context("--namespace / REAPER_NAMESPACE is required")?;

    let mut registry = Registry::default();
    let metrics = Arc::new(Metrics::register(
        &mut registry,
        env!("CARGO_PKG_VERSION"),
        GIT_SHA,
    ));
    let health = Health::new();

    let (stop_tx, stop) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutdown requested");
        let _ = stop_tx.send(true);
    });

    let server = {
        let router = http::router(health.clone(), Arc::new(registry));
        let mut stop = stop.clone();
        let addr = args.http_addr;
        tokio::spawn(async move {
            let shutdown = async move {
                let _ = stop.wait_for(|s| *s).await;
            };
            if let Err(err) = http::serve(addr, router, shutdown).await {
                error!(%addr, %err, "HTTP server failed");
            }
        })
    };

    let client = Client::try_default()
        .await
        .context("building the Kubernetes client")?;
    let caches = Caches::start(client.clone(), namespace.clone())
        .await
        .context("starting the watch caches")?;
    info!(
        keda = caches.keda_enabled(),
        flux = caches.flux_enabled(),
        "waiting for caches to sync"
    );
    tokio::select! {
        synced = caches.wait_until_synced() => synced.context("watch caches stopped before syncing")?,
        () = stopped(stop.clone()) => return Ok(()),
    }
    info!("caches synced");

    let instance = std::env::var("HOSTNAME").unwrap_or_else(|_| "k8s-reaper".to_owned());
    let writer = KubeWriter::new(client.clone());
    let events = KubeEventSink::new(client.clone(), instance).with_uids(caches.uids());
    let mut configs = ConfigSource::new(Api::all(client), args.config_name.clone());
    let notifier = start_notifier(&metrics)?;
    let mut channels = Channels::default();
    let mut throttle = Throttle::default();

    loop {
        let started = Instant::now();
        let config_ok = configs.refresh(&metrics).await;
        let config = configs.active.clone();

        let snap = caches.snapshot(now());
        let plan = decide(&snap, &config);
        log_plan(&plan);
        metrics.unowned_pods(&plan.unsupported);
        let mut outcome = execute(&snap, &plan, config.mode, &writer, &events, &*metrics).await;
        log_outcome(&outcome);

        channels.reload(&args.notifications_dir, &notifier, &metrics);
        let transitions = std::mem::take(&mut outcome.notifications);
        for n in throttle.admit(&config.notifications, transitions, snap.now) {
            notifier.enqueue(n, &config.notifications);
        }

        let result = if config_ok && outcome.failures.is_empty() {
            CycleResult::Ok
        } else {
            CycleResult::Error
        };
        metrics.cycle(result, started.elapsed(), SystemTime::now());
        health.cycle_finished(Instant::now(), config.cycle_interval);
        health.set_ready(true);

        // The next cycle runs at the tick, or sooner when a watched change
        // asks for one (T054), but never within MIN_GAP of this one.
        tokio::select! {
            () = tokio::time::sleep(config.cycle_interval) => {}
            () = async {
                caches.triggered().await;
                tokio::time::sleep(MIN_GAP.saturating_sub(started.elapsed())).await;
            } => info!("cycle triggered by a watched change"),
            () = stopped(stop.clone()) => break,
        }
    }

    let left = notifier.shutdown(NOTIFY_DRAIN).await;
    if left > 0 {
        warn!(left, "notifications not delivered before shutdown");
    }
    drop(caches);
    let _ = server.await;
    Ok(())
}

/// The shortest time between the starts of two cycles when a watched change
/// triggers one. Writes the reaper makes itself can wake the trigger, so this
/// bounds the extra cycles they cause.
const MIN_GAP: Duration = Duration::from_secs(2);

/// How long shutdown waits for queued notifications (contracts/notifications.md).
const NOTIFY_DRAIN: Duration = Duration::from_secs(5);

fn start_notifier(metrics: &Arc<Metrics>) -> anyhow::Result<NotifyHandle> {
    let metrics = Arc::clone(metrics);
    let settings = reaper_notify::Settings {
        user_agent: format!("k8s-reaper/{}", env!("CARGO_PKG_VERSION")),
        ..reaper_notify::Settings::default()
    };
    NotifyHandle::start(
        settings,
        Arc::new(
            move |channel: &str,
                  provider: reaper_notify::Provider,
                  result: reaper_notify::Delivery| {
                metrics.notification(channel, provider.as_str(), result.as_str());
            },
        ),
    )
    .context("starting notification delivery")
}

/// The channel directory, re-read every cycle so a rotated Secret takes effect
/// without a restart. Logs only when the set of channels changes.
#[derive(Default)]
struct Channels {
    valid: Vec<String>,
    invalid: Vec<(String, ChannelError)>,
}

impl Channels {
    fn reload(&mut self, dir: &Path, notifier: &NotifyHandle, metrics: &Metrics) {
        let ChannelSet { valid, invalid } = reaper_notify::load_dir(dir);
        let mut rows: Vec<(String, String, bool)> = valid
            .iter()
            .map(|c| (c.name().to_owned(), c.provider().as_str().to_owned(), true))
            .collect();
        rows.extend(
            invalid
                .iter()
                .map(|(name, _)| (name.clone(), "unknown".to_owned(), false)),
        );
        metrics.notification_channels(&rows);

        let names: Vec<String> = valid.iter().map(|c| c.name().to_owned()).collect();
        if names != self.valid {
            info!(channels = ?names, "notification channels loaded");
            self.valid = names;
        }
        if invalid != self.invalid {
            for (channel, err) in &invalid {
                warn!(channel, %err, "notification channel ignored");
            }
            self.invalid = invalid;
        }
        notifier.set_channels(valid);
    }
}

/// The active configuration: built-in defaults while no ReaperConfig exists,
/// and the last valid one while the current one is invalid (FR-026).
struct ConfigSource {
    api: Api<ReaperConfig>,
    name: String,
    active: Config,
    generation: Option<i64>,
}

impl ConfigSource {
    fn new(api: Api<ReaperConfig>, name: String) -> Self {
        Self {
            api,
            name,
            active: Config::default(),
            generation: None,
        }
    }

    /// Reads the ReaperConfig. Returns `false` when it could not be read.
    async fn refresh(&mut self, metrics: &Metrics) -> bool {
        let object = match self.api.get_opt(&self.name).await {
            Ok(object) => object,
            Err(err) => {
                warn!(name = %self.name, %err, "reading ReaperConfig failed; keeping the active config");
                return false;
            }
        };
        let Some(object) = object else {
            if self.generation.is_some() {
                info!(name = %self.name, "ReaperConfig removed; using built-in defaults");
            }
            self.apply(Config::default(), 0, metrics);
            self.generation = None;
            metrics.config_valid(true);
            return true;
        };
        let generation = object.metadata.generation.unwrap_or_default();
        match object.spec.to_config() {
            Ok(config) => {
                if self.generation != Some(generation) {
                    info!(name = %self.name, generation, mode = %config.mode, "config applied");
                }
                self.apply(config, generation, metrics);
                self.generation = Some(generation);
                metrics.config_valid(true);
            }
            Err(err) => {
                warn!(name = %self.name, generation, %err, "invalid ReaperConfig; keeping the last valid config");
                metrics.config_valid(false);
            }
        }
        true
    }

    fn apply(&mut self, config: Config, generation: i64, metrics: &Metrics) {
        metrics.config(config.mode, generation);
        self.active = config;
    }
}

fn log_plan(plan: &CyclePlan) {
    for pod in &plan.unsupported {
        info!(
            namespace = %pod.namespace,
            pod = %pod.pod,
            owner_kind = pod.owner_kind.as_deref().unwrap_or("none"),
            "crash-looping pod has no supported owner; not tracked"
        );
    }
    for setting in &plan.invalid_settings {
        warn!(
            object = %setting.object,
            key = %setting.key,
            value = %setting.value,
            "invalid reaper setting ignored"
        );
    }
}

fn log_outcome(outcome: &Outcome) {
    for failure in &outcome.failures {
        warn!(workload = %failure.workload, err = %failure.error, "write failed; retrying next cycle");
    }
    for (workload, reason, err) in &outcome.event_failures {
        warn!(%workload, reason, %err, "emitting Event failed");
    }
}

fn now() -> Timestamp {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Timestamp(i64::try_from(secs).unwrap_or(i64::MAX))
}

async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
}
