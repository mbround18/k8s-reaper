# Contract: notifications

Optional, outbound, best-effort messages to chat or mail when a workload
changes state (US6, FR-035 to FR-041). With no channels configured, the
reaper sends nothing and makes no outbound connection.

## Where channels come from

Channel URLs are secrets (a Discord webhook URL is a credential). They live in
a Kubernetes **Secret that the chart mounts into the pod**. The reaper never
reads Secrets through the API, so the RBAC in [config.md](./config.md#rbac-required-clusterrole)
still has no Secret access.

| Flag                      | Env                          | Default                          | Purpose                          |
| ------------------------- | ---------------------------- | -------------------------------- | -------------------------------- |
| `--notifications-dir`     | `REAPER_NOTIFICATIONS_DIR`   | `/etc/k8s-reaper/notifications`  | Directory of mounted Secret keys |

The directory missing or empty means notifications are off. Each file (each
Secret key) is one **channel**. The key is the channel name, used in metrics
and logs. The file holds one URL. Files whose names start with `.` (the
kubelet's `..data` links) are ignored. The directory is re-read every cycle, so
a rotated Secret takes effect without a restart.

```yaml
apiVersion: v1
kind: Secret
metadata: { name: k8s-reaper-notifications, namespace: k8s-reaper }
stringData:
  ops-discord: https://discord.com/api/webhooks/<id>/<token>
  team-slack: https://hooks.slack.com/services/<a>/<b>/<c>
  oncall-mail: smtps://reaper%40example.test:<password>@smtp.example.test:465?from=reaper@example.test&to=oncall@example.test,sre@example.test
  pager-bridge: https://alerts.example.test/hooks/reaper
```

The chart mounts it with `secret.optional: true`, so a missing Secret leaves
the reaper running with notifications off.

## Provider, inferred from the URL

| URL                                                            | Provider  | Payload                                                       |
| -------------------------------------------------------------- | --------- | ------------------------------------------------------------- |
| `https://discord.com/api/webhooks/…`, `https://discordapp.com/api/webhooks/…` | `discord` | `{"content": <text>, "embeds": [<one embed>]}`, ≤ 2000 chars of content |
| `https://hooks.slack.com/…`                                    | `slack`   | `{"text": <text>}` (also works for Mattermost/Rocket.Chat incoming hooks via `generic` with `?format=slack`) |
| `smtp://…` (STARTTLS required) or `smtps://…` (implicit TLS)   | `email`   | Plain-text mail; subject `[k8s-reaper] <Reason> <kind>/<ns>/<name>` |
| any other `https://…`                                          | `generic` | The JSON document below                                        |
| `http://…`                                                     | refused unless `?insecure=true`; reported as an invalid channel | |

A `?format=discord|slack|generic` query parameter overrides inference; it is
stripped before the request is sent. Email takes `from` (required) and `to`
(required, comma-separated) as query parameters, and credentials from the URL
user info (percent-encoded). `smtp://` requires STARTTLS; `smtp://…?insecure=true`
uses STARTTLS only when the server offers it, so it allows an unencrypted
connection (for a local mail catcher in dev only), as `http://…?insecure=true`
does for webhooks. `insecure` is stripped before sending. Default ports: 465
for `smtps`, 587 for `smtp`.

Webhook redirects are not followed: a `3xx` is a failed delivery. Discord
messages set `allowed_mentions` to none, so a name in a note never pings
anyone.

Generic payload (stable, versioned by `schema`):

```json
{
  "schema": "reaper.boop.ninja/notification/v1",
  "reason": "ReapPlanned",
  "type": "Warning",
  "workload": { "kind": "Deployment", "namespace": "plain", "name": "crashloop" },
  "state": "candidate",
  "mode": "report",
  "note": "container api in pod crashloop-7d9f… restarted 14 times …",
  "time": "2026-10-10T16:00:00Z",
  "cluster": "roanoke"
}
```

`cluster` comes from `notifications.clusterName` in ReaperConfig (empty when
unset).

## What is sent

Notifications mirror the transition Events in [events.md](./events.md); the
text is the Event note. Which reasons go out is set in ReaperConfig:

```yaml
spec:
  notifications:
    enabled: true                     # master switch; channels still come from the Secret
    clusterName: roanoke
    reasons: [Reaped, ReapFailed, Restored, RestoreFailed, HelmReleaseHeld, ConfigInvalid]
    minInterval: 1h                   # per workload and reason; repeats inside it are dropped
    channels:                         # optional per-channel reason filter, by channel name
      ops-discord: { reasons: [Reaped, ReapFailed, RestoreFailed] }
```

The `reasons` default is the list above: the transitions a person must act
on. `ReapPlanned`, `ReapSkipped`, `CrashLoopObserved` and `CrashLoopRecovered`
are opt-in because they are frequent, above all while the cluster runs in
report mode (Question 6). The config holds no URLs and no secrets.

## Delivery

- Sent after the cycle's writes, from a bounded queue (256 entries; overflow
  drops the oldest and counts it). A slow or failing channel never delays a
  cycle or a reap.
- Per-message timeout 10 s. Up to 3 attempts, waiting 1 s and then 4 s
  between them; `429` honours `Retry-After` up to 60 s. `4xx` other than `408`
  and `429` is not retried.
- At-most-once across restarts: the queue is in memory and lost on restart.
  Events and metrics stay the durable record.
- A channel URL, token, password or any part of it MUST NOT appear in logs,
  Events, metrics, traces or error messages. Channels are named by their key.

## Metrics (added to [metrics.md](./metrics.md))

| Metric                               | Type    | Labels                          | Meaning                                       |
| ------------------------------------ | ------- | ------------------------------- | --------------------------------------------- |
| `reaper_notifications_total`         | counter | `channel`, `provider`, `result` | `result` ∈ `sent`, `failed`, `dropped`, `filtered` |
| `reaper_notification_channels`       | gauge   | `channel`, `provider`, `valid`  | 1 per channel file read in the last cycle; `valid` is `true` or `false`, and `provider` is `unknown` for an invalid file |

An invalid channel file is logged once, by name and reason, when it first
appears or changes; the others still work.
