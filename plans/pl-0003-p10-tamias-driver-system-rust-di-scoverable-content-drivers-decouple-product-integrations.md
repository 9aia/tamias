+++
id = "PL-003"
title = "P10 — tamias driver system: Rust di scoverable content drivers (decouple product integrations)"
status = "in-progress"
created = "2026-11-07"
owner = "human:luis"
priority = "high"
epic = "tamias"
milestone = "v1"
+++

## Goal

Make tamias's content-type integrations **decoupled and Rust-native** via a
first-class **driver system**. Today a content type shells a *provider*
(`<provider> provision …`). Generalize that into a named, discoverable
**driver protocol**: each product ships a Rust binary
**`tamias-driver-<name>`**; tamias **finds** it and speaks JSON over
stdio. Installing a product (myco, kuda, …) drops its driver; tamias then
knows how to install/deploy that product's content — with no compile-time
coupling either way.

## Why not a `.so`

Rust has no stable ABI for dylib plugins. The ecosystem rule ("embed over
an ndjson/stdio wire") is the right mechanism: a **subprocess driver**
speaking JSON. It is decoupled, language-agnostic at the boundary, and
versionable.

## Design

- **Discovery:** tamias looks for `tamias-driver-<type>` on `PATH`, then
  `<tamias-home>/drivers/`, then a registry entry. Installing a product
  (via tamias) places its driver in `drivers/`.
- **Protocol (JSON over stdio, one request/response per line):**
  - `{"op":"capabilities"}` → the content types + verbs it handles.
  - `{"op":"install","dir":…,"name":…,"config":…}`
  - `{"op":"deploy", …}` · `{"op":"uninstall", …}`
- **Mapping:** a package's `type:` resolves to a driver; tamias dispatches.
  A declared manifest recipe is the data-only fallback (no driver).
- **Superceded:** the current `provider` shell call becomes the driver
  protocol's first implementation (peregrin ships `tamias-driver-peregrin`).

## Phases

- **P10a** — tamias: driver protocol + discovery/registry in `tamias-content`.
- **P10b** — `tamias-driver-peregrin` (move `peregrin provision` behind it).
- **P10c** — `tamias-driver-myco` (+ kuda/mimus) — the genericity proof.
- **P10d** — install-time driver registration (installing a product drops
  its driver so tamias finds it).

## Gate

tamias + peregrin `check/test` green; a fixture driver round-trips.
