//! Which configuration is active, and what to report when it changes
//! (FR-024, FR-026, US4).
//!
//! The adapter reads the `ReaperConfig` each cycle and hands what it saw to
//! [`ConfigState::observe`]. An invalid generation never replaces the last
//! valid one; a deleted object falls back to the defaults.

use std::collections::BTreeSet;

use crate::config::{Config, ConfigError};
use crate::model::{ObjectKind, ObjectRef};
use crate::plan::InvalidSetting;
use crate::ports::{EventRecord, EventType, WorkloadRow};
use crate::record::State;

pub const CONFIG_APPLIED: &str = "ConfigApplied";
pub const CONFIG_INVALID: &str = "ConfigInvalid";
const VALIDATE: &str = "Validate";

/// What changed in the latest [`ConfigState::observe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigChange {
    /// A new generation became active.
    Applied { generation: i64 },
    /// This generation failed validation; the last valid config stays active.
    Invalid { generation: i64, error: ConfigError },
    /// The object is gone; the defaults are active again.
    Removed,
}

impl ConfigChange {
    /// The Event for this change on the `ReaperConfig` named `name`
    /// (contracts/events.md). `Removed` has no object left to carry one.
    #[must_use]
    pub fn event(&self, name: &str) -> Option<EventRecord> {
        let regarding = ObjectRef {
            kind: ObjectKind::ReaperConfig,
            namespace: None,
            name: name.to_owned(),
        };
        match self {
            ConfigChange::Applied { generation } => Some(EventRecord {
                regarding,
                reason: CONFIG_APPLIED,
                event_type: EventType::Normal,
                action: VALIDATE,
                note: format!("generation {generation} is active"),
            }),
            ConfigChange::Invalid { generation, error } => Some(EventRecord {
                regarding,
                reason: CONFIG_INVALID,
                event_type: EventType::Warning,
                action: VALIDATE,
                note: format!(
                    "generation {generation} ignored; keeping the last valid config: {error}"
                ),
            }),
            ConfigChange::Removed => None,
        }
    }
}

/// The `ConfigInvalid` Event for a mode label or threshold override that
/// could not be used.
#[must_use]
pub fn invalid_setting_event(setting: &InvalidSetting) -> EventRecord {
    EventRecord {
        regarding: setting.object.clone(),
        reason: CONFIG_INVALID,
        event_type: EventType::Warning,
        action: VALIDATE,
        note: format!("{}={:?} is not valid; ignored", setting.key, setting.value),
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigState {
    active: Config,
    active_generation: Option<i64>,
    observed_generation: Option<i64>,
    invalid: Option<(i64, ConfigError)>,
    reported: BTreeSet<InvalidSetting>,
}

impl ConfigState {
    /// Records what the adapter read: the object's generation and its
    /// validated spec, or `None` when no object exists.
    pub fn observe(
        &mut self,
        seen: Option<(i64, Result<Config, ConfigError>)>,
    ) -> Option<ConfigChange> {
        let Some((generation, result)) = seen else {
            let had = self.observed_generation.is_some();
            self.active = Config::default();
            self.active_generation = None;
            self.observed_generation = None;
            self.invalid = None;
            return had.then_some(ConfigChange::Removed);
        };
        self.observed_generation = Some(generation);
        match result {
            Ok(config) => {
                self.invalid = None;
                self.active = config;
                let new = self.active_generation != Some(generation);
                self.active_generation = Some(generation);
                new.then_some(ConfigChange::Applied { generation })
            }
            Err(error) => {
                if self.invalid.as_ref().is_some_and(|(g, _)| *g == generation) {
                    return None;
                }
                self.invalid = Some((generation, error.clone()));
                Some(ConfigChange::Invalid { generation, error })
            }
        }
    }

    #[must_use]
    pub fn active(&self) -> &Config {
        &self.active
    }

    /// Generation in use; `None` while running on the defaults.
    #[must_use]
    pub fn active_generation(&self) -> Option<i64> {
        self.active_generation
    }

    /// Generation last read; `None` when no object exists.
    #[must_use]
    pub fn observed_generation(&self) -> Option<i64> {
        self.observed_generation
    }

    /// Why the latest generation is not active, if it is not.
    #[must_use]
    pub fn error(&self) -> Option<&ConfigError> {
        self.invalid.as_ref().map(|(_, e)| e)
    }

    /// Of this cycle's invalid settings, those not reported since they last
    /// appeared. A setting that is fixed and broken again is reported again.
    pub fn new_invalid_settings(&mut self, current: &[InvalidSetting]) -> Vec<InvalidSetting> {
        let current: BTreeSet<InvalidSetting> = current.iter().cloned().collect();
        let new = current.difference(&self.reported).cloned().collect();
        self.reported = current;
        new
    }
}

/// Workloads per state for the `ReaperConfig` status. `skipped` counts
/// candidates that were skipped, so it overlaps `candidate`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub observed: u32,
    pub candidate: u32,
    pub reaping: u32,
    pub reaped: u32,
    pub skipped: u32,
}

impl Counts {
    #[must_use]
    pub fn from_rows(rows: &[WorkloadRow]) -> Self {
        let mut c = Counts::default();
        for row in rows {
            let n = match row.state {
                State::Observed => &mut c.observed,
                State::Candidate => &mut c.candidate,
                State::Reaping => &mut c.reaping,
                State::Reaped => &mut c.reaped,
            };
            *n += 1;
            if row.skipped.is_some() {
                c.skipped += 1;
            }
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FieldError, Mode};
    use crate::model::{WorkloadKind, WorkloadRef};
    use crate::plan::SkipReason;

    fn full() -> Config {
        Config {
            mode: Mode::Full,
            ..Config::default()
        }
    }

    fn bad() -> ConfigError {
        ConfigError {
            errors: vec![FieldError {
                field: "spec.mode",
                message: "must be \"report\" or \"full\", got \"yes\"".into(),
            }],
        }
    }

    #[test]
    fn no_object_runs_on_the_defaults() {
        let mut s = ConfigState::default();
        assert_eq!(s.observe(None), None);
        assert_eq!(s.active(), &Config::default());
        assert_eq!(s.active().mode, Mode::Report);
        assert_eq!(s.active_generation(), None);
    }

    #[test]
    fn a_new_valid_generation_becomes_active_once() {
        let mut s = ConfigState::default();
        assert_eq!(
            s.observe(Some((1, Ok(full())))),
            Some(ConfigChange::Applied { generation: 1 })
        );
        assert_eq!(s.observe(Some((1, Ok(full())))), None);
        assert_eq!(s.active().mode, Mode::Full);
        assert_eq!(s.active_generation(), Some(1));
    }

    #[test]
    fn an_invalid_generation_keeps_the_last_valid_config() {
        let mut s = ConfigState::default();
        s.observe(Some((1, Ok(full()))));
        assert_eq!(
            s.observe(Some((2, Err(bad())))),
            Some(ConfigChange::Invalid {
                generation: 2,
                error: bad()
            })
        );
        // Reported once per generation.
        assert_eq!(s.observe(Some((2, Err(bad())))), None);
        assert_eq!(s.active().mode, Mode::Full);
        assert_eq!(s.active_generation(), Some(1));
        assert_eq!(s.observed_generation(), Some(2));
        assert_eq!(s.error(), Some(&bad()));

        assert_eq!(
            s.observe(Some((3, Ok(Config::default())))),
            Some(ConfigChange::Applied { generation: 3 })
        );
        assert_eq!(s.error(), None);
    }

    #[test]
    fn deletion_falls_back_to_the_defaults() {
        let mut s = ConfigState::default();
        s.observe(Some((1, Ok(full()))));
        assert_eq!(s.observe(None), Some(ConfigChange::Removed));
        assert_eq!(s.active(), &Config::default());
        assert_eq!(s.observe(None), None);
        // A recreated object starts at generation 1 again.
        assert_eq!(
            s.observe(Some((1, Ok(full())))),
            Some(ConfigChange::Applied { generation: 1 })
        );
    }

    #[test]
    fn an_invalid_first_generation_leaves_the_defaults_active() {
        let mut s = ConfigState::default();
        assert!(matches!(
            s.observe(Some((1, Err(bad())))),
            Some(ConfigChange::Invalid { generation: 1, .. })
        ));
        assert_eq!(s.active(), &Config::default());
        assert_eq!(s.active_generation(), None);
    }

    #[test]
    fn events_follow_the_contract() {
        let e = ConfigChange::Applied { generation: 4 }
            .event("default")
            .unwrap();
        assert_eq!(
            (e.reason, e.event_type, e.action),
            ("ConfigApplied", EventType::Normal, "Validate")
        );
        assert_eq!(e.regarding.kind, ObjectKind::ReaperConfig);
        assert_eq!(e.regarding.name, "default");

        let e = ConfigChange::Invalid {
            generation: 5,
            error: bad(),
        }
        .event("default")
        .unwrap();
        assert_eq!(
            (e.reason, e.event_type),
            ("ConfigInvalid", EventType::Warning)
        );
        assert!(e.note.contains("spec.mode"), "{}", e.note);
        assert_eq!(ConfigChange::Removed.event("default"), None);
    }

    fn setting(name: &str, value: &str) -> InvalidSetting {
        InvalidSetting {
            object: WorkloadRef::new(WorkloadKind::Deployment, "app", name).object_ref(),
            key: crate::record::MODE_KEY.into(),
            value: value.into(),
        }
    }

    #[test]
    fn invalid_settings_are_reported_once_until_fixed() {
        let mut s = ConfigState::default();
        let a = setting("web", "yes");
        let b = setting("db", "loud");
        let mut new = |current: Vec<InvalidSetting>| s.new_invalid_settings(&current);
        assert_eq!(new(vec![a.clone()]), vec![a.clone()]);
        assert_eq!(new(vec![a.clone(), b.clone()]), vec![b.clone()]);
        assert_eq!(new(vec![b.clone()]), vec![]);
        // `a` was fixed, then broken again.
        assert_eq!(new(vec![a.clone(), b]), vec![a.clone()]);

        let e = invalid_setting_event(&a);
        assert_eq!(
            (e.reason, e.event_type),
            ("ConfigInvalid", EventType::Warning)
        );
        assert_eq!(e.regarding, a.object);
        assert_eq!(
            e.note,
            r#"reaper.boop.ninja/mode="yes" is not valid; ignored"#
        );
    }

    #[test]
    fn counts_per_state_with_skipped_overlapping() {
        let row = |name: &str, state, skipped| WorkloadRow {
            workload: WorkloadRef::new(WorkloadKind::Deployment, "app", name),
            state,
            skipped,
            crashloop_seconds: None,
        };
        let c = Counts::from_rows(&[
            row("a", State::Observed, None),
            row("b", State::Candidate, None),
            row("c", State::Candidate, Some(SkipReason::RateLimited)),
            row("d", State::Reaped, None),
        ]);
        assert_eq!(
            c,
            Counts {
                observed: 1,
                candidate: 2,
                reaping: 0,
                reaped: 1,
                skipped: 1
            }
        );
    }
}
