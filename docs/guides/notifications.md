# Notifications

k8s-reaper can post to chat or send mail when a workload changes state: it
was reaped, a reap failed, it was restored, and so on. Notifications are
optional and best-effort. With nothing configured, the reaper sends nothing
and opens no outbound connection.

The setup has two parts:

- **Where to send.** A Kubernetes Secret holds the channels. Each key in the
  Secret is one channel and holds a single URL. A channel URL is a credential,
  so it lives in a Secret and never in the ReaperConfig.
- **What to send.** The `notifications` block of the ReaperConfig chooses the
  event reasons, the cluster name shown in messages, and how often a repeat is
  allowed.

Events and metrics stay the durable record. Notifications are lost if the
reaper restarts with messages still queued.

## 1. Create the Secret

The key name becomes the channel name. Metrics, logs and per-channel filters
use that name, so choose names you can recognise later:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: k8s-reaper-notifications
  namespace: k8s-reaper
stringData:
  ops-discord: https://discord.com/api/webhooks/123456789/your-token
  team-slack: https://hooks.slack.com/services/T000/B000/xxxx
  oncall-mail: smtps://reaper%40example.test:app-password@smtp.example.test?from=reaper@example.test&to=oncall@example.test,sre@example.test
  pager-bridge: https://alerts.example.test/hooks/reaper
```

```sh
kubectl apply -f k8s-reaper-notifications.yaml
```

The reaper reads the Secret from files mounted into its pod, not from the
Kubernetes API, so it needs no permission to read Secrets. The mount goes at
`/etc/k8s-reaper/notifications`. Change that path with `--notifications-dir` or
`REAPER_NOTIFICATIONS_DIR`. Mark the volume `optional: true` so a missing
Secret leaves the reaper running with notifications off:

```yaml
# In the reaper Deployment's pod spec
volumes:
  - name: notifications
    secret:
      secretName: k8s-reaper-notifications
      optional: true
containers:
  - name: k8s-reaper
    volumeMounts:
      - name: notifications
        mountPath: /etc/k8s-reaper/notifications
        readOnly: true
```

The reaper re-reads the directory every cycle. Rotating a URL, or adding or
removing a channel, takes effect without a restart once the kubelet syncs the
mounted Secret, which usually takes about a minute.

## 2. Pick a provider

The provider comes from the URL. To override it, add
`?format=discord|slack|generic`; the reaper removes that parameter before
sending.

### Discord

In the channel settings, open **Integrations → Webhooks → New Webhook** and
copy the URL:

```
https://discord.com/api/webhooks/<id>/<token>
```

Each message is one line of text plus an embed with the state, the mode and
the cluster. Mentions are switched off, so a workload name never pings anyone.

### Slack

Create an incoming webhook for a Slack app and copy the URL:

```
https://hooks.slack.com/services/<a>/<b>/<c>
```

Each message is a single line of text. Mattermost and Rocket.Chat incoming
webhooks take the same body; add `format=slack` to their URLs:

```
https://chat.example.test/hooks/abc123?format=slack
```

### Generic webhook

Any other `https://` URL receives a versioned JSON document. Use it for your
own bridge, an alert router or an automation tool:

```json
{
  "schema": "reaper.boop.ninja/notification/v1",
  "reason": "Reaped",
  "type": "Warning",
  "workload": { "kind": "Deployment", "namespace": "shop", "name": "api" },
  "state": "reaped",
  "mode": "full",
  "note": "scaled 3 → 0 after 42 restarts in 30m …",
  "time": "2026-10-10T16:00:00Z",
  "cluster": "prod-eu"
}
```

A field may be added in the future without changing `schema`. A field will
not be removed or change meaning unless `schema` changes. Redirects are not
followed, so point the URL at the final endpoint.

### Email (SMTP)

| URL form                                 | Encryption                         | Default port |
| ---------------------------------------- | ---------------------------------- | ------------ |
| `smtps://user:pass@host?from=…&to=…`     | TLS from the first byte            | 465          |
| `smtp://user:pass@host?from=…&to=…`      | STARTTLS, required                 | 587          |

- `from` is required.
- `to` is required and takes a comma-separated list of addresses.
- Put a different port in the URL as usual: `smtp.example.test:2525`.
- Percent-encode the username and password. `@` becomes `%40` and `:`
  becomes `%3A`.
- If the server does not need a login, leave out `user:pass@`.

```
smtps://reaper%40example.test:app-password@smtp.example.test?from=k8s-reaper%20<reaper@example.test>&to=oncall@example.test
```

The subject is `[k8s-reaper] <Reason> <Kind>/<namespace>/<name>`. The body is
plain text.

### Unencrypted endpoints

The reaper refuses `http://` webhooks and `smtp://` without STARTTLS. To allow
them for a local test catcher, add `insecure=true`. Never use it in
production:

```
http://echo.notify-dev.svc.cluster.local:8080/hooks/reaper?insecure=true
smtp://mailpit.notify-dev.svc.cluster.local:1025?insecure=true&from=reaper@example.test&to=oncall@example.test
```

## 3. Choose what is sent

Notifications follow the reaper's
[Kubernetes Events](../../specs/001-crashloop-reaper/contracts/events.md).
Configure them in the ReaperConfig:

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec:
  notifications:
    enabled: true
    clusterName: prod-eu
    reasons: [Reaped, ReapFailed, Restored, RestoreFailed, HelmReleaseHeld, ConfigInvalid]
    minInterval: 1h
    channels:
      ops-discord: { reasons: [Reaped, ReapFailed, RestoreFailed] }
```

| Field         | Default                | Meaning |
| ------------- | ---------------------- | ------- |
| `enabled`     | `true`                 | Master switch. Even when `true`, nothing is sent until the Secret holds a channel. |
| `clusterName` | empty                  | Shown in every message, up to 63 characters. Set it when several clusters post to one channel. |
| `reasons`     | the six in the example | Which Event reasons are sent at all. |
| `minInterval` | `1h`                   | A repeat of the same reason for the same workload within this time is dropped. Range `0s` to `24h`. |
| `channels`    | none                   | Optional per-channel filter, keyed by channel name. A channel not listed here gets every reason in `reasons`. |

The default reasons are the transitions that need a person to act. These
reasons are opt-in because they can be frequent, especially in report mode:

- `ReapPlanned`: what full mode would reap.
- `ReapSkipped`
- `CrashLoopObserved`
- `CrashLoopRecovered`
- `RecordFailed`: the reaper could not write its own label or annotations on
  a workload. It repeats every cycle until fixed, and the
  `ReaperActionFailed` alert already covers it
  ([monitoring.md](monitoring.md#when-actionrecord)).

**A channel filter can only narrow `reasons`.** A reason missing from the
top-level list reaches no channel. To send `ReapPlanned` to one channel only,
add it to `reasons`, then give every other channel a filter without it.

An unknown reason name or an out-of-range value makes the config invalid. The
reaper then keeps the last valid config and records a `ConfigInvalid` Event.

## 4. Check that it works

The reaper exports two metrics:

| Metric                         | What to look at |
| ------------------------------ | --------------- |
| `reaper_notification_channels` | One series per channel file. `valid="false"` means the reaper could not use the URL. |
| `reaper_notifications_total`   | Per channel: `result` is `sent`, `failed`, `dropped` (queue full or shutting down) or `filtered` (the channel's filter excluded the reason). |

If a channel file is invalid, the reaper logs `notification channel ignored`
once, with the channel name and the reason, for example `channel=ops-discord
err="http is unencrypted; add insecure=true to allow it"`. The log never
includes the URL. The other channels keep working.

Delivery:

- Each message has 10 seconds per attempt.
- A failed message is retried twice, after 1 s and then 4 s.
- A `429` response waits for `Retry-After`, up to 60 s.
- Client errors (`4xx`) are not retried, except `408` and `429`.
- A slow or failing channel never delays a reap.

## Helm chart

```yaml
notifications:
  # Name of the Secret holding channel URLs. The chart only mounts it
  # (optional: true); create it yourself or with your secrets tooling.
  # "" drops the mount, and with it notifications.
  secretName: k8s-reaper-notifications
  mountPath: /etc/k8s-reaper/notifications
```

The chart never creates the Secret and never puts URLs in values, so channel
URLs stay out of Helm release history.
