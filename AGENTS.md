# Tamias

Package & content manager for the agent factory — fetch, store, cache,
dispense. Named for *Tamias* — the chipmunk; Greek *tamías* =
"steward/treasurer", after its food-hoarding. tamias owns the
*generic* half of distribution (ref grammar, transport, store, cache)
and delegates content **meaning** to pluggable *content types* — so one
manager ships peregrin bundles, myco kinds/templates, kuda domains,
mimus rules, and anything else. Rust workspace, ports-and-adapters.
Seeded from a copy of `cuora` (its anatomy + cf-Worker pattern) and
grown from peregrin's distribution machinery (its own distribution ADR).

<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->

<!-- sturn:agent-awareness -->

## Sturn awareness

- **No PRs.** The pushed branch/commit is the deliverable; review
  streams continuously (sturn WAL), merge is a human/sturn call.
- sturn-wired repo (`sturn.toml` present): `sturn claim "<glob>"
  --ws <ws>` *before* edits — claims surface conflicts in ms.
- Ops via `sturn write|rename|delete`; `sturn snap` folds the ws
  into a real commit; `sturn push` publishes heads + refs/sturn/*.
- `sturn leave` on exit — drops the actor's claims.

<!-- /sturn:agent-awareness -->

<!-- kuda:agent-awareness -->

## Kuda awareness

- `.kuda/` cards are committed artifacts — read them before
  re-exploring (`kuda map`, `kuda card <mod>`); `kuda status`
  reports per-card drift vs HEAD.
- Code changes that invalidate a card → regenerate it (`kuda
  sync`) in the same change; cards ride the diff they describe.
- `.kuda/index.sqlite` is gitignored + rebuildable; the `*.yaml`
  cards are the checked-in source of truth.

<!-- /kuda:agent-awareness -->

## Layout

Ports-and-adapters, flat `crates/` workspace. `tamias-core` is pure
domain (no I/O); ports are traits, impls are separate crates;
`crates/tamias-cli` (package `tamias`) is the composition root.

| Crate | Role |
| --- | --- |
| `crates/tamias-core` | package, ref grammar, manifest, records, digest — pure |
| `crates/tamias-config` + `-yml`/`-toml` | config model + format port — YAML (`tamias.yml`) is the standard, TOML the alternative |
| `crates/tamias-source` + `-fs`/`-git`/`-http`/`-cf` | ref grammar + source port (`resolve`→`fetch`) + impls (`cf:` = Cloudflare registry refs) |
| `crates/tamias-store` + `-local`/`-cf` | install store port + impls (`-cf` scaffolded) |
| `crates/tamias-cache` + `-local`/`-cf` | content-addressed fetch cache (`-cf` scaffolded) |
| `crates/tamias-registry` + `-local`/`-cf` | index + publish port + impls (local dir, optionally HTTP-served; CF Worker client) |
| `crates/tamias-publish` | author-side packing — payload dir + manifest → `.tgz` |
| `crates/tamias-deploy` + `-local`/`-cf` | deploy port — provision a remote registry (`-cf` materializes Worker + wrangler config) / per-user service unit, systemd or launchd (`-local`) |
| `crates/tamias-driver` | content-driver host — discovers `tamias-driver-<type>` on PATH/`<tamias-home>/drivers/`, dispatches install/deploy/uninstall, registers payload-bundled drivers (consent-gated): portable `drivers/<name>` files plus compiled `drivers/bin/<target-triple>/<name>` bins — only the host triple's dir is installed, a missing platform is a hard error |
| `crates/tamias-install` | the one install pipeline — ref → plan → fetch/cache → manifest → bundled-driver consent + dispatch → store commit; consumed by `tamias-cli` (`install`/`add`/`update`) and `tamias-serve` (`install` op) with surface-tuned progress/consent hints |
| `crates/tamias-driver-tamias` | the *pavo* view driver for the tamias product itself (bin `tamias-driver-tamias`, manifest `name: "tamias"`, `types: []` — read-only over store/cache/registry/drivers). The `tamias-driver-` prefix is required for pavo discovery; it is **not** a content-type driver — a hypothetical `type: tamias` package would fail loudly on the unknown verb, never silently |
| `crates/tamias-serve` | ndjson JSON-RPC over stdio for embedding (`install`/`list`/`info`/`verify`/`remove`) — same wire as `sepia serve`/`cuora serve`/`myco serve` |
| `crates/tamias-shell` | `bash -c` / direct-argv exec helper with env injection (intended for `tamias run`) — **orphan: declared in the workspace but zero dependents** |
| `crates/tamias-telemetry` | opt-in OpenTelemetry layer over the daemons' `tracing` stack (OTLP only when an endpoint is configured) |
| `crates/tamias-testkit` | fixtures + impl conformance |
| `crates/tamias-cli` | the `tamias` binary — clap, `--json`, `manifest`/`schema` |
| `xtask` (pkg `tamias-xtask`) | check / test / coverage / install / package / smoke / site |

> **Seed state (P2 re-derivation).** This repo was seeded from a copy of
> `cuora`, so the crates currently carry cuora's vault/identity/deploy
> shapes under `tamias-*` names. They are being re-derived into the map
> above — see the org-ledger seed proposal and `project/adrs`.

## Boundary

Tamias owns **content distribution** — refs, transport, the install
store + fetch cache, and content-type dispatch. Neighbors own theirs:

- **peregrin** — orchestration; `install/remove/list/…` become thin
  wrappers over `tamias-*` (peregrin's distribution impl moves here).
- **myco** — work memory; its kinds/views/templates ship as a
  `myco-config` content type.
- **kuda** — code memory; `kuda-domain` packs.
- **mimus** — critic; `mimus-rules` packs.
- **cuora** — secrets; publish tokens / private-registry creds via
  `cuora://` refs (values never in `tamias.yaml`).
- **sturn** — ephemeral coordination; claim the store/lock during
  concurrent installs.
- **bupha** / `ci-gate` — the first published bundle.
- **sepia** — session lifecycle · **cicada** — scheduled updates.

A hosted registry is in scope (it's a store/registry impl); a daemon or
multi-actor arbitration belongs to peregrin.

## Commands

```bash
cargo xtask check          # fmt + clippy -D warnings
cargo xtask test           # whole suite (nextest if present)
cargo xtask package        # cargo package --workspace (verify) — pre-release
cargo xtask smoke          # install from crates.io + run — post-publish
cargo xtask coverage       # llvm-cov report (HTML → target/llvm-cov/html);
                           # extra args pass through, e.g. --fail-under-lines 97
cargo xtask install        # release-install tamias → ~/.cargo/bin
cargo xtask site           # build the docs site (needs docs/ worktree)
```

Git hooks live in `.hooks/` (`core.hooksPath` = `.hooks`):
pre-commit runs `cargo fmt --check`, pre-push `cargo xtask check`.

## The worktree anatomy

`project/` and `docs/` are git worktrees of orphan branches —
`project` (the PM ledger, created by `myco init`), `docs` (the
VitePress site → GitHub Pages). Both are gitignored on `main`; never
commit their contents to `main`.

## Rules

- Workspace lints are shared (`Cargo.toml [workspace.lints]`):
  `unsafe_code` forbidden, `unwrap_used` warned — `-D warnings` in CI
  means no warnings tolerated. Tests `#![allow]` liberally.
- No `unsafe`. Edition 2021.
- Keep the binary dependency-light and sync; no async runtime unless a
  content-type provider needs it.
- **Generic engine, pluggable content.** Never link a product crate
  from the engine — content types are *declared* (manifest data), and a
  provider binary is launched over a wire ("no plugin system: files").
- Content provenance is sacred: every install records its source ref,
  resolved sha, and payload digest; `verify` recomputes the digest.
- Digest-pinned refs; optional ssh-sign (sturn-style). Secret **names**
  never values.
- Agents never hand-edit `project/` when a `myco` verb exists.
- `.agents/skills/` is the canonical skill location; `skills-lock.json`
  pins upstream skills.

## Docs

`docs/` worktree — VitePress + Diátaxis (tutorials/how-to/reference/
explanation) → GitHub Pages at `9aia.com/tamias`. `docs/gen-*.py`
renders cli/config/crate-map from source — keep `lib.rs` module docs
accurate; they feed the generated reference.
