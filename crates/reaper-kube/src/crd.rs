//! The `ReaperConfig` custom resource (contracts/config.md).
//!
//! Every spec field is optional: the core fills defaults and re-validates
//! (`reaper_core::config::Config::from_raw`). The schema here repeats the
//! enum, range and pattern rules so the API server rejects bad values first.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, CustomResourceExt};
use reaper_core::config::{Config, ConfigError, RawConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Go/Flux duration form accepted by the core: `90s`, `30m`, `1h30m`.
const DURATION_PATTERN: &str = "^([0-9]+[hms])+$";

#[derive(CustomResource, Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "reaper.boop.ninja",
    version = "v1alpha1",
    kind = "ReaperConfig",
    shortname = "rc",
    status = "ReaperConfigStatus",
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.mode"}"#,
    printcolumn = r#"{"name":"Valid","type":"string","jsonPath":".status.conditions[?(@.type==\"Valid\")].status"}"#,
    printcolumn = r#"{"name":"Candidates","type":"integer","jsonPath":".status.counts.candidate"}"#,
    printcolumn = r#"{"name":"Reaped","type":"integer","jsonPath":".status.counts.reaped"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ReaperConfigSpec {
    /// `report` (default) only records what would happen; `full` acts.
    #[serde(default)]
    pub mode: SpecMode,
    /// Time between cycles (10s–10m, default 60s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = DURATION_PATTERN))]
    pub cycle_interval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<LimitsSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flux: Option<FluxSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_jobs: Option<CronJobsSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<NotificationsSpec>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SpecMode {
    #[default]
    Report,
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DetectionSpec {
    /// Continuous crash-looping before a workload is a candidate (5m–168h, default 30m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = DURATION_PATTERN))]
    pub crash_loop_duration: Option<String>,
    /// Restart count of the crash-looping container (1–10000, default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 10000))]
    pub min_restarts: Option<i64>,
    /// Continuous health before state is cleared (1m–24h, default 10m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = DURATION_PATTERN))]
    pub recovery_window: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScopeSpec {
    /// Namespace names or globs never acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_namespaces: Option<Vec<String>>,
    /// Namespace globs to act on; empty means all. The reaper's own namespace is always excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_namespaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LimitsSpec {
    /// Reaps per cycle (0–50, default 1; 0 reports everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 50))]
    pub max_reaps_per_cycle: Option<i64>,
    /// Workloads reaped at once (0–500, default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 500))]
    pub max_reaped: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FluxSpec {
    /// Disable kustomize reconcile on reaped objects (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kustomize: Option<bool>,
    /// Suspend owning HelmReleases (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helm_release: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CronJobsSpec {
    /// Observe CronJobs and pause ones that keep failing (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Every run failed for this long (10m–168h, default 6h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = DURATION_PATTERN))]
    pub failing_duration: Option<String>,
    /// And at least this many failed runs (1–1000, default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1000))]
    pub min_failed_runs: Option<i64>,
    /// Also pause a CronJob that fails most of its recent runs, even with the odd success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_rate: Option<FailureRateSpec>,
}

const PERCENT_PATTERN: &str = "^(5[1-9]|[6-9][0-9]|100)%?$";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureRateSpec {
    /// The failure-rate rule (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Finished runs in the window; it must be full before it is judged (3–100, default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 3, max = 100))]
    pub window_runs: Option<i64>,
    /// Share of failed runs that makes a candidate, "80%" or "80" (51–100, default 80%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = PERCENT_PATTERN))]
    pub threshold: Option<String>,
}

/// Which transitions reach notification channels (US6). Channel URLs come
/// from a mounted Secret, never from here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationsSpec {
    /// Master switch (default true); with no channels mounted nothing is sent anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Cluster name shown in messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 63))]
    pub cluster_name: Option<String>,
    /// Event reasons to send (default: `Reaped`, `ReapFailed`, `Restored`, `RestoreFailed`, `HelmReleaseHeld`, `ConfigInvalid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasons: Option<Vec<String>>,
    /// Drop repeats for the same object and reason inside this window (0s–24h, default 1h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = DURATION_PATTERN))]
    pub min_interval: Option<String>,
    /// Per-channel reason filters, keyed by the channel's Secret key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<BTreeMap<String, ChannelSpec>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSpec {
    /// Reasons this channel receives, out of `notifications.reasons`.
    #[serde(default)]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReaperConfigStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Generation in use; lags `observedGeneration` while the spec is invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cycle_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<StateCounts>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StateCounts {
    pub observed: u32,
    pub candidate: u32,
    pub reaping: u32,
    pub reaped: u32,
    pub skipped: u32,
}

impl ReaperConfigSpec {
    #[must_use]
    pub fn to_raw(&self) -> RawConfig {
        let detection = self.detection.clone().unwrap_or_default();
        let scope = self.scope.clone().unwrap_or_default();
        let limits = self.limits.clone().unwrap_or_default();
        let flux = self.flux.clone().unwrap_or_default();
        let cron_jobs = self.cron_jobs.clone().unwrap_or_default();
        let failure_rate = cron_jobs.failure_rate.clone().unwrap_or_default();
        let notifications = self.notifications.clone().unwrap_or_default();
        RawConfig {
            mode: Some(
                match self.mode {
                    SpecMode::Report => "report",
                    SpecMode::Full => "full",
                }
                .to_owned(),
            ),
            cycle_interval: self.cycle_interval.clone(),
            crash_loop_duration: detection.crash_loop_duration,
            min_restarts: detection.min_restarts,
            recovery_window: detection.recovery_window,
            exclude_namespaces: scope.exclude_namespaces,
            include_namespaces: scope.include_namespaces,
            max_reaps_per_cycle: limits.max_reaps_per_cycle,
            max_reaped: limits.max_reaped,
            flux_kustomize: flux.kustomize,
            flux_helm_release: flux.helm_release,
            cron_jobs_enabled: cron_jobs.enabled,
            cron_jobs_failing_duration: cron_jobs.failing_duration,
            cron_jobs_min_failed_runs: cron_jobs.min_failed_runs,
            cron_jobs_failure_rate_enabled: failure_rate.enabled,
            cron_jobs_failure_rate_window_runs: failure_rate.window_runs,
            cron_jobs_failure_rate_threshold: failure_rate.threshold,
            notifications_enabled: notifications.enabled,
            notifications_cluster_name: notifications.cluster_name,
            notifications_reasons: notifications.reasons,
            notifications_min_interval: notifications.min_interval,
            notifications_channels: notifications
                .channels
                .map(|c| c.into_iter().map(|(k, v)| (k, v.reasons)).collect()),
        }
    }

    pub fn to_config(&self) -> Result<Config, ConfigError> {
        Config::from_raw(&self.to_raw())
    }
}

/// The comment block at the top of the chart's copy of the CRD. `crd_yaml`
/// prints it too, so regenerating the chart's file is a plain redirect.
const CRD_HEADER: &str = "\
# GENERATED by `k8s-reaper crd` (https://github.com/mbround18/k8s-reaper).
# Do not hand-edit. Regenerate from the k8s-reaper repo at the matching
# appVersion and replace this file:
#   cargo run -q -p k8s-reaper -- crd   (or: docker run --rm mbround18/k8s-reaper:<tag> crd)
# Helm installs files in crds/ on first install only and never upgrades them;
# use `kubectl apply -f` or Flux `install/upgrade.crds: CreateReplace`.
";

/// The CRD as YAML, as printed by `k8s-reaper crd` and shipped by the chart,
/// byte for byte: a `# GENERATED` header comment, then the CRD.
#[must_use]
pub fn crd_yaml() -> String {
    let body = serde_yaml_ng::to_string(&ReaperConfig::crd()).expect("CRD serialises");
    format!("{CRD_HEADER}{body}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reaper_core::config::Mode;

    use super::*;

    #[test]
    fn empty_spec_is_the_default_config() {
        let spec: ReaperConfigSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.to_config().unwrap(), Config::default());
    }

    #[test]
    fn nested_fields_reach_the_core() {
        let spec: ReaperConfigSpec = serde_json::from_value(serde_json::json!({
            "mode": "full",
            "detection": { "minRestarts": 4 },
            "scope": { "includeNamespaces": ["shop-*"] },
            "limits": { "maxReaped": 2 },
            "flux": { "helmRelease": false },
            "cronJobs": { "enabled": false, "failingDuration": "2h", "minFailedRuns": 5 },
            "notifications": {
                "clusterName": "prod",
                "reasons": ["ReapPlanned"],
                "minInterval": "30m",
                "channels": { "ops-discord": { "reasons": ["ReapPlanned"] } }
            }
        }))
        .unwrap();
        let c = spec.to_config().unwrap();
        assert_eq!(c.mode, Mode::Full);
        assert_eq!(c.detection.min_restarts, 4);
        assert_eq!(c.scope.include_namespaces, ["shop-*"]);
        assert_eq!(c.limits.max_reaped, 2);
        assert!(!c.flux.helm_release);
        assert!(!c.cron_jobs.enabled);
        assert_eq!(c.cron_jobs.failing_duration, Duration::from_hours(2));
        assert_eq!(c.cron_jobs.min_failed_runs, 5);
        assert_eq!(c.notifications.cluster_name, "prod");
        assert!(c.notifications.reasons.contains("ReapPlanned"));
        assert_eq!(c.notifications.min_interval, Duration::from_mins(30));
        assert!(c.notifications.channels["ops-discord"].contains("ReapPlanned"));
    }

    #[test]
    fn core_validation_still_applies() {
        let spec: ReaperConfigSpec = serde_json::from_value(
            serde_json::json!({ "detection": { "crashLoopDuration": "1m" } }),
        )
        .unwrap();
        let err = spec.to_config().unwrap_err();
        assert_eq!(err.errors[0].field, "spec.detection.crashLoopDuration");
    }

    #[test]
    fn unknown_mode_is_rejected_at_parse() {
        let parsed: Result<ReaperConfigSpec, _> =
            serde_json::from_value(serde_json::json!({ "mode": "off" }));
        assert!(parsed.is_err());
    }

    #[test]
    fn failure_rate_is_in_the_schema() {
        let crd = serde_json::to_value(ReaperConfig::crd()).unwrap();
        let fr = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
            ["cronJobs"]["properties"]["failureRate"]["properties"];
        assert_eq!(fr["enabled"]["type"], "boolean");
        assert_eq!(fr["windowRuns"]["type"], "integer");
        assert_eq!(fr["windowRuns"]["minimum"], 3.0);
        assert_eq!(fr["windowRuns"]["maximum"], 100.0);
        assert_eq!(fr["threshold"]["type"], "string");
        assert_eq!(fr["threshold"]["pattern"], "^(5[1-9]|[6-9][0-9]|100)%?$");
    }

    #[test]
    fn failure_rate_reaches_the_core() {
        let spec: ReaperConfigSpec = serde_json::from_value(serde_json::json!({
            "cronJobs": { "failureRate": { "enabled": false, "windowRuns": 5, "threshold": "90%" } }
        }))
        .unwrap();
        let fr = spec.to_config().unwrap().cron_jobs.failure_rate;
        assert!(!fr.enabled);
        assert_eq!(fr.window_runs, 5);
        assert_eq!(fr.threshold, 90);
        let spec: ReaperConfigSpec =
            serde_json::from_value(serde_json::json!({ "cronJobs": { "enabled": true } })).unwrap();
        assert_eq!(
            spec.to_config().unwrap().cron_jobs.failure_rate,
            reaper_core::config::FailureRate::default()
        );
    }
}
