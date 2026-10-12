//! Annotation codec (contracts/annotations.md, contracts/labels.md).
//!
//! The reaper keeps all of its state in `reaper.boop.ninja/*` annotations and
//! one label on the objects it touches. Decoding never panics: a malformed
//! value is reported and treated as absent.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::model::{NsName, WorkloadRef};
use crate::time::Timestamp;

pub const PREFIX: &str = "reaper.boop.ninja/";
pub const STATE_LABEL: &str = "reaper.boop.ninja/state";
pub const MODE_KEY: &str = "reaper.boop.ninja/mode";
pub const CRASHLOOP_SINCE: &str = "reaper.boop.ninja/crashloop-since";
pub const HEALTHY_SINCE: &str = "reaper.boop.ninja/healthy-since";
pub const PLAN: &str = "reaper.boop.ninja/plan";
pub const ORIGINAL_REPLICAS: &str = "reaper.boop.ninja/original-replicas";
pub const REASON: &str = "reaper.boop.ninja/reason";
pub const PREVIOUS_RECONCILE: &str = "reaper.boop.ninja/previous-reconcile";
pub const HELMRELEASE: &str = "reaper.boop.ninja/helmrelease";
pub const REAPED_AT: &str = "reaper.boop.ninja/reaped-at";
pub const RESTORE: &str = "reaper.boop.ninja/restore";
pub const HELD_FOR: &str = "reaper.boop.ninja/held-for";
pub const SUSPENDED_BY_REAPER: &str = "reaper.boop.ninja/suspended-by-reaper";
pub const FAILING_SINCE: &str = "reaper.boop.ninja/failing-since";
pub const FAILED_RUNS: &str = "reaper.boop.ninja/failed-runs";
pub const LAST_FAILED_JOB: &str = "reaper.boop.ninja/last-failed-job";
pub const RUN_HISTORY: &str = "reaper.boop.ninja/run-history";

// Person-set threshold overrides (FR-034). Never written or deleted by the reaper.
pub const CRASHLOOP_DURATION: &str = "reaper.boop.ninja/crashloop-duration";
pub const MIN_RESTARTS: &str = "reaper.boop.ninja/min-restarts";
pub const FAILING_DURATION: &str = "reaper.boop.ninja/failing-duration";
pub const MIN_FAILED_RUNS: &str = "reaper.boop.ninja/min-failed-runs";
pub const FAILURE_WINDOW: &str = "reaper.boop.ninja/failure-window";
pub const FAILURE_THRESHOLD: &str = "reaper.boop.ninja/failure-threshold";
pub const FAILURE_RATE: &str = "reaper.boop.ninja/failure-rate";

/// Keys under the prefix that belong to people (`restore` is person-set but
/// the reaper removes it once handled, so it is not listed here).
pub const PERSON_KEYS: [&str; 8] = [
    MODE_KEY,
    CRASHLOOP_DURATION,
    MIN_RESTARTS,
    FAILING_DURATION,
    MIN_FAILED_RUNS,
    FAILURE_WINDOW,
    FAILURE_THRESHOLD,
    FAILURE_RATE,
];

pub const FLUX_RECONCILE: &str = "kustomize.toolkit.fluxcd.io/reconcile";
pub const FLUX_RECONCILE_DISABLED: &str = "disabled";
const NONE_LITERAL: &str = "<none>";

/// True for keys the reaper owns and may delete. `mode` and the threshold
/// overrides are under the prefix but belong to people.
#[must_use]
pub fn is_owned_key(key: &str) -> bool {
    key.starts_with(PREFIX) && !PERSON_KEYS.contains(&key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Observed,
    Candidate,
    Reaping,
    Reaped,
}

impl State {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Observed => "observed",
            State::Candidate => "candidate",
            State::Reaping => "reaping",
            State::Reaped => "reaped",
        }
    }

    pub const ALL: [State; 4] = [
        State::Observed,
        State::Candidate,
        State::Reaping,
        State::Reaped,
    ];
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        State::ALL.into_iter().find(|st| st.as_str() == s).ok_or(())
    }
}

/// The value `kustomize.toolkit.fluxcd.io/reconcile` had before the reaper
/// set it. Encoded as the value itself, or `<none>` when it was absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviousReconcile {
    Absent,
    Value(String),
}

impl PreviousReconcile {
    #[must_use]
    pub fn from_current(current: Option<&str>) -> Self {
        current.map_or(PreviousReconcile::Absent, |v| {
            PreviousReconcile::Value(v.to_owned())
        })
    }

    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            PreviousReconcile::Absent => NONE_LITERAL.to_owned(),
            PreviousReconcile::Value(v) => v.clone(),
        }
    }

    fn decode(s: &str) -> Self {
        if s == NONE_LITERAL {
            PreviousReconcile::Absent
        } else {
            PreviousReconcile::Value(s.to_owned())
        }
    }

    /// The annotation value to put back on restore (`None` deletes it).
    #[must_use]
    pub fn restored_value(&self) -> Option<String> {
        match self {
            PreviousReconcile::Absent => None,
            PreviousReconcile::Value(v) => Some(v.clone()),
        }
    }
}

impl Serialize for PreviousReconcile {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.encode())
    }
}

impl<'de> Deserialize<'de> for PreviousReconcile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(PreviousReconcile::decode(&String::deserialize(d)?))
    }
}

/// A value that could not be decoded; it is treated as absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub key: String,
    pub value: String,
}

/// One finished Job of a CronJob, as recorded in `run-history`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRun {
    /// `None` only for a success inferred from `lastSuccessfulTime`.
    pub job: Option<String>,
    /// The Job's finish time.
    pub at: Timestamp,
    pub ok: bool,
}

impl RecordedRun {
    /// True when a run at `(at, job)` sorts after this one. A `None` name
    /// compares equal to any name at the same time (research R2, R4).
    #[must_use]
    pub fn is_before(&self, at: Timestamp, job: Option<&str>) -> bool {
        match (self.at.cmp(&at), self.job.as_deref(), job) {
            (std::cmp::Ordering::Equal, Some(mine), Some(theirs)) => mine < theirs,
            (o, _, _) => o == std::cmp::Ordering::Less,
        }
    }
}

/// The CronJob's recent finished runs, oldest first (FR-001). Invariants:
/// `runs` holds at least one failure and is strictly increasing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHistory {
    /// Finish time of the first recorded failure.
    pub since: Timestamp,
    pub runs: Vec<RecordedRun>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunHistoryWire {
    since: String,
    runs: Vec<RecordedRunWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedRunWire {
    job: Option<String>,
    at: String,
    ok: bool,
}

impl RunHistory {
    /// Failed runs in the window.
    #[must_use]
    pub fn failed(&self) -> u32 {
        u32::try_from(self.runs.iter().filter(|r| !r.ok).count()).unwrap_or(u32::MAX)
    }

    /// Compact JSON for the `run-history` annotation.
    #[must_use]
    pub fn encode(&self) -> String {
        let wire = RunHistoryWire {
            since: self.since.to_string(),
            runs: self
                .runs
                .iter()
                .map(|r| RecordedRunWire {
                    job: r.job.clone(),
                    at: r.at.to_string(),
                    ok: r.ok,
                })
                .collect(),
        };
        serde_json::to_string(&wire).expect("plain data serialises")
    }

    /// `None` when the value is not valid JSON, has unknown fields, or breaks
    /// an invariant.
    #[must_use]
    pub fn decode(s: &str) -> Option<Self> {
        let wire: RunHistoryWire = serde_json::from_str(s).ok()?;
        let runs = wire
            .runs
            .into_iter()
            .map(|r| {
                Some(RecordedRun {
                    at: Timestamp::parse_rfc3339(&r.at).ok()?,
                    job: r.job,
                    ok: r.ok,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let increasing = runs
            .windows(2)
            .all(|w| w[0].is_before(w[1].at, w[1].job.as_deref()));
        (increasing && runs.iter().any(|r| !r.ok)).then_some(())?;
        Some(RunHistory {
            since: Timestamp::parse_rfc3339(&wire.since).ok()?,
            runs,
        })
    }
}

/// Everything the reaper records on a workload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkloadRecord {
    pub state: Option<State>,
    pub crashloop_since: Option<Timestamp>,
    pub healthy_since: Option<Timestamp>,
    /// Raw plan JSON as last published.
    pub plan: Option<String>,
    pub original_replicas: Option<u32>,
    pub reason: Option<String>,
    pub previous_reconcile: Option<PreviousReconcile>,
    pub helm_release: Option<NsName>,
    pub reaped_at: Option<Timestamp>,
    /// `restore` is set to a non-empty value.
    pub restore: bool,
    /// CronJob only: start of the failing streak, distinct failed Jobs
    /// counted, and the last one counted (contracts/annotations.md).
    pub failing_since: Option<Timestamp>,
    pub failed_runs: Option<u32>,
    pub last_failed_job: Option<String>,
    /// CronJob only: recent runs for the failure-rate rule.
    pub run_history: Option<RunHistory>,
}

impl WorkloadRecord {
    #[must_use]
    pub fn decode(
        annotations: &BTreeMap<String, String>,
        state_label: Option<&str>,
    ) -> (Self, Vec<DecodeError>) {
        let mut errors = Vec::new();
        let mut bad = |key: &str, value: &str| {
            errors.push(DecodeError {
                key: key.to_owned(),
                value: value.to_owned(),
            });
        };
        let get = |k: &str| annotations.get(k).map(String::as_str);
        let time = |k: &str, bad: &mut dyn FnMut(&str, &str)| {
            get(k).and_then(|v| Timestamp::parse_rfc3339(v).map_err(|_| bad(k, v)).ok())
        };

        let state = state_label.and_then(|v| v.parse().map_err(|()| bad(STATE_LABEL, v)).ok());
        let crashloop_since = time(CRASHLOOP_SINCE, &mut bad);
        let healthy_since = time(HEALTHY_SINCE, &mut bad);
        let reaped_at = time(REAPED_AT, &mut bad);
        let failing_since = time(FAILING_SINCE, &mut bad);
        let failed_runs =
            get(FAILED_RUNS).and_then(|v| v.parse::<u32>().map_err(|_| bad(FAILED_RUNS, v)).ok());
        let original_replicas = get(ORIGINAL_REPLICAS)
            .and_then(|v| v.parse::<u32>().map_err(|_| bad(ORIGINAL_REPLICAS, v)).ok());
        let helm_release = get(HELMRELEASE)
            .and_then(|v| v.parse::<NsName>().map_err(|_| bad(HELMRELEASE, v)).ok());
        let run_history = get(RUN_HISTORY).and_then(|v| {
            let h = RunHistory::decode(v);
            if h.is_none() {
                bad(RUN_HISTORY, v);
            }
            h
        });
        let record = WorkloadRecord {
            state,
            crashloop_since,
            healthy_since,
            plan: get(PLAN).map(str::to_owned),
            original_replicas,
            reason: get(REASON).map(str::to_owned),
            previous_reconcile: get(PREVIOUS_RECONCILE).map(PreviousReconcile::decode),
            helm_release,
            reaped_at,
            restore: get(RESTORE).is_some_and(|v| !v.is_empty()),
            failing_since,
            failed_runs,
            last_failed_job: get(LAST_FAILED_JOB).map(str::to_owned),
            run_history,
        };
        (record, errors)
    }

    /// When the crash loop or failing streak began; for a CronJob, the
    /// earlier of its streak and its run history (003 FR-007).
    #[must_use]
    pub fn since(&self) -> Option<Timestamp> {
        let history = self.run_history.as_ref().map(|h| h.since);
        [self.crashloop_since.or(self.failing_since), history]
            .into_iter()
            .flatten()
            .min()
    }

    /// The owned annotations (and state label) that represent this record.
    #[must_use]
    pub fn encode(&self) -> (BTreeMap<String, String>, Option<String>) {
        let mut a = BTreeMap::new();
        let mut put = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                a.insert(k.to_owned(), v);
            }
        };
        put(CRASHLOOP_SINCE, self.crashloop_since.map(|t| t.to_string()));
        put(HEALTHY_SINCE, self.healthy_since.map(|t| t.to_string()));
        put(PLAN, self.plan.clone());
        put(
            ORIGINAL_REPLICAS,
            self.original_replicas.map(|n| n.to_string()),
        );
        put(REASON, self.reason.clone());
        put(
            PREVIOUS_RECONCILE,
            self.previous_reconcile
                .as_ref()
                .map(PreviousReconcile::encode),
        );
        put(
            HELMRELEASE,
            self.helm_release.as_ref().map(ToString::to_string),
        );
        put(REAPED_AT, self.reaped_at.map(|t| t.to_string()));
        put(RESTORE, self.restore.then(|| "true".to_owned()));
        put(FAILING_SINCE, self.failing_since.map(|t| t.to_string()));
        put(FAILED_RUNS, self.failed_runs.map(|n| n.to_string()));
        put(LAST_FAILED_JOB, self.last_failed_job.clone());
        put(
            RUN_HISTORY,
            self.run_history.as_ref().map(RunHistory::encode),
        );
        (a, self.state.map(|s| s.as_str().to_owned()))
    }
}

/// The reaper's hold on a HelmRelease.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelmReleaseHold {
    pub held_for: BTreeSet<WorkloadRef>,
    pub suspended_by_reaper: Option<bool>,
    pub previous_reconcile: Option<PreviousReconcile>,
}

impl HelmReleaseHold {
    #[must_use]
    pub fn decode(annotations: &BTreeMap<String, String>) -> (Self, Vec<DecodeError>) {
        let mut errors = Vec::new();
        let held_for = annotations.get(HELD_FOR).map_or_else(BTreeSet::new, |v| {
            serde_json::from_str::<Vec<String>>(v)
                .ok()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|i| i.parse::<WorkloadRef>().ok())
                        .collect::<Option<BTreeSet<_>>>()
                })
                .unwrap_or_else(|| {
                    errors.push(DecodeError {
                        key: HELD_FOR.into(),
                        value: v.clone(),
                    });
                    BTreeSet::new()
                })
        });
        let suspended_by_reaper =
            annotations
                .get(SUSPENDED_BY_REAPER)
                .and_then(|v| match v.as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => {
                        errors.push(DecodeError {
                            key: SUSPENDED_BY_REAPER.into(),
                            value: v.clone(),
                        });
                        None
                    }
                });
        let previous_reconcile = annotations
            .get(PREVIOUS_RECONCILE)
            .map(|v| PreviousReconcile::decode(v));
        (
            HelmReleaseHold {
                held_for,
                suspended_by_reaper,
                previous_reconcile,
            },
            errors,
        )
    }

    #[must_use]
    pub fn encode(&self) -> BTreeMap<String, String> {
        let mut a = BTreeMap::new();
        if !self.held_for.is_empty() {
            let items: Vec<String> = self.held_for.iter().map(ToString::to_string).collect();
            a.insert(
                HELD_FOR.to_owned(),
                serde_json::to_string(&items).expect("strings serialise"),
            );
        }
        if let Some(b) = self.suspended_by_reaper {
            a.insert(SUSPENDED_BY_REAPER.to_owned(), b.to_string());
        }
        if let Some(p) = &self.previous_reconcile {
            a.insert(PREVIOUS_RECONCILE.to_owned(), p.encode());
        }
        a
    }
}

/// A metadata merge patch: `Some(v)` sets a key, `None` deletes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataPatch {
    pub annotations: BTreeMap<String, Option<String>>,
    pub labels: BTreeMap<String, Option<String>>,
}

impl MetadataPatch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.annotations.is_empty() && self.labels.is_empty()
    }

    /// The patch that turns the current owned annotations and state label into
    /// `desired`. Keys the reaper does not own are never touched.
    #[must_use]
    pub fn between(
        current_annotations: &BTreeMap<String, String>,
        current_state_label: Option<&str>,
        desired_annotations: &BTreeMap<String, String>,
        desired_state_label: Option<&str>,
    ) -> Self {
        let mut p = MetadataPatch::default();
        for (k, v) in desired_annotations {
            if current_annotations.get(k) != Some(v) {
                p.annotations.insert(k.clone(), Some(v.clone()));
            }
        }
        for k in current_annotations.keys() {
            if is_owned_key(k) && !desired_annotations.contains_key(k) {
                p.annotations.insert(k.clone(), None);
            }
        }
        if current_state_label != desired_state_label {
            p.labels.insert(
                STATE_LABEL.to_owned(),
                desired_state_label.map(str::to_owned),
            );
        }
        p
    }

    /// True when every key in the patch is a `reaper.boop.ninja/*` key.
    #[must_use]
    pub fn only_reaper_keys(&self) -> bool {
        self.annotations
            .keys()
            .chain(self.labels.keys())
            .all(|k| k.starts_with(PREFIX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorkloadKind;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn full_record() -> WorkloadRecord {
        WorkloadRecord {
            state: Some(State::Reaped),
            crashloop_since: Some(Timestamp::parse_rfc3339("2026-10-10T08:00:00Z").unwrap()),
            healthy_since: Some(Timestamp::parse_rfc3339("2026-10-10T09:00:00Z").unwrap()),
            plan: Some(r#"{"actions":["markReaped"]}"#.into()),
            original_replicas: Some(2),
            reason: Some(r#"container "api" CrashLoopBackOff"#.into()),
            previous_reconcile: Some(PreviousReconcile::Absent),
            helm_release: Some(NsName::new("helm", "web")),
            reaped_at: Some(Timestamp::parse_rfc3339("2026-10-10T10:00:00Z").unwrap()),
            restore: true,
            failing_since: Some(Timestamp::parse_rfc3339("2026-10-10T02:00:00Z").unwrap()),
            failed_runs: Some(7),
            last_failed_job: Some("ingest-29340000".into()),
            run_history: Some(history()),
        }
    }

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse_rfc3339(s).unwrap()
    }

    fn run(job: Option<&str>, at: &str, ok: bool) -> RecordedRun {
        RecordedRun {
            job: job.map(str::to_owned),
            at: ts(at),
            ok,
        }
    }

    fn history() -> RunHistory {
        RunHistory {
            since: ts("2026-10-12T02:00:53Z"),
            runs: vec![
                run(Some("backup-29340000"), "2026-10-12T02:00:53Z", false),
                run(None, "2026-10-13T02:00:40Z", true),
            ],
        }
    }

    #[test]
    fn workload_record_round_trips_every_key() {
        let r = full_record();
        let (a, label) = r.encode();
        for key in [
            CRASHLOOP_SINCE,
            HEALTHY_SINCE,
            PLAN,
            ORIGINAL_REPLICAS,
            REASON,
            PREVIOUS_RECONCILE,
            HELMRELEASE,
            REAPED_AT,
            RESTORE,
            FAILING_SINCE,
            FAILED_RUNS,
            LAST_FAILED_JOB,
            RUN_HISTORY,
        ] {
            assert!(a.contains_key(key), "{key}");
        }
        assert_eq!(a[PREVIOUS_RECONCILE], "<none>");
        assert_eq!(a[CRASHLOOP_SINCE], "2026-10-10T08:00:00Z");
        assert_eq!(label.as_deref(), Some("reaped"));
        let (back, errors) = WorkloadRecord::decode(&a, label.as_deref());
        assert_eq!(errors, Vec::new());
        assert_eq!(back, r);
    }

    #[test]
    fn previous_reconcile_keeps_a_real_value() {
        let a = map(&[(PREVIOUS_RECONCILE, "disabled")]);
        let (r, _) = WorkloadRecord::decode(&a, None);
        assert_eq!(
            r.previous_reconcile,
            Some(PreviousReconcile::Value("disabled".into()))
        );
        assert_eq!(
            r.previous_reconcile.unwrap().restored_value().as_deref(),
            Some("disabled")
        );
        assert_eq!(PreviousReconcile::Absent.restored_value(), None);
    }

    #[test]
    fn malformed_values_are_reported_not_panicked() {
        let a = map(&[
            (CRASHLOOP_SINCE, "yesterday"),
            (ORIGINAL_REPLICAS, "-1"),
            (HELMRELEASE, "no-slash"),
            (REAPED_AT, ""),
            (RESTORE, ""),
        ]);
        let (r, errors) = WorkloadRecord::decode(&a, Some("zombie"));
        assert_eq!(r, WorkloadRecord::default());
        let keys: Vec<_> = errors.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                STATE_LABEL,
                CRASHLOOP_SINCE,
                REAPED_AT,
                ORIGINAL_REPLICAS,
                HELMRELEASE
            ]
        );
    }

    #[test]
    fn hold_round_trips_sorted_and_deduplicated() {
        let mut hold = HelmReleaseHold {
            held_for: BTreeSet::new(),
            suspended_by_reaper: Some(true),
            previous_reconcile: Some(PreviousReconcile::Value("enabled".into())),
        };
        hold.held_for
            .insert(WorkloadRef::new(WorkloadKind::StatefulSet, "helm", "db"));
        hold.held_for
            .insert(WorkloadRef::new(WorkloadKind::Deployment, "helm", "web"));
        hold.held_for
            .insert(WorkloadRef::new(WorkloadKind::Deployment, "helm", "web"));
        let a = hold.encode();
        assert_eq!(
            a[HELD_FOR],
            r#"["Deployment/helm/web","StatefulSet/helm/db"]"#
        );
        assert_eq!(a[SUSPENDED_BY_REAPER], "true");
        let (back, errors) = HelmReleaseHold::decode(&a);
        assert_eq!(errors, Vec::new());
        assert_eq!(back, hold);
    }

    #[test]
    fn malformed_hold_is_reported() {
        let a = map(&[(HELD_FOR, "[\"Job/x/y\"]"), (SUSPENDED_BY_REAPER, "yes")]);
        let (hold, errors) = HelmReleaseHold::decode(&a);
        assert_eq!(hold, HelmReleaseHold::default());
        assert_eq!(errors.len(), 2);
        let (_, errors) = HelmReleaseHold::decode(&map(&[(HELD_FOR, "not json")]));
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn person_set_keys_are_not_owned() {
        for key in [
            MODE_KEY,
            CRASHLOOP_DURATION,
            MIN_RESTARTS,
            FAILING_DURATION,
            MIN_FAILED_RUNS,
            FAILURE_WINDOW,
            FAILURE_THRESHOLD,
            FAILURE_RATE,
        ] {
            assert!(!is_owned_key(key), "{key}");
        }
        assert!(is_owned_key(RUN_HISTORY));
        assert!(is_owned_key(RESTORE));
        assert!(is_owned_key(PLAN));
        assert!(!is_owned_key("example.com/other"));
    }

    #[test]
    fn patch_between_touches_only_owned_keys() {
        let current = map(&[
            (CRASHLOOP_SINCE, "2026-10-10T08:00:00Z"),
            (HEALTHY_SINCE, "2026-10-10T08:05:00Z"),
            (MODE_KEY, "report"),
            ("reaper.boop.ninja/unknown", "x"),
        ]);
        let desired = map(&[(CRASHLOOP_SINCE, "2026-10-10T08:00:00Z"), (PLAN, "{}")]);
        let p = MetadataPatch::between(&current, None, &desired, Some("candidate"));
        assert_eq!(p.annotations.get(PLAN), Some(&Some("{}".to_owned())));
        assert_eq!(p.annotations.get(HEALTHY_SINCE), Some(&None));
        assert_eq!(p.annotations.get("reaper.boop.ninja/unknown"), Some(&None));
        assert!(!p.annotations.contains_key(MODE_KEY));
        assert!(!p.annotations.contains_key(CRASHLOOP_SINCE));
        assert_eq!(
            p.labels.get(STATE_LABEL),
            Some(&Some("candidate".to_owned()))
        );
        assert!(p.only_reaper_keys());
        assert!(MetadataPatch::between(&desired, Some("x"), &desired, Some("x")).is_empty());
    }

    #[test]
    fn run_history_encodes_compact_json() {
        assert_eq!(
            history().encode(),
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"backup-29340000","at":"2026-10-12T02:00:53Z","ok":false},{"job":null,"at":"2026-10-13T02:00:40Z","ok":true}]}"#
        );
        assert_eq!(RunHistory::decode(&history().encode()), Some(history()));
        assert_eq!(history().failed(), 1);
    }

    #[test]
    fn run_history_absent_is_not_written() {
        let r = WorkloadRecord {
            run_history: None,
            ..full_record()
        };
        assert!(!r.encode().0.contains_key(RUN_HISTORY));
    }

    #[test]
    fn unreadable_run_history_is_reported_and_absent() {
        for bad in [
            "not json",
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"a","at":"2026-10-12T02:00:53Z","ok":false}],"extra":1}"#,
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"a","at":"2026-10-12T02:00:53Z","ok":false,"x":1}]}"#,
            r#"{"since":"yesterday","runs":[{"job":"a","at":"2026-10-12T02:00:53Z","ok":false}]}"#,
            // empty, or no failure in the window
            r#"{"since":"2026-10-12T02:00:53Z","runs":[]}"#,
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"a","at":"2026-10-12T02:00:53Z","ok":true}]}"#,
            // not strictly increasing by (at, job)
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"b","at":"2026-10-12T02:00:53Z","ok":false},{"job":"a","at":"2026-10-12T02:00:53Z","ok":false}]}"#,
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"a","at":"2026-10-12T03:00:00Z","ok":false},{"job":"b","at":"2026-10-12T02:00:53Z","ok":false}]}"#,
            r#"{"since":"2026-10-12T02:00:53Z","runs":[{"job":"a","at":"2026-10-12T02:00:53Z","ok":false},{"job":null,"at":"2026-10-12T02:00:53Z","ok":true}]}"#,
        ] {
            let (r, errors) = WorkloadRecord::decode(&map(&[(RUN_HISTORY, bad)]), None);
            assert_eq!(r.run_history, None, "{bad}");
            assert_eq!(errors.len(), 1, "{bad}");
            assert_eq!(errors[0].key, RUN_HISTORY);
        }
    }

    #[test]
    fn recorded_run_ordering_treats_null_job_as_equal() {
        let r = run(Some("b"), "2026-10-12T02:00:00Z", false);
        assert!(r.is_before(ts("2026-10-12T02:00:01Z"), Some("a")));
        assert!(r.is_before(ts("2026-10-12T02:00:00Z"), Some("c")));
        assert!(!r.is_before(ts("2026-10-12T02:00:00Z"), Some("b")));
        assert!(!r.is_before(ts("2026-10-12T02:00:00Z"), Some("a")));
        assert!(!r.is_before(ts("2026-10-12T02:00:00Z"), None));
        let n = run(None, "2026-10-12T02:00:00Z", true);
        assert!(!n.is_before(ts("2026-10-12T02:00:00Z"), Some("z")));
        assert!(n.is_before(ts("2026-10-12T02:00:01Z"), None));
    }
}
