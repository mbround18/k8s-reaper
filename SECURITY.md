# Security policy

k8s-reaper runs with cluster-wide permission to read workloads and, in full
mode, to scale them to zero and suspend Flux objects. A flaw in it can stop
services, so please report one privately.

## Reporting a vulnerability

Use GitHub's private reporting: the **Security** tab of this repository →
**Report a vulnerability**. Please do not open a public issue.

Include what you can of:

- the version (`k8s-reaper --version`, or the image tag);
- what an attacker can do, and what access they need first;
- steps or a manifest that reproduce it.

You should get a reply within a week. Fixes are released as a new patch
version and noted in the release.

## Supported versions

Only the latest release gets security fixes.

## Scope

In scope:

- the reaper acting on a workload, namespace or Flux object it should not
  (outside its scope, opted out, or its own namespace);
- notification channel URLs or other secrets leaking into logs, Events,
  metrics or annotations;
- RBAC in `deploy/` or the Helm chart granting more than the reaper needs.

Out of scope: anyone who can already edit the `ReaperConfig` or the reaper's
Deployment controls what it does, by design.
