---
url: /tamias/explanation/decisions.md
---
# Decisions

ADR-lite: the calls that shaped tamias, the alternatives rejected, and
the consequences we live with.

## Ports-and-adapters, flat crates

**Decision:** the engine is pure domain (`tamias-core`, no I/O); every
external capability — source, store, cache, registry, deploy — is a
trait with `-local`/`-cf`/platform impls in sibling crates.

**Alternatives:** a monolithic binary; an async runtime with plugin
traits loaded in-process.

**Consequence:** impls are swappable and testable in isolation, and the
binary stays sync and dependency-light. The cost is crate sprawl — the
crate map is wide — and every new capability is a new port + impl pair
rather than a function.

## Content types are declared, drivers are subprocesses

**Decision:** a package's `type:` is manifest *data*; the manager
launches `tamias-driver-<type>` as a subprocess over argv — "no plugin
system: files".

**Alternatives:** linking product crates into the engine; an in-process
plugin ABI (dlopen, WASM); a central registry of trusted drivers only.

**Consequence:** adding a content type never changes the engine, and a
malformed driver can't corrupt tamias's memory — but driver bugs are
process boundaries, dispatch has a 120s timeout, and payload-shipped
drivers needed the `--allow-drivers` consent gate. See
[Write a driver](/how-to/write-a-driver).

## `tamias` itself stays a native binary

**Decision:** the `tamias` bin is never a `bins[]` shim — every shim in
the org execs `tamias bin exec`, so tamias can't retire into a shim
(bootstrap, T-007).

**Alternatives:** shipping tamias as a pack like every other product
CLI.

**Consequence:** `cargo install tamias` works forever and shims never
chase a moving helper; the cost is tamias alone among product CLIs is
cargo-installable, which the docs must keep explaining.

## YAML is the standard config, TOML the alternative

**Decision:** `tamias.yml`/`tamias.yaml` is the standard manifest and
project config; TOML is supported via `tamias-config-toml`.

**Alternatives:** TOML-only (the Rust ecosystem default); JSON.

**Consequence:** manifests read like the org's other YAML documents
(peregrin campaigns, kuda cards), at the price of a second format impl
to keep honest — drift between the two loaders is a real bug class.

## `include:` composes, publish flattens

**Decision:** `include:` entries deep-merge into one composed tree that
drivers see and the digest covers; `publish`/`bundle pack` flatten by
default (`--keep-includes` ships raw).

**Alternatives:** shipping includes unresolved (consumers re-compose);
always flattening at install.

**Consequence:** installs are self-contained and digests are stable, but
a flattened publish loses the include structure — `--keep-includes`
exists for packs that want consumers to compose.

## Signing is optional, trust tiers are computed

**Decision:** package signing uses `ssh-keygen -Y` (namespace
`tamias-package`); every install computes a trust tier from pins +
signatures + signers files rather than trusting a declared badge.

**Alternatives:** mandatory signing; a bespoke signature format;
registry-attested trust.

**Consequence:** unpinned installs still work (tier `unpinned`, loudly
labeled) while `signing.required` can hard-fail them for registry
policies. `cf:` installs are TLS-authenticated but not sha-pinned at
transport — a documented caveat, mitigated by signatures. See
[Security model](/explanation/security).
