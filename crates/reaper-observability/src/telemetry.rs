//! The `Telemetry` port over `prometheus-client`, mirrored to `OTel` meters
//! (contracts/metrics.md).
//!
//! `OTel` instruments come from the global meter provider, so they are no-ops
//! unless one is installed. Their names are the Prometheus names with `.` for
//! `_` and no `_total`.

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{
    Counter as OtelCounter, Gauge as OtelGauge, Histogram as OtelHistogram, ObservableGauge,
};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use reaper_core::config::Mode;
use reaper_core::ports::{ActionKind, ActionResult, Telemetry, WorkloadRow};

const METER: &str = "k8s-reaper";

/// How a reconcile cycle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleResult {
    Ok,
    Error,
}

impl CycleResult {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CycleResult::Ok => "ok",
            CycleResult::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct WorkloadLabels {
    namespace: String,
    kind: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct StateLabels {
    namespace: String,
    kind: String,
    name: String,
    state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct SkippedLabels {
    namespace: String,
    kind: String,
    name: String,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ActionLabels {
    action: &'static str,
    result: &'static str,
    mode: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct CycleLabels {
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ConfigLabels {
    mode: &'static str,
    generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct NotificationLabels {
    channel: String,
    provider: String,
    result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ChannelLabels {
    channel: String,
    provider: String,
    valid: &'static str,
}

/// One channel file read in the last cycle: name, provider, valid.
pub type ChannelRow = (String, String, bool);

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct BuildLabels {
    version: String,
    git_sha: String,
}

/// The per-workload label sets of one `workloads` call.
#[derive(Debug, Default)]
struct Emitted {
    state: HashSet<StateLabels>,
    skipped: HashSet<SkippedLabels>,
    crashloop: HashSet<WorkloadLabels>,
}

impl Emitted {
    fn from_rows(rows: &[WorkloadRow]) -> Self {
        let mut out = Emitted::default();
        for row in rows {
            let w = workload_labels(row);
            out.state.insert(StateLabels {
                namespace: w.namespace.clone(),
                kind: w.kind.clone(),
                name: w.name.clone(),
                state: row.state.as_str().to_owned(),
            });
            if let Some(reason) = row.skipped {
                out.skipped.insert(SkippedLabels {
                    namespace: w.namespace.clone(),
                    kind: w.kind.clone(),
                    name: w.name.clone(),
                    reason: reason.as_str().to_owned(),
                });
            }
            if row.crashloop_seconds.is_some() {
                out.crashloop.insert(w);
            }
        }
        out
    }
}

fn workload_labels(row: &WorkloadRow) -> WorkloadLabels {
    WorkloadLabels {
        namespace: row.workload.namespace.clone(),
        kind: row.workload.kind.as_str().to_owned(),
        name: row.workload.name.clone(),
    }
}

fn workload_attrs(row: &WorkloadRow) -> Vec<KeyValue> {
    vec![
        KeyValue::new("namespace", row.workload.namespace.clone()),
        KeyValue::new("kind", row.workload.kind.as_str()),
        KeyValue::new("name", row.workload.name.clone()),
    ]
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `OTel` mirrors. The observable gauges read the last `workloads` rows, so
/// cleared workloads stop being reported.
struct Otel {
    _workload_state: ObservableGauge<u64>,
    _workload_skipped: ObservableGauge<u64>,
    _crashloop_seconds: ObservableGauge<i64>,
    actions: OtelCounter<u64>,
    cycles: OtelCounter<u64>,
    cycle_duration: OtelHistogram<f64>,
    last_cycle: OtelGauge<f64>,
    config_info: OtelGauge<u64>,
    config_valid: OtelGauge<u64>,
    notifications: OtelCounter<u64>,
    _notification_channels: ObservableGauge<u64>,
}

impl Otel {
    fn new(
        rows: &Arc<Mutex<Vec<WorkloadRow>>>,
        channels: &Arc<Mutex<Vec<ChannelRow>>>,
        version: &str,
        git_sha: &str,
    ) -> Self {
        let meter = opentelemetry::global::meter(METER);
        let r = Arc::clone(rows);
        let workload_state = meter
            .u64_observable_gauge("reaper.workload.state")
            .with_callback(move |o| {
                for row in lock(&r).iter() {
                    let mut attrs = workload_attrs(row);
                    attrs.push(KeyValue::new("state", row.state.as_str()));
                    o.observe(1, &attrs);
                }
            })
            .build();
        let r = Arc::clone(rows);
        let workload_skipped = meter
            .u64_observable_gauge("reaper.workload.skipped")
            .with_callback(move |o| {
                for row in lock(&r).iter() {
                    if let Some(reason) = row.skipped {
                        let mut attrs = workload_attrs(row);
                        attrs.push(KeyValue::new("reason", reason.as_str()));
                        o.observe(1, &attrs);
                    }
                }
            })
            .build();
        let r = Arc::clone(rows);
        let crashloop_seconds = meter
            .i64_observable_gauge("reaper.workload.crashloop.seconds")
            .with_callback(move |o| {
                for row in lock(&r).iter() {
                    if let Some(secs) = row.crashloop_seconds {
                        o.observe(secs, &workload_attrs(row));
                    }
                }
            })
            .build();
        let c = Arc::clone(channels);
        let notification_channels = meter
            .u64_observable_gauge("reaper.notification.channels")
            .with_callback(move |o| {
                for (channel, provider, valid) in lock(&c).iter() {
                    o.observe(
                        1,
                        &[
                            KeyValue::new("channel", channel.clone()),
                            KeyValue::new("provider", provider.clone()),
                            KeyValue::new("valid", valid.to_string()),
                        ],
                    );
                }
            })
            .build();
        meter.u64_gauge("reaper.build.info").build().record(
            1,
            &[
                KeyValue::new("version", version.to_owned()),
                KeyValue::new("git_sha", git_sha.to_owned()),
            ],
        );
        Otel {
            _workload_state: workload_state,
            _workload_skipped: workload_skipped,
            _crashloop_seconds: crashloop_seconds,
            actions: meter.u64_counter("reaper.actions").build(),
            cycles: meter.u64_counter("reaper.cycles").build(),
            cycle_duration: meter
                .f64_histogram("reaper.cycle.duration.seconds")
                .with_unit("s")
                .build(),
            last_cycle: meter
                .f64_gauge("reaper.last.cycle.timestamp.seconds")
                .with_unit("s")
                .build(),
            config_info: meter.u64_gauge("reaper.config.info").build(),
            config_valid: meter.u64_gauge("reaper.config.valid").build(),
            notifications: meter.u64_counter("reaper.notifications").build(),
            _notification_channels: notification_channels,
        }
    }
}

/// Every contracted metric, registered once into a [`Registry`].
pub struct Metrics {
    workload_state: Family<StateLabels, Gauge>,
    workload_skipped: Family<SkippedLabels, Gauge>,
    crashloop_seconds: Family<WorkloadLabels, Gauge>,
    actions: Family<ActionLabels, Counter>,
    cycles: Family<CycleLabels, Counter>,
    cycle_duration: Histogram,
    last_cycle: Gauge<f64, AtomicU64>,
    config_info: Family<ConfigLabels, Gauge>,
    config_valid: Gauge,
    notifications: Family<NotificationLabels, Counter>,
    notification_channels: Family<ChannelLabels, Gauge>,
    /// The last `notification_channels` rows, read by the `OTel` callback.
    channels: Arc<Mutex<Vec<ChannelRow>>>,
    /// Label sets emitted by the last `workloads` call.
    emitted: Mutex<Emitted>,
    /// The last `workloads` rows, read by the `OTel` callbacks.
    rows: Arc<Mutex<Vec<WorkloadRow>>>,
    otel: Otel,
}

impl Metrics {
    /// Registers every metric into `registry` and sets `reaper_build_info`.
    pub fn register(registry: &mut Registry, version: &str, git_sha: &str) -> Self {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let channels = Arc::new(Mutex::new(Vec::new()));
        let metrics = Metrics {
            workload_state: Family::default(),
            workload_skipped: Family::default(),
            crashloop_seconds: Family::default(),
            actions: Family::default(),
            cycles: Family::default(),
            cycle_duration: Histogram::new(exponential_buckets(0.05, 2.0, 12)),
            last_cycle: Gauge::default(),
            config_info: Family::default(),
            config_valid: Gauge::default(),
            notifications: Family::default(),
            notification_channels: Family::default(),
            emitted: Mutex::default(),
            otel: Otel::new(&rows, &channels, version, git_sha),
            rows,
            channels,
        };
        let build_info = Family::<BuildLabels, Gauge>::default();
        build_info
            .get_or_create(&BuildLabels {
                version: version.to_owned(),
                git_sha: git_sha.to_owned(),
            })
            .set(1);

        registry.register(
            "reaper_workload_state",
            "1 for the workload's current state",
            metrics.workload_state.clone(),
        );
        registry.register(
            "reaper_workload_skipped",
            "1 while a candidate is skipped for reason",
            metrics.workload_skipped.clone(),
        );
        registry.register(
            "reaper_workload_crashloop_seconds",
            "Seconds since crashloop-since (failing-since for a CronJob)",
            metrics.crashloop_seconds.clone(),
        );
        registry.register(
            "reaper_actions",
            "Reap and restore steps",
            metrics.actions.clone(),
        );
        registry.register("reaper_cycles", "Reconcile cycles", metrics.cycles.clone());
        registry.register(
            "reaper_cycle_duration_seconds",
            "Decide and execute time per cycle",
            metrics.cycle_duration.clone(),
        );
        registry.register(
            "reaper_last_cycle_timestamp_seconds",
            "Unix time the last cycle finished",
            metrics.last_cycle.clone(),
        );
        registry.register(
            "reaper_config_info",
            "1 for the active config",
            metrics.config_info.clone(),
        );
        registry.register(
            "reaper_config_valid",
            "1 if the latest ReaperConfig generation is valid, else 0",
            metrics.config_valid.clone(),
        );
        registry.register(
            "reaper_notifications",
            "Notifications by channel and result",
            metrics.notifications.clone(),
        );
        registry.register(
            "reaper_notification_channels",
            "1 per channel file read in the last cycle",
            metrics.notification_channels.clone(),
        );
        registry.register("reaper_build_info", "Build version", build_info);
        metrics
    }

    /// Records a finished cycle: its result, duration and finish time.
    pub fn cycle(&self, result: CycleResult, duration: Duration, finished: SystemTime) {
        let secs = duration.as_secs_f64();
        let at = finished
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.cycles
            .get_or_create(&CycleLabels {
                result: result.as_str(),
            })
            .inc();
        self.cycle_duration.observe(secs);
        self.last_cycle.set(at);

        self.otel
            .cycles
            .add(1, &[KeyValue::new("result", result.as_str())]);
        self.otel.cycle_duration.record(secs, &[]);
        self.otel.last_cycle.record(at, &[]);
    }

    /// Sets the active config; replaces the previous `reaper_config_info` series.
    pub fn config(&self, mode: Mode, generation: i64) {
        self.config_info.clear();
        self.config_info
            .get_or_create(&ConfigLabels {
                mode: mode.as_str(),
                generation: generation.to_string(),
            })
            .set(1);
        self.otel.config_info.record(
            1,
            &[
                KeyValue::new("mode", mode.as_str()),
                KeyValue::new("generation", generation.to_string()),
            ],
        );
    }

    /// Whether the latest `ReaperConfig` generation is valid.
    pub fn config_valid(&self, valid: bool) {
        self.config_valid.set(i64::from(valid));
        self.otel.config_valid.record(u64::from(valid), &[]);
    }

    /// Counts one notification result (`sent`, `failed`, `dropped`, `filtered`).
    pub fn notification(&self, channel: &str, provider: &str, result: &str) {
        self.notifications
            .get_or_create(&NotificationLabels {
                channel: channel.to_owned(),
                provider: provider.to_owned(),
                result: result.to_owned(),
            })
            .inc();
        self.otel.notifications.add(
            1,
            &[
                KeyValue::new("channel", channel.to_owned()),
                KeyValue::new("provider", provider.to_owned()),
                KeyValue::new("result", result.to_owned()),
            ],
        );
    }

    /// Replaces `reaper_notification_channels` with this cycle's channel files.
    pub fn notification_channels(&self, rows: &[ChannelRow]) {
        self.notification_channels.clear();
        for (channel, provider, valid) in rows {
            self.notification_channels
                .get_or_create(&ChannelLabels {
                    channel: channel.clone(),
                    provider: provider.clone(),
                    valid: if *valid { "true" } else { "false" },
                })
                .set(1);
        }
        *lock(&self.channels) = rows.to_vec();
    }
}

impl Telemetry for Metrics {
    fn workloads(&self, rows: &[WorkloadRow]) {
        let next = Emitted::from_rows(rows);
        let mut emitted = lock(&self.emitted);
        for gone in emitted.state.difference(&next.state) {
            self.workload_state.remove(gone);
        }
        for gone in emitted.skipped.difference(&next.skipped) {
            self.workload_skipped.remove(gone);
        }
        for gone in emitted.crashloop.difference(&next.crashloop) {
            self.crashloop_seconds.remove(gone);
        }
        for labels in &next.state {
            self.workload_state.get_or_create(labels).set(1);
        }
        for labels in &next.skipped {
            self.workload_skipped.get_or_create(labels).set(1);
        }
        for row in rows {
            if let Some(secs) = row.crashloop_seconds {
                self.crashloop_seconds
                    .get_or_create(&workload_labels(row))
                    .set(secs);
            }
        }
        *emitted = next;
        *lock(&self.rows) = rows.to_vec();
    }

    fn action(&self, kind: ActionKind, result: ActionResult, mode: Mode) {
        self.actions
            .get_or_create(&ActionLabels {
                action: kind.as_str(),
                result: result.as_str(),
                mode: mode.as_str(),
            })
            .inc();
        self.otel.actions.add(
            1,
            &[
                KeyValue::new("action", kind.as_str()),
                KeyValue::new("result", result.as_str()),
                KeyValue::new("mode", mode.as_str()),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reaper_core::model::{WorkloadKind, WorkloadRef};
    use reaper_core::plan::SkipReason;
    use reaper_core::record::State;

    use super::*;

    /// contracts/metrics.md: name, type and label names.
    const CONTRACT: &[(&str, &str, &[&str])] = &[
        (
            "reaper_workload_state",
            "gauge",
            &["namespace", "kind", "name", "state"],
        ),
        (
            "reaper_workload_skipped",
            "gauge",
            &["namespace", "kind", "name", "reason"],
        ),
        (
            "reaper_workload_crashloop_seconds",
            "gauge",
            &["namespace", "kind", "name"],
        ),
        (
            "reaper_actions_total",
            "counter",
            &["action", "result", "mode"],
        ),
        ("reaper_cycles_total", "counter", &["result"]),
        ("reaper_cycle_duration_seconds", "histogram", &[]),
        ("reaper_last_cycle_timestamp_seconds", "gauge", &[]),
        ("reaper_config_info", "gauge", &["mode", "generation"]),
        ("reaper_config_valid", "gauge", &[]),
        (
            "reaper_notifications_total",
            "counter",
            &["channel", "provider", "result"],
        ),
        (
            "reaper_notification_channels",
            "gauge",
            &["channel", "provider", "valid"],
        ),
        ("reaper_build_info", "gauge", &["version", "git_sha"]),
    ];

    type Shape = BTreeMap<String, (String, Vec<String>)>;

    fn encode(registry: &Registry) -> String {
        let mut body = String::new();
        prometheus_client::encoding::text::encode(&mut body, registry).unwrap();
        body
    }

    fn label_names(sample: &str) -> Vec<String> {
        let Some((_, rest)) = sample.split_once('{') else {
            return Vec::new();
        };
        let inner = &rest[..rest.find('}').unwrap()];
        inner
            .split("\",")
            .filter_map(|kv| kv.split_once('=').map(|(k, _)| k.to_owned()))
            .filter(|k| k != "le")
            .collect()
    }

    /// Contract names (counters with `_total`) to type and sorted label names.
    fn shape(body: &str) -> Shape {
        let mut types = BTreeMap::new();
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, ty) = rest.split_once(' ').unwrap();
                types.insert(name.to_owned(), ty.to_owned());
            }
        }
        let mut out: Shape = BTreeMap::new();
        for (family, ty) in &types {
            let name = if ty == "counter" {
                format!("{family}_total")
            } else {
                family.clone()
            };
            out.insert(name, (ty.clone(), Vec::new()));
        }
        for line in body.lines().filter(|l| !l.starts_with('#')) {
            let sample = line.split([' ', '{']).next().unwrap();
            let family = types
                .keys()
                .filter(|f| sample.starts_with(f.as_str()))
                .max_by_key(|f| f.len())
                .unwrap_or_else(|| panic!("sample {sample} has no family"));
            let ty = &types[family];
            let name = if ty == "counter" {
                format!("{family}_total")
            } else {
                family.clone()
            };
            let mut labels = label_names(line);
            labels.sort();
            let entry = &mut out.get_mut(&name).unwrap().1;
            if entry.is_empty() {
                *entry = labels;
            } else if !labels.is_empty() {
                assert_eq!(entry, &labels, "{name} has mixed label sets");
            }
        }
        out
    }

    fn contract() -> Shape {
        CONTRACT
            .iter()
            .map(|(name, ty, labels)| {
                let mut labels: Vec<String> = labels.iter().map(|&l| l.to_owned()).collect();
                labels.sort();
                ((*name).to_owned(), ((*ty).to_owned(), labels))
            })
            .collect()
    }

    fn row(
        kind: WorkloadKind,
        name: &str,
        state: State,
        skipped: Option<SkipReason>,
    ) -> WorkloadRow {
        WorkloadRow {
            workload: WorkloadRef::new(kind, "apps", name),
            state,
            skipped,
            crashloop_seconds: Some(600),
        }
    }

    fn metrics() -> (Registry, Metrics) {
        let mut registry = Registry::default();
        let metrics = Metrics::register(&mut registry, "0.1.0", "abc123");
        (registry, metrics)
    }

    #[test]
    fn exposition_has_exactly_the_contracted_metrics() {
        let (registry, metrics) = metrics();
        metrics.workloads(&[row(
            WorkloadKind::Deployment,
            "web",
            State::Candidate,
            Some(SkipReason::RateLimited),
        )]);
        metrics.action(ActionKind::Scale, ActionResult::Planned, Mode::Report);
        metrics.cycle(
            CycleResult::Ok,
            Duration::from_millis(250),
            SystemTime::now(),
        );
        metrics.config(Mode::Report, 3);
        metrics.config_valid(true);
        metrics.notification("ops-discord", "discord", "sent");
        metrics.notification_channels(&[("ops-discord".into(), "discord".into(), true)]);

        let body = encode(&registry);
        assert_eq!(shape(&body), contract(), "{body}");
    }

    #[test]
    fn notification_channels_keep_only_the_last_cycle() {
        let (registry, metrics) = metrics();
        metrics.notification_channels(&[
            ("ops-discord".into(), "discord".into(), true),
            ("broken".into(), "unknown".into(), false),
        ]);
        metrics.notification_channels(&[("ops-discord".into(), "discord".into(), true)]);
        let body = encode(&registry);
        assert!(body.contains(
            r#"reaper_notification_channels{channel="ops-discord",provider="discord",valid="true"} 1"#
        ));
        assert!(!body.contains("broken"), "{body}");
    }

    #[test]
    fn a_cleared_workload_loses_its_series() {
        let (registry, metrics) = metrics();
        metrics.workloads(&[
            row(
                WorkloadKind::Deployment,
                "web",
                State::Candidate,
                Some(SkipReason::OptedOut),
            ),
            row(WorkloadKind::CronJob, "backup", State::Reaped, None),
        ]);
        let body = encode(&registry);
        assert!(body.contains("name=\"web\""), "{body}");
        assert!(body.contains("name=\"backup\""), "{body}");

        metrics.workloads(&[row(WorkloadKind::CronJob, "backup", State::Reaped, None)]);
        let body = encode(&registry);
        assert!(!body.contains("name=\"web\""), "{body}");
        assert!(body.contains("name=\"backup\""), "{body}");

        metrics.workloads(&[]);
        let body = encode(&registry);
        assert!(!body.contains("name=\""), "{body}");
    }

    #[test]
    fn a_state_change_replaces_the_state_series() {
        let (registry, metrics) = metrics();
        metrics.workloads(&[row(WorkloadKind::Deployment, "web", State::Candidate, None)]);
        metrics.workloads(&[row(WorkloadKind::Deployment, "web", State::Reaped, None)]);
        let body = encode(&registry);
        assert!(!body.contains("state=\"candidate\""), "{body}");
        assert!(body.contains("state=\"reaped\""), "{body}");
    }

    #[test]
    fn actions_count_by_kind_result_and_mode() {
        let (registry, metrics) = metrics();
        metrics.action(
            ActionKind::CronJobSuspend,
            ActionResult::Success,
            Mode::Full,
        );
        metrics.action(
            ActionKind::CronJobSuspend,
            ActionResult::Success,
            Mode::Full,
        );
        metrics.action(ActionKind::Scale, ActionResult::Error, Mode::Full);
        let body = encode(&registry);
        assert!(
            body.contains(
                "reaper_actions_total{action=\"cronjob_suspend\",result=\"success\",mode=\"full\"} 2"
            ),
            "{body}"
        );
        assert!(
            body.contains(
                "reaper_actions_total{action=\"scale\",result=\"error\",mode=\"full\"} 1"
            ),
            "{body}"
        );
    }

    #[test]
    fn config_info_keeps_only_the_active_config() {
        let (registry, metrics) = metrics();
        metrics.config(Mode::Report, 1);
        metrics.config(Mode::Full, 2);
        let body = encode(&registry);
        assert!(!body.contains("generation=\"1\""), "{body}");
        assert!(
            body.contains("reaper_config_info{mode=\"full\",generation=\"2\"} 1"),
            "{body}"
        );
    }
}
