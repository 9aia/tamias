+++
id = "ADR-0005"
title = "no tamias-native supervisor — platform unit managers are the boundary"
status = "done"
created = "2026-11-14"
owner = "devin"
priority = "med"
labels = ["architecture"]
links = ["P-011"]
+++

# no tamias-native supervisor — platform unit managers are the boundary

Status: **accepted** · links: org P-011 (cancelled)

## Context

P-011 asked for "tamias supervise" — provider services on hosts
*without* a platform unit manager (containers, minimal Linux). The
question: does tamias need its own supervisor for those hosts?

## Decision

**No.** `tamias-provider`'s `ServiceBackend` already auto-detects and
covers every realistic host: systemd (Linux), launchd (macOS),
Task Scheduler (Windows). The uncovered set — containers and
init-less minimal systems — is deliberately out of scope:

- An in-process supervisor inside a container is an anti-pattern:
  PID 1 semantics, no real init, no cgroup/session integration.
  Orchestration there belongs to the platform (k8s, the container
  runtime, the host's init), not to the payload manager.
- `tamias run` already covers the legitimate foreground/dev case.
- Hosts without any unit manager that still need supervision are a
  platform problem — solve it with the platform's mechanism, not a
  second supervisor inside tamias.

## Consequences

- `ServiceBackend` stays the only supervision surface; no `supervise`
  verb, no detached tamias-owned daemon mode.
- Reopen only if a concrete host appears that (a) runs provider
  services, (b) has no unit manager, and (c) is not a container —
  i.e. a real gap, not a hypothetical.
