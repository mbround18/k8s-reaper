//! Runtime configuration: defaults, parsing and validation (data-model.md).

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::time::{format_duration, parse_duration};

/// Effective operating mode. Ordered so the most restrictive is the smallest:
/// `Off < Report < Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Report,
    Full,
}

impl Mode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Full => "full",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Global mode: `Report` or `Full` (never `Off`).
    pub mode: Mode,
    pub cycle_interval: Duration,
    pub detection: Detection,
    pub scope: Scope,
    pub limits: Limits,
    pub flux: FluxToggles,
    pub cron_jobs: CronJobs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub crash_loop_duration: Duration,
    pub min_restarts: u32,
    pub recovery_window: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub exclude_namespaces: Vec<String>,
    pub include_namespaces: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_reaps_per_cycle: u32,
    pub max_reaped: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FluxToggles {
    pub kustomize: bool,
    pub helm_release: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronJobs {
    pub enabled: bool,
    pub failing_duration: Duration,
    pub min_failed_runs: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::Report,
            cycle_interval: Duration::from_mins(1),
            detection: Detection {
                crash_loop_duration: Duration::from_mins(30),
                min_restarts: 10,
                recovery_window: Duration::from_mins(10),
            },
            scope: Scope {
                exclude_namespaces: [
                    "kube-system",
                    "kube-public",
                    "kube-node-lease",
                    "flux-system",
                ]
                .map(String::from)
                .to_vec(),
                include_namespaces: Vec::new(),
            },
            limits: Limits {
                max_reaps_per_cycle: 1,
                max_reaped: 5,
            },
            flux: FluxToggles {
                kustomize: true,
                helm_release: true,
            },
            cron_jobs: CronJobs {
                enabled: true,
                failing_duration: Duration::from_hours(6),
                min_failed_runs: 3,
            },
        }
    }
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// Configuration as it arrives from outside (the CRD spec): every field
/// optional, durations as strings. `Config::from_raw` fills defaults and
/// reports every invalid field at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawConfig {
    pub mode: Option<String>,
    pub cycle_interval: Option<String>,
    pub crash_loop_duration: Option<String>,
    pub min_restarts: Option<i64>,
    pub recovery_window: Option<String>,
    pub exclude_namespaces: Option<Vec<String>>,
    pub include_namespaces: Option<Vec<String>>,
    pub max_reaps_per_cycle: Option<i64>,
    pub max_reaped: Option<i64>,
    pub flux_kustomize: Option<bool>,
    pub flux_helm_release: Option<bool>,
    pub cron_jobs_enabled: Option<bool>,
    pub cron_jobs_failing_duration: Option<String>,
    pub cron_jobs_min_failed_runs: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// CRD field path, e.g. `spec.detection.minRestarts`.
    pub field: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid configuration: {}", .errors.iter().map(|e| format!("{}: {}", e.field, e.message)).collect::<Vec<_>>().join("; "))]
pub struct ConfigError {
    pub errors: Vec<FieldError>,
}

impl Config {
    pub fn from_raw(raw: &RawConfig) -> Result<Config, ConfigError> {
        let mut errors = Vec::new();
        let mut c = Config::default();

        match raw.mode.as_deref() {
            None | Some("report") => c.mode = Mode::Report,
            Some("full") => c.mode = Mode::Full,
            Some(other) => errors.push(FieldError {
                field: "spec.mode",
                message: format!("must be \"report\" or \"full\", got {other:?}"),
            }),
        }
        duration_field(
            &mut errors,
            "spec.cycleInterval",
            raw.cycle_interval.as_deref(),
            (10, 10 * MINUTE),
            &mut c.cycle_interval,
        );
        duration_field(
            &mut errors,
            "spec.detection.crashLoopDuration",
            raw.crash_loop_duration.as_deref(),
            (5 * MINUTE, 7 * DAY),
            &mut c.detection.crash_loop_duration,
        );
        int_field(
            &mut errors,
            "spec.detection.minRestarts",
            raw.min_restarts,
            (1, 10_000),
            &mut c.detection.min_restarts,
        );
        duration_field(
            &mut errors,
            "spec.detection.recoveryWindow",
            raw.recovery_window.as_deref(),
            (MINUTE, DAY),
            &mut c.detection.recovery_window,
        );
        list_field(
            &mut errors,
            "spec.scope.excludeNamespaces",
            raw.exclude_namespaces.as_ref(),
            &mut c.scope.exclude_namespaces,
        );
        list_field(
            &mut errors,
            "spec.scope.includeNamespaces",
            raw.include_namespaces.as_ref(),
            &mut c.scope.include_namespaces,
        );
        int_field(
            &mut errors,
            "spec.limits.maxReapsPerCycle",
            raw.max_reaps_per_cycle,
            (0, 50),
            &mut c.limits.max_reaps_per_cycle,
        );
        int_field(
            &mut errors,
            "spec.limits.maxReaped",
            raw.max_reaped,
            (0, 500),
            &mut c.limits.max_reaped,
        );
        if let Some(v) = raw.flux_kustomize {
            c.flux.kustomize = v;
        }
        if let Some(v) = raw.flux_helm_release {
            c.flux.helm_release = v;
        }
        if let Some(v) = raw.cron_jobs_enabled {
            c.cron_jobs.enabled = v;
        }
        duration_field(
            &mut errors,
            "spec.cronJobs.failingDuration",
            raw.cron_jobs_failing_duration.as_deref(),
            (10 * MINUTE, 7 * DAY),
            &mut c.cron_jobs.failing_duration,
        );
        int_field(
            &mut errors,
            "spec.cronJobs.minFailedRuns",
            raw.cron_jobs_min_failed_runs,
            (1, 1000),
            &mut c.cron_jobs.min_failed_runs,
        );

        if errors.is_empty() {
            Ok(c)
        } else {
            Err(ConfigError { errors })
        }
    }

    /// Renders back to the raw form (used to show the active config).
    #[must_use]
    pub fn to_raw(&self) -> RawConfig {
        RawConfig {
            mode: Some(self.mode.as_str().into()),
            cycle_interval: Some(format_duration(self.cycle_interval)),
            crash_loop_duration: Some(format_duration(self.detection.crash_loop_duration)),
            min_restarts: Some(self.detection.min_restarts.into()),
            recovery_window: Some(format_duration(self.detection.recovery_window)),
            exclude_namespaces: Some(self.scope.exclude_namespaces.clone()),
            include_namespaces: Some(self.scope.include_namespaces.clone()),
            max_reaps_per_cycle: Some(self.limits.max_reaps_per_cycle.into()),
            max_reaped: Some(self.limits.max_reaped.into()),
            flux_kustomize: Some(self.flux.kustomize),
            flux_helm_release: Some(self.flux.helm_release),
            cron_jobs_enabled: Some(self.cron_jobs.enabled),
            cron_jobs_failing_duration: Some(format_duration(self.cron_jobs.failing_duration)),
            cron_jobs_min_failed_runs: Some(self.cron_jobs.min_failed_runs.into()),
        }
    }
}

/// Parses a `reaper.boop.ninja/mode` label or annotation value. Only `off`
/// and `report` are valid: a label can never raise the mode.
#[must_use]
pub fn parse_mode_override(value: &str) -> Option<Mode> {
    match value {
        "off" => Some(Mode::Off),
        "report" => Some(Mode::Report),
        _ => None,
    }
}

fn list_field(
    errors: &mut Vec<FieldError>,
    field: &'static str,
    value: Option<&Vec<String>>,
    target: &mut Vec<String>,
) {
    let Some(list) = value else { return };
    if list.iter().any(String::is_empty) {
        errors.push(FieldError {
            field,
            message: "entries must not be empty".into(),
        });
    } else {
        target.clone_from(list);
    }
}

fn duration_field(
    errors: &mut Vec<FieldError>,
    field: &'static str,
    value: Option<&str>,
    (min, max): (u64, u64),
    target: &mut Duration,
) {
    let Some(value) = value else { return };
    match parse_duration(value) {
        Ok(d) if (min..=max).contains(&d.as_secs()) => *target = d,
        Ok(_) => errors.push(FieldError {
            field,
            message: format!(
                "{value} is out of range {}..{}",
                format_duration(Duration::from_secs(min)),
                format_duration(Duration::from_secs(max))
            ),
        }),
        Err(e) => errors.push(FieldError {
            field,
            message: e.to_string(),
        }),
    }
}

fn int_field(
    errors: &mut Vec<FieldError>,
    field: &'static str,
    value: Option<i64>,
    (min, max): (u32, u32),
    target: &mut u32,
) {
    let Some(value) = value else { return };
    match u32::try_from(value) {
        Ok(v) if (min..=max).contains(&v) => *target = v,
        _ => errors.push(FieldError {
            field,
            message: format!("{value} is out of range {min}..{max}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn defaults_match_data_model() {
        let c = Config::default();
        assert_eq!(c.mode, Mode::Report);
        assert_eq!(c.cycle_interval, secs(60));
        assert_eq!(c.detection.crash_loop_duration, secs(30 * 60));
        assert_eq!(c.detection.min_restarts, 10);
        assert_eq!(c.detection.recovery_window, secs(10 * 60));
        assert_eq!(c.limits.max_reaps_per_cycle, 1);
        assert_eq!(c.limits.max_reaped, 5);
        assert_eq!(
            c.scope.exclude_namespaces,
            [
                "kube-system",
                "kube-public",
                "kube-node-lease",
                "flux-system"
            ]
        );
        assert_eq!(c.scope.include_namespaces, Vec::<String>::new());
        assert!(c.flux.kustomize && c.flux.helm_release);
    }

    #[test]
    fn empty_raw_is_defaults() {
        assert_eq!(
            Config::from_raw(&RawConfig::default()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn raw_values_override_defaults() {
        let raw = RawConfig {
            mode: Some("full".into()),
            cycle_interval: Some("90s".into()),
            crash_loop_duration: Some("1h30m".into()),
            min_restarts: Some(3),
            recovery_window: Some("1m".into()),
            exclude_namespaces: Some(vec![]),
            include_namespaces: Some(vec!["slam-*".into()]),
            max_reaps_per_cycle: Some(0),
            max_reaped: Some(500),
            flux_kustomize: Some(false),
            flux_helm_release: Some(false),
            ..RawConfig::default()
        };
        let c = Config::from_raw(&raw).unwrap();
        assert_eq!(c.mode, Mode::Full);
        assert_eq!(c.cycle_interval, secs(90));
        assert_eq!(c.detection.crash_loop_duration, secs(5400));
        assert_eq!(c.detection.min_restarts, 3);
        assert_eq!(c.detection.recovery_window, secs(60));
        assert_eq!(c.scope.exclude_namespaces, Vec::<String>::new());
        assert_eq!(c.scope.include_namespaces, ["slam-*"]);
        assert_eq!(c.limits.max_reaps_per_cycle, 0);
        assert_eq!(c.limits.max_reaped, 500);
        assert!(!c.flux.kustomize && !c.flux.helm_release);
        assert_eq!(Config::from_raw(&c.to_raw()).unwrap(), c);
    }

    #[test]
    fn every_invalid_field_is_reported() {
        let raw = RawConfig {
            mode: Some("off".into()),
            cycle_interval: Some("5s".into()),
            crash_loop_duration: Some("8d".into()),
            min_restarts: Some(0),
            recovery_window: Some("25h".into()),
            max_reaps_per_cycle: Some(51),
            max_reaped: Some(-1),
            include_namespaces: Some(vec![String::new()]),
            ..RawConfig::default()
        };
        let err = Config::from_raw(&raw).unwrap_err();
        let fields: Vec<_> = err.errors.iter().map(|e| e.field).collect();
        assert_eq!(
            fields,
            [
                "spec.mode",
                "spec.cycleInterval",
                "spec.detection.crashLoopDuration",
                "spec.detection.minRestarts",
                "spec.detection.recoveryWindow",
                "spec.scope.includeNamespaces",
                "spec.limits.maxReapsPerCycle",
                "spec.limits.maxReaped",
            ]
        );
        assert!(err.to_string().contains("spec.mode"));
    }

    #[test]
    fn cronjob_settings() {
        let c = Config::default();
        assert!(c.cron_jobs.enabled);
        assert_eq!(c.cron_jobs.failing_duration, Duration::from_hours(6));
        assert_eq!(c.cron_jobs.min_failed_runs, 3);
        let raw = RawConfig {
            cron_jobs_enabled: Some(false),
            cron_jobs_failing_duration: Some("10m".into()),
            cron_jobs_min_failed_runs: Some(1000),
            ..RawConfig::default()
        };
        let c = Config::from_raw(&raw).unwrap();
        assert!(!c.cron_jobs.enabled);
        assert_eq!(c.cron_jobs.failing_duration, Duration::from_mins(10));
        assert_eq!(c.cron_jobs.min_failed_runs, 1000);
        assert_eq!(Config::from_raw(&c.to_raw()).unwrap(), c);
        let raw = RawConfig {
            cron_jobs_failing_duration: Some("9m".into()),
            cron_jobs_min_failed_runs: Some(0),
            ..RawConfig::default()
        };
        let fields: Vec<_> = Config::from_raw(&raw)
            .unwrap_err()
            .errors
            .iter()
            .map(|e| e.field)
            .collect();
        assert_eq!(
            fields,
            [
                "spec.cronJobs.failingDuration",
                "spec.cronJobs.minFailedRuns"
            ]
        );
    }

    #[test]
    fn range_bounds_are_inclusive() {
        let ok = RawConfig {
            cycle_interval: Some("10s".into()),
            crash_loop_duration: Some("5m".into()),
            min_restarts: Some(10_000),
            recovery_window: Some("24h".into()),
            ..RawConfig::default()
        };
        assert!(Config::from_raw(&ok).is_ok());
        let too_small = RawConfig {
            crash_loop_duration: Some("299s".into()),
            ..RawConfig::default()
        };
        assert!(Config::from_raw(&too_small).is_err());
        let too_big = RawConfig {
            cycle_interval: Some("10m1s".into()),
            ..RawConfig::default()
        };
        assert!(Config::from_raw(&too_big).is_err());
    }

    #[test]
    fn mode_override_only_lowers() {
        assert_eq!(parse_mode_override("off"), Some(Mode::Off));
        assert_eq!(parse_mode_override("report"), Some(Mode::Report));
        assert_eq!(parse_mode_override("full"), None);
        assert_eq!(parse_mode_override("Off"), None);
        assert_eq!(parse_mode_override(""), None);
        assert!(Mode::Off < Mode::Report && Mode::Report < Mode::Full);
    }

    #[test]
    fn durations_render_back() {
        assert_eq!(
            Config::default().to_raw().crash_loop_duration.as_deref(),
            Some("30m")
        );
        let _ = format_duration(secs(1));
    }
}
