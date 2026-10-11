//! Reads the `ReaperConfig` every cycle and writes its status (FR-024,
//! FR-026; contracts/config.md).
//!
//! One GET per cycle rather than a watch: a change is picked up by the next
//! cycle, which is all US4 asks, and there is no extra cache to keep in sync.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::jiff;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};
use reaper_core::Timestamp;
use reaper_core::config_state::{ConfigChange, ConfigState, Counts};
use serde_json::json;

use crate::crd::{ReaperConfig, ReaperConfigStatus, StateCounts};

pub const VALID: &str = "Valid";
pub const READY: &str = "Ready";

pub struct ConfigSource {
    api: Api<ReaperConfig>,
    name: String,
    state: ConfigState,
    /// The conditions on the object, so a transition time survives a status
    /// rewrite that does not change the condition.
    conditions: Vec<Condition>,
}

impl ConfigSource {
    #[must_use]
    pub fn new(client: Client, name: impl Into<String>) -> Self {
        Self {
            api: Api::all(client),
            name: name.into(),
            state: ConfigState::default(),
            conditions: Vec::new(),
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
        }
        Ok(self.state.observe(seen))
    }

    /// Writes the status subresource. Does nothing while no object exists.
    pub async fn write_status(
        &mut self,
        counts: Counts,
        last_cycle: Timestamp,
        ready: bool,
    ) -> Result<(), kube::Error> {
        if self.state.observed_generation().is_none() {
            return Ok(());
        }
        let status = status(&self.state, counts, last_cycle, ready, &self.conditions);
        self.api
            .patch_status(
                &self.name,
                &PatchParams::default(),
                &Patch::Merge(json!({ "status": status })),
            )
            .await?;
        self.conditions = status.conditions;
        Ok(())
    }
}

/// The status for `state` after a cycle that finished at `now`.
#[must_use]
pub fn status(
    state: &ConfigState,
    counts: Counts,
    now: Timestamp,
    ready: bool,
    previous: &[Condition],
) -> ReaperConfigStatus {
    let observed = state.observed_generation();
    let valid = match state.error() {
        None => condition(VALID, true, "Valid", String::new()),
        Some(err) => condition(VALID, false, "Invalid", err.to_string()),
    };
    let ready = if ready {
        condition(READY, true, "CycleCompleted", String::new())
    } else {
        condition(READY, false, "NotReady", String::new())
    };
    let conditions = [valid, ready]
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
        let s = status(&state, counts, Timestamp(1_000), true, &[]);
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
        let s = status(&state, Counts::default(), Timestamp(1_000), true, &[]);
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
        let first = status(&state, Counts::default(), Timestamp(1_000), true, &[]);
        let again = status(
            &state,
            Counts::default(),
            Timestamp(2_000),
            true,
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
            true,
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
}
