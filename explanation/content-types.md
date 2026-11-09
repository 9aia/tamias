---
url: /tamias/explanation/content-types.md
---
# Content types

The engine is **generic**: it moves *bytes + a manifest* and doesn't
care what the bytes mean. A **content type** declares the meaning —
where and how a package installs.

## Declared, not linked

A content type is *data first*: the package's manifest names its `type`
(`kind`). The manager never links a product crate. When a product must do
real work (materialize a node config, enable a systemd unit), it ships a
**driver** binary the manager launches as a subprocess — "no plugin
system: files" (peregrin’s own ADR).

## The roster

| Type | Payload | Installs into |
| --- | --- | --- |
| `peregrin-bundle` | workflows, `.node.yml`, `.snippet.yml`, `.field.yml`, `.prompt.yml` | the peregrin bundle store |
| `myco-config` | `[[kinds]]`, `[views]`, `[verbs]`, templates | `myco.toml` / a seeded `project/` |
| `kuda-domain` | `data/domains/*.toml` | `.kuda/domains/` |
| `mimus-rules` | `rules/*.toml` | `rules/` |
| `files` | anything | the store entry (no driver) |

Adding a type is a new `tamias-driver-<type>` binary — never a change to
the engine.

## Drivers

The engine is generic — it moves *bytes + a manifest* and unpacks into
the store. Everything product-specific beyond that is a **driver**:
when a package's `type:` isn't `files`, tamias resolves a
`tamias-driver-<type>` executable — on `PATH` or in
`<tamias-home>/drivers/` — and runs it as a subprocess:

```text
tamias-driver-<type> install --dir <payload> --name <name> [--config <file>]
tamias-driver-<type> deploy   --dir <dir>     --name <name> [--config <file>]
tamias-driver-<type> uninstall --dir <dir>    --name <name>
tamias-driver-<type> capabilities            # → {"types":[…]}
```

The driver owns the product's schema — where it materializes, what
"deployed" means, how to tear down. tamias never learns it.

A package can **ship its own driver** under `drivers/` in the payload;
installing registers it into `<tamias-home>/drivers/`. Because that's
arbitrary code, it requires `--allow-drivers` /
`TAMIAS_ALLOW_DRIVERS=1` — see [Security model](/explanation/security).

`files` is the built-in type: the store entry is the whole story, no
driver needed. Any other type **fails to install** without a
discoverable driver — fail closed, never silent.

See [Write a driver](/how-to/write-a-driver) for the authoring side.
