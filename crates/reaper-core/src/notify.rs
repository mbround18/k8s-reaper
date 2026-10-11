//! Notifications (US6, FR-035 to FR-041): which transitions go to which
//! channel, and how often. Delivery is an adapter (`reaper-notify`); this
//! module only decides, as a pure function of time and history.

use std::collections::BTreeMap;

use crate::config::{Mode, Notifications};
use crate::model::ObjectRef;
use crate::ports::EventType;
use crate::record::State;
use crate::time::Timestamp;

/// Every Event reason in contracts/events.md, the only valid filter values.
pub const EVENT_REASONS: &[&str] = &[
    "CrashLoopObserved",
    "ReapPlanned",
    "ReapSkipped",
    "Reaped",
    "ReapFailed",
    "Restored",
    "RestoreFailed",
    "RestoreIgnored",
    "CrashLoopRecovered",
    "JobFailuresObserved",
    "JobFailuresRecovered",
    "HelmReleaseHeld",
    "HelmReleaseReleased",
    "ConfigInvalid",
    "ConfigApplied",
];

/// Sent unless the config says otherwise: what a person has to act on.
/// `ReapPlanned` is opt-in (spec.md Question 6).
pub const DEFAULT_REASONS: &[&str] = &[
    "Reaped",
    "ReapFailed",
    "Restored",
    "RestoreFailed",
    "HelmReleaseHeld",
    "ConfigInvalid",
];

/// One transition, as sent to every channel that wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub reason: &'static str,
    pub event_type: EventType,
    pub regarding: ObjectRef,
    /// The state the object is left in; `None` when it was cleared.
    pub state: Option<State>,
    pub mode: Mode,
    /// The Event note.
    pub note: String,
    pub time: Timestamp,
}

/// Hands notifications to delivery. Must not block: delivery is best-effort
/// and never delays a cycle (FR-038).
pub trait Notifier {
    fn enqueue(&self, notification: Notification, config: &Notifications);
}

/// Whether `channel` takes `reason`, given that the global filter passed.
#[must_use]
pub fn channel_wants(config: &Notifications, channel: &str, reason: &str) -> bool {
    config
        .channels
        .get(channel)
        .is_none_or(|reasons| reasons.contains(reason))
}

/// The global filter and the per-object, per-reason `minInterval` (FR-037).
#[derive(Debug, Default)]
pub struct Throttle {
    last_sent: BTreeMap<(ObjectRef, &'static str), Timestamp>,
}

impl Throttle {
    /// Keeps the notifications that pass, and remembers them.
    pub fn admit(
        &mut self,
        config: &Notifications,
        batch: Vec<Notification>,
        now: Timestamp,
    ) -> Vec<Notification> {
        let window = i64::try_from(config.min_interval.as_secs()).unwrap_or(i64::MAX);
        self.last_sent
            .retain(|_, sent| now.seconds_since(*sent) < window);
        if !config.enabled {
            return Vec::new();
        }
        batch
            .into_iter()
            .filter(|n| config.reasons.contains(n.reason))
            .filter(|n| {
                let key = (n.regarding.clone(), n.reason);
                if self.last_sent.contains_key(&key) {
                    return false;
                }
                if window > 0 {
                    self.last_sent.insert(key, now);
                }
                true
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use super::*;
    use crate::model::{WorkloadKind, WorkloadRef};

    fn n(name: &str, reason: &'static str) -> Notification {
        Notification {
            reason,
            event_type: EventType::Warning,
            regarding: WorkloadRef::new(WorkloadKind::Deployment, "app", name).object_ref(),
            state: Some(State::Reaped),
            mode: Mode::Full,
            note: "container api restarted 12 times".into(),
            time: Timestamp(0),
        }
    }

    fn reasons(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn default_reasons_are_known_event_reasons() {
        for r in DEFAULT_REASONS {
            assert!(EVENT_REASONS.contains(r), "{r}");
        }
    }

    #[test]
    fn only_configured_reasons_pass() {
        let config = Notifications::default();
        let mut t = Throttle::default();
        let out = t.admit(
            &config,
            vec![
                n("a", "ReapPlanned"),
                n("a", "Reaped"),
                n("b", "CrashLoopObserved"),
            ],
            Timestamp(100),
        );
        assert_eq!(out, vec![n("a", "Reaped")]);
    }

    #[test]
    fn disabled_sends_nothing() {
        let config = Notifications {
            enabled: false,
            ..Notifications::default()
        };
        assert_eq!(
            Throttle::default().admit(&config, vec![n("a", "Reaped")], Timestamp(0)),
            Vec::new()
        );
    }

    #[test]
    fn repeats_inside_min_interval_are_dropped() {
        let config = Notifications::default(); // 1h
        let mut t = Throttle::default();
        assert_eq!(
            t.admit(&config, vec![n("a", "Reaped")], Timestamp(0)).len(),
            1
        );
        // Same object and reason: dropped. Another object or reason: sent.
        let out = t.admit(
            &config,
            vec![n("a", "Reaped"), n("b", "Reaped"), n("a", "ReapFailed")],
            Timestamp(1800),
        );
        assert_eq!(out, vec![n("b", "Reaped"), n("a", "ReapFailed")]);
        // After the window it goes out again.
        assert_eq!(
            t.admit(&config, vec![n("a", "Reaped")], Timestamp(3600))
                .len(),
            1
        );
    }

    #[test]
    fn zero_interval_never_throttles_or_remembers() {
        let config = Notifications {
            min_interval: Duration::ZERO,
            ..Notifications::default()
        };
        let mut t = Throttle::default();
        for _ in 0..3 {
            assert_eq!(
                t.admit(&config, vec![n("a", "Reaped")], Timestamp(5)).len(),
                1
            );
        }
        assert_eq!(t.last_sent.len(), 0);
    }

    #[test]
    fn per_channel_filters_narrow_the_global_set() {
        let config = Notifications {
            channels: BTreeMap::from([("ops".to_owned(), reasons(&["ReapFailed"]))]),
            ..Notifications::default()
        };
        assert!(channel_wants(&config, "ops", "ReapFailed"));
        assert!(!channel_wants(&config, "ops", "Reaped"));
        assert!(channel_wants(&config, "team", "Reaped"));
    }
}
