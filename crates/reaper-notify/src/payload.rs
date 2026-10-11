//! What each provider receives (contracts/notifications.md). Pure functions of
//! the notification and the cluster name.

use std::fmt::Write as _;

use reaper_core::model::ObjectRef;
use reaper_core::notify::Notification;
use reaper_core::ports::EventType;
use reaper_core::record::State;
use serde_json::{Value, json};

/// The generic payload's `schema` value.
pub const SCHEMA: &str = "reaper.boop.ninja/notification/v1";

/// Discord rejects `content` longer than this.
pub const DISCORD_CONTENT_MAX: usize = 2000;
const DISCORD_DESCRIPTION_MAX: usize = 4096;

/// `Reaped Deployment plain/crashloop`
#[must_use]
pub fn headline(n: &Notification) -> String {
    format!("{} {}", n.reason, object(&n.regarding))
}

/// One line for chat: `[cluster] Reaped Deployment plain/crashloop: <note>`.
#[must_use]
pub fn text(n: &Notification, cluster: &str) -> String {
    let prefix = if cluster.is_empty() {
        String::new()
    } else {
        format!("[{cluster}] ")
    };
    format!("{prefix}{}: {}", headline(n), n.note)
}

#[must_use]
pub fn slack(n: &Notification, cluster: &str) -> Value {
    json!({ "text": text(n, cluster) })
}

#[must_use]
pub fn discord(n: &Notification, cluster: &str) -> Value {
    let mut fields = vec![
        json!({ "name": "state", "value": state(n), "inline": true }),
        json!({ "name": "mode", "value": n.mode.as_str(), "inline": true }),
    ];
    if !cluster.is_empty() {
        fields.push(json!({ "name": "cluster", "value": cluster, "inline": true }));
    }
    json!({
        "content": truncate(&text(n, cluster), DISCORD_CONTENT_MAX),
        "embeds": [{
            "title": headline(n),
            "description": truncate(&n.note, DISCORD_DESCRIPTION_MAX),
            "color": match n.event_type {
                EventType::Warning => 0x00E6_7E22,
                EventType::Normal => 0x002E_CC71,
            },
            "timestamp": n.time.to_string(),
            "fields": fields,
        }],
        // Names in a note must never ping anyone.
        "allowed_mentions": { "parse": [] },
    })
}

#[must_use]
pub fn generic(n: &Notification, cluster: &str) -> Value {
    json!({
        "schema": SCHEMA,
        "reason": n.reason,
        "type": event_type(n.event_type),
        "workload": {
            "kind": n.regarding.kind.as_str(),
            "namespace": n.regarding.namespace,
            "name": n.regarding.name,
        },
        "state": n.state.map(State::as_str),
        "mode": n.mode.as_str(),
        "note": n.note,
        "time": n.time.to_string(),
        "cluster": cluster,
    })
}

/// `[k8s-reaper] Reaped Deployment/plain/crashloop`
#[must_use]
pub fn email_subject(n: &Notification) -> String {
    let r = &n.regarding;
    match &r.namespace {
        Some(ns) => format!(
            "[k8s-reaper] {} {}/{ns}/{}",
            n.reason,
            r.kind.as_str(),
            r.name
        ),
        None => format!("[k8s-reaper] {} {}/{}", n.reason, r.kind.as_str(), r.name),
    }
}

#[must_use]
pub fn email_body(n: &Notification, cluster: &str) -> String {
    let mut body = format!("{}\n\n{}\n\n", headline(n), n.note);
    if !cluster.is_empty() {
        let _ = writeln!(body, "cluster: {cluster}");
    }
    let _ = write!(
        body,
        "state:   {}\nmode:    {}\ntype:    {}\ntime:    {}\n",
        state(n),
        n.mode.as_str(),
        event_type(n.event_type),
        n.time,
    );
    body
}

fn object(r: &ObjectRef) -> String {
    match &r.namespace {
        Some(ns) => format!("{} {ns}/{}", r.kind.as_str(), r.name),
        None => format!("{} {}", r.kind.as_str(), r.name),
    }
}

fn state(n: &Notification) -> &'static str {
    n.state.map_or("none", |s| s.as_str())
}

fn event_type(t: EventType) -> &'static str {
    match t {
        EventType::Normal => "Normal",
        EventType::Warning => "Warning",
    }
}

/// At most `max` characters, ending in `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}
