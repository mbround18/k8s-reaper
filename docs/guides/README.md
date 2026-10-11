# k8s-reaper guides

Everything you need to run k8s-reaper. If you are new, read them in the order
below. Building or changing the reaper itself is covered in
[CONTRIBUTING.md](../../CONTRIBUTING.md).

## Start here

1. [Getting started](getting-started.md): install with Flux, read the reports
   in report mode, then switch to full mode.
2. [How it works](how-it-works.md): what counts as crash-looping, the states a
   workload goes through, plans, skip reasons and the safety limits.
3. [Rolling out to production](rolling-out.md): a checklist and a real
   timeline for going from report to full mode.

## Reference

- [Configuration](configuration.md): every `ReaperConfig` field, its default
  and range, and the per-workload labels and annotations.
- [Helm chart](helm-chart.md): every chart value, what the chart creates,
  upgrading and uninstalling.
- [Monitoring and alerts](monitoring.md): metrics, health endpoints, logs,
  OpenTelemetry, and a runbook for each alert.

## Topics

- [Flux](flux.md): how the reaper keeps Flux from undoing a reap, and how a
  restore hands control back.
- [CronJobs](cronjobs.md): suspending CronJobs that fail every run, and how
  their failures are counted.
- [Restoring a workload](restoring-a-workload.md): bringing a reaped workload
  back with one annotation.
- [Notifications](notifications.md): Discord, Slack, generic webhooks and
  email.
- [Troubleshooting](troubleshooting.md): symptoms, causes and fixes.

## At a glance

| You want to | Do this |
| --- | --- |
| See what the reaper found | `kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state` |
| Read why, and what it would do | `kubectl get <kind> <name> -n <ns> -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/plan}'` |
| Check the mode and counts | `kubectl get reaperconfig default` |
| Keep a workload out entirely | `kubectl label <kind> <name> -n <ns> reaper.boop.ninja/mode=off` |
| Bring a reaped workload back | `kubectl annotate <kind> <name> -n <ns> reaper.boop.ninja/restore=true` |
| Watch it work | `kubectl get events -A --field-selector reason=Reaped -w` |
