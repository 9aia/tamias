+++
id = "PL-002"
title = "P9 — move provisioning/deploy/service from peregrin to tamias (host metaservice)"
status = "done"
created = "2026-11-07"
owner = "human:luis"
priority = "high"
epic = "tamias"
milestone = "v1"
+++

## Goal

Move **provisioning / deploy / service lifecycle** out of peregrin into
tamias. tamias becomes the **host/shell metaservice** (the thing that
manages what is installed and running on a box); peregrin returns to its
domain (campaigns, drivers, the node, sessions, hub).

## Boundary

| Concern | Owner |
| --- | --- |
| fetch / store / cache / registry | tamias |
| install / remove / list / info / update / pin / verify | tamias |
| **provision** (bundle → runnable artifact) | tamias verb + product provider |
| **deploy** (webhook, service unit, activate) | tamias verb + product provider |
| **service lifecycle** (systemd/launchd unit) | tamias (`deploy` adapter) |
| campaigns / drivers / events / node / sessions / hub | peregrin |
| `peregrin serve` | peregrin (the command the unit runs) |

## Mechanism

- **Content-type providers over a wire.** tamias never links a product
  crate; it shells the product's provider:
  `<provider> provision {install,deploy,uninstall} --dir <dir> --name <name>`.
- **tamias owns the generic service mechanism** (`tamias service …`, a
  systemd/launchd adapter); providers supply the domain spec / side
  effects (unit ExecStart, webhook target).

## Phases

- **P9a — tamias host layer.** `deploy` port + `-local` (systemd/launchd)
  adapter; provider protocol in `tamias-content`; CLI `deploy` /
  `uninstall` / `service`; `install` runs the provider's install hook.
- **P9b — peregrin provider.** `peregrin provision install|deploy|uninstall`
  (moves `ops.rs`/`service.rs` logic); the `peregrin-bundle` content type
  names `peregrin` as its provider.
- **P9c — prune peregrin.** Drop the distribution + provisioning +
  service verbs (tamias is the entry point); peregrin `check/test` green.

## Gate

tamias and peregrin `check/test` green at each step.

## Risks

- Provider-protocol churn; systemd/launchd portability.
- Keeping peregrin's schema/knowledge out of tamias (the whole point of
  providers).
