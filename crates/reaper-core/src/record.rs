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

pub const FLUX_RECONCILE: &str = "kustomize.toolkit.fluxcd.io/reconcile";
pub const FLUX_RECONCILE_DISABLED: &str = "disabled";
const NONE_LITERAL: &str = "<none>";

/// True for keys the reaper owns and may delete. `mode` is under the prefix
/// but belongs to people.
#[must_use]
pub fn is_owned_key(key: &str) -> bool {
    key.starts_with(PREFIX) && key != MODE_KEY
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
        let original_replicas = get(ORIGINAL_REPLICAS)
            .and_then(|v| v.parse::<u32>().map_err(|_| bad(ORIGINAL_REPLICAS, v)).ok());
        let helm_release = get(HELMRELEASE)
            .and_then(|v| v.parse::<NsName>().map_err(|_| bad(HELMRELEASE, v)).ok());
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
        };
        (record, errors)
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
}
