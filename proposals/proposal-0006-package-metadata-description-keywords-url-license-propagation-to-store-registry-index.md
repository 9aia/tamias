+++
id = "P-006"
title = "package metadata: description/keywords/url/license propagation to store + registry index"
status = "open"
created = "2026-11-14"
owner = "devin"
priority = "med"
labels = ["metadata", "registry"]
+++

## Problem

`Manifest` already carries `description`, but it stops there: `StoreRecord` drops it, the registry index doesn't store it, and `tamias list`/`info`/`search`/TUI can't show it. Packages have no `keywords`, `homepage`/`url`, `license`, or `authors` fields at all — so discovery (`tamias search`), provenance display, and any future registry UI are blind to the metadata that makes a package index useful (npm/cargo/rubygems all carry this set).

## Proposal

Extend the manifest schema and propagate metadata end-to-end:

1. **Manifest fields** (`tamias-core::Manifest`): add optional
   `keywords: Vec<String>`, `homepage: Option<String>` (or `urls` map
   if multi-link is wanted — keep it simple: `homepage` +
   `repository`), `license: Option<String>` (SPDX id or free text),
   `authors: Vec<String>` (or `maintainers`). All optional, serde
   defaults, schemars-derived — no breaking change.
2. **Propagation**: carry `description` + the new fields into
   `StoreRecord` (optional, skip-serializing) so `list`/`info` report
   them without re-reading manifests; extend the registry index entry
   schema (versioned — check how index entries are versioned today)
   so publish/search see them.
3. **Surfaces**: `tamias list`/`info`/`search` (`--json` + human) and
   the TUI package view render description/keywords/license; `search`
   matches on name + keywords + description.
4. **Validation**: charset/length bounds on keywords (like `is_valid_kind`
   style), URL sanity (https preferred, http allowed with warning?),
   SPDX-ish license string — keep validation light, warn-don't-reject
   where reasonable.
5. **Back-compat**: old manifests/packs without the fields keep
   working (all optional); old store records/index entries decode
   fine (serde defaults).

## Non-goals

- Registry UI / hosted search ranking — index data only.
- Signing/verification changes — metadata is display data, not
  provenance anchors (digest/signature semantics unchanged).
- crates.io/npm publishing interop — maybe later via tarsi.

## Acceptance

- `tamias.yaml` accepts the new fields; `tamias publish` indexes them;
  `tamias install`/`list`/`info`/`search`/`ui` show them;
  golden fixtures updated; `cargo xtask check` + `test` green.
