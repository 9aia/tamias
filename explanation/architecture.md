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
| `driver` | how a content *type* installs/deploys | `tamias-driver-<type>` (subprocess, JSON) |

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
| `tamias-publish` | adapter | author side — pack + push |
| `tamias-registry-cf` | adapter | Cloudflare registry impl — a Worker + R2 over HTTP |
| `tamias-registry-local` | adapter | dir registry impl — a folder index, servable over HTTP |
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
| `tamias` | binary | tamias — package & content manager: fetch, store, cache, dispense |

## The install path

```
ref ─► resolve ─► (cache hit?) ─► fetch ─► overlay (include:) ─► driver install ─► store
```

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
refs (`#sha256:…`, resolved shas) are the integrity contract.
