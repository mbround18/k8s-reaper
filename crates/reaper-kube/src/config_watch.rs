//! Reads the `ReaperConfig` every cycle and writes its status (FR-024,
//! FR-026; contracts/config.md).
//!
//! One GET per cycle rather than a watch: a change is picked up by the next
//! cycle, which is all US4 asks, and there is no extra cache to keep in sync.
//!
//! The status has two writers (002 R3): the cycle loop writes all of it with
//! [`ConfigSource::write_status`], and a status task writes only the `Ready`
//! condition with [`ReadyWriter::write_ready`], so a hung loop still shows as
//! `Stalled`.

use std::sync::{Arc, Mutex, PoisonError};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::jiff;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};
use reaper_core::Timestamp;
use reaper_core::config_state::{ConfigChange, ConfigState, Counts};
use reaper_core::ports::Readiness;
use serde_json::json;

use crate::crd::{ReaperConfig, ReaperConfigStatus, StateCounts};

pub const VALID: &str = "Valid";
pub const READY: &str = "Ready";

/// The status last written, shared by both writers.
type LastStatus = Arc<Mutex<Option<ReaperConfigStatus>>>;

pub struct ConfigSource {
    api: Api<ReaperConfig>,
    name: String,
    state: ConfigState,
    /// The conditions on the object, so a transition time survives a status
    /// rewrite that does not change the condition.
    conditions: Vec<Condition>,
    last: LastStatus,
}

impl ConfigSource {
    #[must_use]
    pub fn new(client: Client, name: impl Into<String>) -> Self {
        Self {
            api: Api::all(client),
            name: name.into(),
            state: ConfigState::default(),
            conditions: Vec::new(),
            last: LastStatus::default(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn state(&self) -> &ConfigState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut ConfigState {
        &mut self.state
    }

    /// A handle that writes the `Ready` condition from another task.
    #[must_use]
    pub fn ready_writer(&self) -> ReadyWriter {
        ReadyWriter {
            api: self.api.clone(),
            name: self.name.clone(),
            last: Arc::clone(&self.last),
        }
    }

    /// Reads the object. On an error nothing changes and the active config
    /// stays in use.
    pub async fn refresh(&mut self) -> Result<Option<ConfigChange>, kube::Error> {
        let object = self.api.get_opt(&self.name).await?;
        let seen = object.map(|o| {
            self.conditions = o.status.map(|s| s.conditions).unwrap_or_default();
            (
                o.metadata.generation.unwrap_or_default(),
                o.spec.to_config(),
            )
        });
        if seen.is_none() {
            self.conditions.clear();
            // Nothing to write `Ready` to until the loop writes a status.
            *lock(&self.last) = None;
        }
        Ok(self.state.observe(seen))
    }

    /// Writes the status subresource. Does nothing while no object exists.
    pub async fn write_status(
        &mut self,
        counts: Counts,
        last_cycle: Timestamp,
        readiness: Readiness,
    ) -> Result<(), kube::Error> {
        if self.state.observed_generation().is_none() {
            return Ok(());
        }
        let status = status(&self.state, counts, last_cycle, readiness, &self.conditions);
        self.api
            .patch_status(
                &self.name,
                &PatchParams::default(),
                &Patch::Merge(json!({ "status": status })),
            )
            .await?;
        self.conditions.clone_from(&status.conditions);
        *lock(&self.last) = Some(status);
        Ok(())
    }
}

/// Writes only the `Ready` condition, from a task other than the cycle loop.
/// Clones share the last written status with the [`ConfigSource`] they came
/// from.
#[derive(Clone)]
pub struct ReadyWriter {
    api: Api<ReaperConfig>,
    name: String,
    last: LastStatus,
}

impl ReadyWriter {
    /// Re-writes the last written status with only `Ready` changed, at `now`.
    /// Does nothing when its reason is unchanged.
    ///
    /// Before the loop has written a status (at start-up, or while the first
    /// cycle hangs), the status on the object stands in for it, so `Ready`
    /// leaves the previous pod's value; with no object, or no status on it,
    /// there is nothing to write.
    ///
    /// Only `conditions` is patched, so the counts and `lastCycleTime` the
    /// loop wrote are left alone.
    pub async fn write_ready(
        &self,
        readiness: Readiness,
        now: Timestamp,
    ) -> Result<(), kube::Error> {
        let written = lock(&self.last).clone();
        let last = if let Some(last) = written {
            last
        } else {
            let on_object = self.api.get_opt(&self.name).await?.and_then(|o| o.status);
            let Some(on_object) = on_object else {
                return Ok(());
            };
            on_object
        };
        let Some(next) = with_ready(&last, readiness, now) else {
            lock(&self.last).get_or_insert(last);
            return Ok(());
        };
        // The lock is not held across the request: the loop must never wait
        // on this task.
        self.api
            .patch_status(
                &self.name,
                &PatchParams::default(),
                &Patch::Merge(json!({ "status": { "conditions": next.conditions } })),
            )
            .await?;
        // If the loop wrote a newer status meanwhile, keep that one.
        let mut stored = lock(&self.last);
        if stored.as_ref().is_none_or(|s| *s == last) {
            *stored = Some(next);
        }
        Ok(())
    }
}

fn lock(last: &LastStatus) -> std::sync::MutexGuard<'_, Option<ReaperConfigStatus>> {
    last.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `last` with the `Ready` condition for `readiness`, or `None` when its
/// reason would not change. The transition time moves only with the status.
#[must_use]
pub fn with_ready(
    last: &ReaperConfigStatus,
    readiness: Readiness,
    now: Timestamp,
) -> Option<ReaperConfigStatus> {
    let previous = last.conditions.iter().find(|c| c.type_ == READY);
    if previous.is_some_and(|p| p.reason == readiness.as_str()) {
        return None;
    }
    let mut ready = ready_condition(readiness);
    ready.observed_generation = last.observed_generation;
    ready.last_transition_time = previous
        .filter(|p| p.status == ready.status)
        .map_or_else(|| time(now), |p| p.last_transition_time.clone());
    let mut next = last.clone();
    next.conditions.retain(|c| c.type_ != READY);
    next.conditions.push(ready);
    Some(next)
}

/// The status for `state` after a cycle that finished at `now`.
#[must_use]
pub fn status(
    state: &ConfigState,
    counts: Counts,
    now: Timestamp,
    readiness: Readiness,
    previous: &[Condition],
) -> ReaperConfigStatus {
    let observed = state.observed_generation();
    let valid = match state.error() {
        None => condition(VALID, true, "Valid", String::new()),
        Some(err) => condition(VALID, false, "Invalid", err.to_string()),
    };
    let conditions = [valid, ready_condition(readiness)]
        .into_iter()
        .map(|mut c| {
            c.observed_generation = observed;
            c.last_transition_time = previous
                .iter()
                .find(|p| p.type_ == c.type_ && p.status == c.status)
                .map_or_else(|| time(now), |p| p.last_transition_time.clone());
            c
        })
        .collect();
    ReaperConfigStatus {
        observed_generation: observed,
        active_generation: state.active_generation(),
        last_cycle_time: Some(now.to_string()),
        counts: Some(StateCounts {
            observed: counts.observed,
            candidate: counts.candidate,
            reaping: counts.reaping,
            reaped: counts.reaped,
            skipped: counts.skipped,
        }),
        conditions,
    }
}

/// `Ready`: `True/CycleCompleted`, `False/Stalled` or `False/CachesNotSynced`
/// (data-model.md, "Readiness"). `NotReady` is no longer written.
fn ready_condition(readiness: Readiness) -> Condition {
    condition(
        READY,
        readiness.is_ready(),
        readiness.as_str(),
        String::new(),
    )
}

fn condition(type_: &str, ok: bool, reason: &str, message: String) -> Condition {
    Condition {
        type_: type_.to_owned(),
        status: if ok { "True" } else { "False" }.to_owned(),
        reason: reason.to_owned(),
        message,
        observed_generation: None,
        last_transition_time: Time(jiff::Timestamp::UNIX_EPOCH),
    }
}

fn time(t: Timestamp) -> Time {
    Time(jiff::Timestamp::from_second(t.0).unwrap_or(jiff::Timestamp::UNIX_EPOCH))
}

#[cfg(test)]
mod tests {
    use reaper_core::config::{Config, ConfigError, FieldError};
    use reaper_core::ports::Readiness;

    use super::*;

    fn invalid() -> ConfigError {
        ConfigError {
            errors: vec![FieldError {
                field: "spec.cycleInterval",
                message: "must be between 10s and 10m".into(),
            }],
        }
    }

    fn of<'a>(s: &'a ReaperConfigStatus, type_: &str) -> &'a Condition {
        s.conditions.iter().find(|c| c.type_ == type_).unwrap()
    }

    #[test]
    fn valid_and_ready_with_counts() {
        let mut state = ConfigState::default();
        state.observe(Some((3, Ok(Config::default()))));
        let counts = Counts {
            candidate: 7,
            ..Counts::default()
        };
        let s = status(
            &state,
            counts,
            Timestamp(1_000),
            Readiness::CycleCompleted,
            &[],
        );
        assert_eq!(s.observed_generation, Some(3));
        assert_eq!(s.active_generation, Some(3));
        assert_eq!(s.last_cycle_time.as_deref(), Some("1970-01-01T00:16:40Z"));
        assert_eq!(s.counts.as_ref().unwrap().candidate, 7);
        let valid = of(&s, VALID);
        assert_eq!(
            (valid.status.as_str(), valid.observed_generation),
            ("True", Some(3))
        );
        assert_eq!(valid.last_transition_time, time(Timestamp(1_000)));
        assert_eq!(of(&s, READY).status, "True");
    }

    #[test]
    fn invalid_generation_lags_and_explains() {
        let mut state = ConfigState::default();
        state.observe(Some((3, Ok(Config::default()))));
        state.observe(Some((4, Err(invalid()))));
        let s = status(
            &state,
            Counts::default(),
            Timestamp(1_000),
            Readiness::CycleCompleted,
            &[],
        );
        assert_eq!(s.observed_generation, Some(4));
        assert_eq!(s.active_generation, Some(3));
        let valid = of(&s, VALID);
        assert_eq!(
            (valid.status.as_str(), valid.reason.as_str()),
            ("False", "Invalid")
        );
        assert!(
            valid.message.contains("spec.cycleInterval"),
            "{}",
            valid.message
        );
    }

    #[test]
    fn transition_time_moves_only_on_a_change() {
        let mut state = ConfigState::default();
        state.observe(Some((1, Ok(Config::default()))));
        let first = status(
            &state,
            Counts::default(),
            Timestamp(1_000),
            Readiness::CycleCompleted,
            &[],
        );
        let again = status(
            &state,
            Counts::default(),
            Timestamp(2_000),
            Readiness::CycleCompleted,
            &first.conditions,
        );
        assert_eq!(
            of(&again, VALID).last_transition_time,
            time(Timestamp(1_000))
        );

        state.observe(Some((2, Err(invalid()))));
        let broken = status(
            &state,
            Counts::default(),
            Timestamp(3_000),
            Readiness::CycleCompleted,
            &again.conditions,
        );
        assert_eq!(
            of(&broken, VALID).last_transition_time,
            time(Timestamp(3_000))
        );
        assert_eq!(
            of(&broken, READY).last_transition_time,
            time(Timestamp(1_000))
        );
    }

    #[test]
    fn ready_follows_readiness_and_never_says_not_ready() {
        let mut state = ConfigState::default();
        state.observe(Some((1, Ok(Config::default()))));
        for (readiness, expected) in [
            (Readiness::CycleCompleted, ("True", "CycleCompleted")),
            (Readiness::Stalled, ("False", "Stalled")),
            (Readiness::CachesNotSynced, ("False", "CachesNotSynced")),
        ] {
            let s = status(&state, Counts::default(), Timestamp(1_000), readiness, &[]);
            let ready = of(&s, READY);
            assert_eq!((ready.status.as_str(), ready.reason.as_str()), expected);
        }
        for readiness in Readiness::ALL {
            let s = status(&state, Counts::default(), Timestamp(1_000), readiness, &[]);
            assert_ne!(of(&s, READY).reason, "NotReady", "{readiness:?}");
            assert_eq!(of(&s, READY).reason, readiness.as_str());
        }
    }

    #[test]
    fn with_ready_changes_only_ready() {
        let mut state = ConfigState::default();
        state.observe(Some((3, Ok(Config::default()))));
        let counts = Counts {
            reaped: 2,
            ..Counts::default()
        };
        let before = status(
            &state,
            counts,
            Timestamp(1_000),
            Readiness::CycleCompleted,
            &[],
        );

        assert_eq!(
            with_ready(&before, Readiness::CycleCompleted, Timestamp(2_000)),
            None,
            "same reason: nothing to write"
        );
        let stalled = with_ready(&before, Readiness::Stalled, Timestamp(2_000)).unwrap();
        assert_eq!(stalled.counts, before.counts);
        assert_eq!(stalled.last_cycle_time, before.last_cycle_time);
        assert_eq!(of(&stalled, VALID), of(&before, VALID));
        let ready = of(&stalled, READY);
        assert_eq!(
            (ready.status.as_str(), ready.reason.as_str()),
            ("False", "Stalled")
        );
        assert_eq!(ready.observed_generation, Some(3));
        assert_eq!(ready.last_transition_time, time(Timestamp(2_000)));

        // False to False keeps the transition time.
        let unsynced = with_ready(&stalled, Readiness::CachesNotSynced, Timestamp(3_000)).unwrap();
        assert_eq!(
            of(&unsynced, READY).last_transition_time,
            time(Timestamp(2_000))
        );
    }
}
