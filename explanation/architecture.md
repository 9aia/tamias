# Architecture

> Seeded from a copy of `cuora` (its ports-and-adapters anatomy and the
> `-cf` Worker pattern).

tamias is **ports-and-adapters**. The engine owns the generic half of
distribution; every content *meaning* is an adapter.

## Ports

| Port | Role | Impls |
| --- | --- | --- |
| `ref` | claims + parses a ref into an opaque spec | one per kind (in the `source-*` crates) |
| `source` | `plan(spec)` → `fetch(plan, into)` | `-fs`, `-git`, `-http`, `-cf` |
| `store` | put/get/verify an installed package | `-local`, `-cf` |
| `cache` | content-addressed fetch cache | `-local`, `-cf` |
| `registry` | index + publish | `-local`, `-cf` (Worker + R2 + D1/KV) |
| `deploy` | service unit + teardown | `-local` (systemd) |
| `driver` | how a content *type* installs/deploys | `tamias-driver-<type>` (subprocess, argv) |

## Crate map

<!-- crate-map -->
| crate | layer | purpose |
|---|---|---|
| `tamias-core` | domain | Pure domain — package identity (@scope/name@semver), refs, manifests, records, digests (no I/O) |
| `tamias-cache` | port | cache port — read-through fetched content |
| `tamias-config` | port | Config port — where tamias.toml-shaped config comes from |
| `tamias-deploy` | port | deploy port — provision a remote registry (Worker + R2 + D1/KV) |
| `tamias-registry` | port | registry port — index + publish |
| `tamias-source` | port | source port — turn a ref into bytes |
| `tamias-store` | port | store port — content-addressed blobs |
| `tamias-cache-cf` | adapter | Cloudflare adapter (P6) |
| `tamias-cache-local` | adapter | local cache impl — content-addressed fetch cache |
| `tamias-config-toml` | adapter | TOML config impl |
| `tamias-config-yml` | adapter | YAML config impl (the standard) — tamias.yml / tamias.yaml |
| `tamias-deploy-cf` | adapter | Cloudflare deploy impl — the registry Worker + wrangler config |
| `tamias-deploy-local` | adapter | local deploy impl — manage a per-user service unit (systemd/launchd) |
| `tamias-driver` | adapter | content driver host — discovery + dispatch of tamias-driver-&lt;type&gt; (no product knowledge) |
| `tamias-driver-tamias` | adapter | tamias-driver-tamias — pavo view driver over the tamias install store, fetch cache, registry index, and driver dir (read-only) |
| `tamias-install` | adapter | The shared install pipeline — ref → fetch/cache → manifest → driver dispatch → store commit |
| `tamias-provider` | adapter | provider lifecycle — installed `provider:` blocks → units, deploy/uninstall, run exec specs, journal argv |
| `tamias-publish` | adapter | author side — pack + push |
| `tamias-registry-cf` | adapter | Cloudflare registry impl — a Worker + R2 over HTTP |
| `tamias-registry-local` | adapter | dir registry impl — a folder index, servable over HTTP |
| `tamias-registry-open` | adapter | shared registry-source layer — name-or-ref resolution + named-source config CRUD |
| `tamias-serve` | adapter | ndjson JSON-RPC over stdio — the embedding surface |
| `tamias-shell` | adapter | Run a command in a working directory via bash -c — the generic shell-out helper |
| `tamias-source-cf` | adapter | Cloudflare registry source impl — the `cf:` ref arm |
| `tamias-source-fs` | adapter | fs source impl — local dirs and tarballs |
| `tamias-source-git` | adapter | git source impl — clone URL //subdir #rev |
| `tamias-source-http` | adapter | http source impl — .tgz GET + optional sha256 pin |
| `tamias-store-cf` | adapter | Cloudflare adapter (P6) |
| `tamias-store-local` | adapter | local store impl — a directory |
| `tamias-telemetry` | adapter | Opt-in OpenTelemetry layer over the tracing stack — OTLP export, W3C context propagation |
| `tamias-testkit` | adapter | Fixtures + adapter conformance for every port |
| `tamias-tui` | adapter | tamias — interactive package-manager TUI (ratatui); the `tamias ui` surface |
| `tamias` | binary | tamias — package & content manager: fetch, store, cache, dispense |

## The install path

```
ref ─► resolve ─► (cache hit?) ─► fetch ─► driver install ─► store
```

`include:` composes payloads: each entry resolves to a store payload
or a recursively-installed ref, `overlay()` deep-merges bases (earlier
includes win, local wins) and the *composed* tree is what drivers see,
what gets digested, and what the store commits. `publish`/`bundle pack`
flatten includes by default (`--keep-includes` ships the raw payload).

`resolve` pins git refs to a commit sha up front, so the content-
addressed cache can be hit *before* fetching. The fetch→unpack split is
what makes repeat installs and air-gapped nodes free.

## Stores

- **install store** — `<name>@<version>/` with a record (source ref,
  resolved sha, payload digest, installed-at).
- **fetch cache** — content-addressed; `remove` never touches it.
- **registry** — where published content lives: a directory, or a
  Cloudflare Worker + R2 + D1/KV.

## Provenance

`verify` recomputes the payload digest against the record. Digest-pinned
refs (`#sha256:…`, resolved shas) are the integrity contract — see
[Security model](/explanation/security) for the full trust story,
including the `--allow-drivers` consent gate on payload-shipped drivers.

## The embedding surface

`tamias serve` (the `tamias-serve` crate) runs the same
install/list/info/verify/remove verbs plus the registry surface —
`search`, `publish`, and `registry.list|get|add|remove` — as an
**ndjson JSON-RPC service over stdio** for agents and daemons that want
a persistent handle instead of one-shot CLI calls. Registry name-or-ref
resolution and config writes go through `tamias-registry-open`, the same
code the CLI verbs use. The protocol is specified in
[serve reference](/reference/serve).

## Observability

`tamias-telemetry` is an opt-in OpenTelemetry layer over the tracing
stack. Nothing exports unless you set an endpoint:

- `TAMIAS_OTEL_ENDPOINT` — OTLP/HTTP base URL (the opt-in switch), or
  the standard `OTEL_EXPORTER_OTLP_ENDPOINT` /
  `…_TRACES_ENDPOINT`.
- `TAMIAS_OTEL_SERVICE_NAME` / `OTEL_SERVICE_NAME` — `service.name`.
- W3C `traceparent`/`tracestate` propagation is installed when export
  is on; `RUST_LOG`-style filtering still applies.
