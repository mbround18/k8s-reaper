# reaper-notify

Notification delivery: Discord, Slack, generic webhooks and SMTP email.
Implements the core `Notifier` port.

- `channels`: one channel per file in the mounted Secret directory; the
  provider is inferred from the URL or set with `?format=`.
- `payload`: the body each provider receives, as pure functions.
- `worker`: `NotifyHandle`, a bounded best-effort queue and a background
  worker with retries. A slow channel never delays a cycle.
- Channel URLs are credentials, held in `Secret` and never logged.

Tests: `cargo test -p reaper-notify` (delivery runs against a local server).

See [CONTRIBUTING.md, Architecture](../../CONTRIBUTING.md#architecture).
