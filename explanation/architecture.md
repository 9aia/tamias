# Architecture

> Seeded from a copy of `arca` (its ports-and-adapters anatomy and the
> `-cf` Worker pattern). The crate set is being re-derived — see the
> org-ledger proposal `P-002`.

tamias is **ports-and-adapters**. The engine owns the generic half of
distribution; every content *meaning* is an adapter.

## Ports

| Port | Role | Impls |
| --- | --- | --- |
| `source` | `resolve(ref)` → `fetch(into)` | `-fs`, `-git`, `-http`, `-registry` |
| `store` | put/get/verify an installed package | `-local`, `-cf` |
| `cache` | content-addressed fetch cache | `-local`, `-cf` |
| `content` | how a package *type* installs | `-peregrin`, `-myco`, … |
| `registry` | index + publish | `-local`, `-cf` (Worker + R2 + D1/KV) |

## The install path

```
ref ─► resolve ─► (cache hit?) ─► fetch ─► overlay (include:) ─► content-type install ─► store
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
