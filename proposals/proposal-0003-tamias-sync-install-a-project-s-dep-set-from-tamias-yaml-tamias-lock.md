+++
id = "P-003"
title = "tamias sync — install a project's dep set from tamias.yaml + tamias.lock"
status = "done"
created = "2026-11-10"
owner = "devin"
priority = "medium"
labels = ["deps"]
+++

## Problem

`tamias.yaml` `deps:` + `tamias.lock` are a write-only ledger today:
`add`/`update`/`pin` (CLI) and the TUI Manifest pane record refs and
resolved records, but **no verb installs a project's dep set**. A fresh
clone — or a new machine — has the manifest and lock and must reinstall
every dep by hand. This is the missing half of the dependency story
every other package manager has (`npm install`, `cargo build`,
`pip install -r`).

## Proposal

`tamias sync` — reconcile the project manifest's `deps:` against the
install store + lock:

- Load project manifest (`tamias.yaml`, `load_project_at`) + `tamias.lock`
  from cwd.
- For each `deps:` ref: install via the shared `tamias-install` pipeline
  when the store lacks the package or the record drifted from the lock
  (source_ref/resolved/digest mismatch); refresh the lock entry after
  install (same `Lock::upsert_record`/`sync_record` path `add` uses —
  already shared in tamias-core post-T-014).
- Report: installed / already-current / reinstalled / failed per dep;
  `--json` output; non-zero exit if any dep fails.
- Store is global, not project-scoped → sync NEVER removes entries;
  "extraneous" packages are reported as info at most (`--verbose`),
  removal stays `tamias remove`'s job. Document this boundary.
- Moving refs (`#branch`): sync re-resolves like `add` does and refreshes
  the lock — pinning stays `tamias pin`'s job. `--locked` flag could
  refuse to move a locked entry (nice-to-have; implement if cheap).
- TUI: Manifest pane gains `s` sync verb reusing the same op; serve op
  `sync` for embedders — same shared path (post-T-014 pattern).
- `include:` stays reserved (T-015) — sync covers `deps:` only.

## Non-goals

- Lock-file-only installs without manifest (`--lock` mode) — the lock
  records provenance, not a standalone spec; revisit if asked.
- Pruning/global GC; workspace/dep-graph semantics beyond flat deps.
