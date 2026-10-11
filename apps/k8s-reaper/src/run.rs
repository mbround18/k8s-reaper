//! `k8s-reaper run`: wire the adapters to the core and run the cycle loop.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use kube::Client;
use prometheus_client::registry::Registry;
use reaper_core::Timestamp;
use reaper_core::config::Mode;
use reaper_core::config_state::{ConfigChange, Counts, invalid_setting_event};
use reaper_core::decide::decide;
use reaper_core::executor::{Outcome, execute};
use reaper_core::notify::{Notification, Notifier as _, Throttle};
use reaper_core::plan::{CyclePlan, InvalidSetting};
use reaper_core::ports::{EventRecord, EventSink as _};
use reaper_kube::config_watch::{ConfigSource, ReadyWriter};
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

    let server = spawn_server(args.http_addr, &health, registry, stop.clone());

    let client = Client::try_default()
        .await
        .context("building the Kubernetes client")?;
    let mut configs = ConfigSource::new(client.clone(), args.config_name.clone());
    // The status task starts before the caches sync, so `Ready` says
    // `CachesNotSynced` while they do (T126).
    let interval = spawn_status_task(
        configs.ready_writer(),
        health.clone(),
        configs.state().active().cycle_interval,
        stop.clone(),
    );
    let Some(caches) = start_caches(client.clone(), namespace, &health, stop.clone()).await? else {
        return Ok(());
    };

    let instance = std::env::var("HOSTNAME").unwrap_or_else(|_| "k8s-reaper".to_owned());
    let writer = KubeWriter::new(client.clone());
    let events = KubeEventSink::new(client.clone(), instance).with_uids(caches.uids());
    let notifier = start_notifier(&metrics)?;
    let mut channels = Channels::default();
    let mut throttle = Throttle::default();
    #[cfg(debug_assertions)]
    let mut hang = crate::dev_hang::DevHang::from_env();

    loop {
        let started = Instant::now();
        health.cycle_started(started, configs.state().active().cycle_interval);
        #[cfg(debug_assertions)]
        hang.cycle().await;
        let snap = caches.snapshot(now());
        let mut announced = Vec::new();
        let config_ok = refresh_config(
            &mut configs,
            &events,
            &metrics,
            &health,
            snap.now,
            &mut announced,
        )
        .await;
        let config = configs.state().active().clone();
        interval.send_if_modified(|i| std::mem::replace(i, config.cycle_interval) != *i);

        let plan = decide(&snap, &config);
        log_plan(&plan);
        let invalid = configs
            .state_mut()
            .new_invalid_settings(&plan.invalid_settings);
        for setting in invalid {
            report_invalid_setting(&setting, &events, config.mode, snap.now, &mut announced).await;
        }
        metrics.unowned_pods(&plan.unsupported);
        let mut outcome = execute(&snap, &plan, config.mode, &writer, &events, &*metrics).await;
        log_outcome(&outcome);

        channels.reload(&args.notifications_dir, &notifier, &metrics);
        let mut transitions = std::mem::take(&mut outcome.notifications);
        transitions.append(&mut announced);
        for n in throttle.admit(&config.notifications, transitions, snap.now) {
            notifier.enqueue(n, &config.notifications);
        }

        let result = if config_ok && outcome.failures.is_empty() {
            CycleResult::Ok
        } else {
            CycleResult::Error
        };
        metrics.cycle(result, started.elapsed(), SystemTime::now());
        finish_cycle(&health, &mut configs, outcome.counts).await;

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

/// Reads the ReaperConfig and reports a change as a log line, an Event and a
/// notification (FR-026, T063). Returns `false` when it could not be read; the
/// active config is then kept.
///
/// Once a valid config, or the built-in defaults with no object, is in force,
/// tells `health` so (FR-003); that latches, so a later invalid edit does not
/// make the pod unready.
async fn refresh_config(
    configs: &mut ConfigSource,
    events: &KubeEventSink,
    metrics: &Metrics,
    health: &Health,
    time: Timestamp,
    announced: &mut Vec<Notification>,
) -> bool {
    let change = match configs.refresh().await {
        Ok(change) => change,
        Err(err) => {
            warn!(name = configs.name(), %err, "reading ReaperConfig failed; keeping the active config");
            return false;
        }
    };
    let state = configs.state();
    let mode = state.active().mode;
    metrics.config(mode, state.active_generation().unwrap_or(0));
    metrics.config_valid(state.error().is_none());
    if state.error().is_none() {
        health.config_applied();
    }
    let Some(change) = change else {
        return true;
    };
    let name = configs.name();
    match &change {
        ConfigChange::Applied { generation } => {
            info!(name, generation, %mode, "config applied");
        }
        ConfigChange::Invalid { generation, error } => {
            warn!(name, generation, %error, "invalid ReaperConfig; keeping the last valid config");
        }
        ConfigChange::Removed => info!(name, "ReaperConfig removed; using built-in defaults"),
    }
    if let Some(event) = change.event(name) {
        announce(events, event, mode, time, announced).await;
    }
    true
}

/// Starts the watch caches and waits for their first sync, then tells
/// `health` (FR-003). `None` when shutdown was asked for first.
async fn start_caches(
    client: Client,
    namespace: String,
    health: &Health,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<Option<Caches>> {
    let caches = Caches::start(client, namespace)
        .await
        .context("starting the watch caches")?;
    info!(
        keda = caches.keda_enabled(),
        flux = caches.flux_enabled(),
        "waiting for caches to sync"
    );
    tokio::select! {
        synced = caches.wait_until_synced() => synced.context("watch caches stopped before syncing")?,
        () = stopped(stop) => return Ok(None),
    }
    info!("caches synced");
    health.caches_synced();
    Ok(Some(caches))
}

/// Records the end of a cycle in `health`, then writes the status with the
/// readiness that follows from it (FR-003).
async fn finish_cycle(health: &Health, configs: &mut ConfigSource, counts: Counts) {
    let finished = Instant::now();
    health.cycle_finished(finished, configs.state().active().cycle_interval);
    let readiness = health.readiness_at(finished);
    if let Err(err) = configs.write_status(counts, now(), readiness).await {
        warn!(name = configs.name(), %err, "writing ReaperConfig status failed");
    }
}

/// Keeps the `Ready` condition current between cycles (T126): every cycle
/// interval it writes what `health` says, so a loop that hangs shows as
/// `Stalled` on the ReaperConfig. It shares only `Health`'s atomics and the
/// status mutex with the loop. A failed write is logged and retried at the
/// next tick; nothing here ends the process.
///
/// Returns the sender the loop uses to change the interval.
fn spawn_status_task(
    ready: ReadyWriter,
    health: Health,
    interval: Duration,
    stop: watch::Receiver<bool>,
) -> watch::Sender<Duration> {
    let (interval_tx, mut interval) = watch::channel(interval);
    tokio::spawn(async move {
        loop {
            let readiness = health.readiness_at(Instant::now());
            if let Err(err) = ready.write_ready(readiness, now()).await {
                warn!(%err, readiness = readiness.as_str(), "writing ReaperConfig Ready failed");
            }
            let wait = *interval.borrow_and_update();
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                // A new interval takes effect at once, not after the old one.
                Ok(()) = interval.changed() => {}
                () = stopped(stop.clone()) => break,
            }
        }
    });
    interval_tx
}

/// Serves health and metrics until `stop` turns true.
fn spawn_server(
    addr: SocketAddr,
    health: &Health,
    registry: Registry,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let router = http::router(health.clone(), Arc::new(registry));
    tokio::spawn(async move {
        let shutdown = async move {
            let _ = stop.wait_for(|s| *s).await;
        };
        if let Err(err) = http::serve(addr, router, shutdown).await {
            error!(%addr, %err, "HTTP server failed");
        }
    })
}

/// A mode label or threshold override that could not be used, reported once
/// until it is fixed (FR-026).
async fn report_invalid_setting(
    setting: &InvalidSetting,
    events: &KubeEventSink,
    mode: Mode,
    time: Timestamp,
    announced: &mut Vec<Notification>,
) {
    warn!(
        object = %setting.object,
        key = %setting.key,
        value = %setting.value,
        "invalid reaper setting ignored"
    );
    announce(
        events,
        invalid_setting_event(setting),
        mode,
        time,
        announced,
    )
    .await;
}

/// Publishes a config Event and queues the same transition for notification.
async fn announce(
    events: &KubeEventSink,
    event: EventRecord,
    mode: Mode,
    time: Timestamp,
    announced: &mut Vec<Notification>,
) {
    if let Err(err) = events.publish(&event).await {
        warn!(object = %event.regarding, reason = event.reason, %err, "emitting Event failed");
    }
    announced.push(Notification {
        reason: event.reason,
        event_type: event.event_type,
        regarding: event.regarding,
        state: None,
        mode,
        note: event.note,
        time,
    });
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
