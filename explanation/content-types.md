# Content types

The engine is **generic**: it moves *bytes + a manifest* and doesn't
care what the bytes mean. A **content type** declares the meaning —
where and how a package installs.

## Declared, not linked

A content type is *data first*: the package's manifest names its `type`
and carries an install recipe (`mode`, `into`, `validate`). The manager
runs the recipe; it never links a product crate. When a product must do
real work (merge `myco.toml`, enable a systemd unit), it ships a
*provider* binary the manager launches over a wire — "no plugin
system: files" (peregrin ADR-0082 #3).

## The roster

| Type | Payload | Installs into |
| --- | --- | --- |
| `peregrin-bundle` | workflows, `.node.yml`, `.snippet.yml`, `.field.yml`, `.prompt.yml` | the peregrin bundle store |
| `myco-config` | `[[kinds]]`, `[views]`, `[verbs]`, templates | `myco.toml` / a seeded `project/` |
| `kuda-domain` | `data/domains/*.toml` | `.kuda/domains/` |
| `mimus-rules` | `rules/*.toml` | `rules/` |
| `files` | anything | `install.into` (escape hatch) |

Adding a type is a new `tamias-content-*` crate (or a declared manifest
recipe) — never a change to the engine.

## Providers (and the driver system)

The engine is generic — it moves *bytes + a manifest* and unpacks into the
store. Everything product-specific beyond that is a **provider**: tamias
shells `<provider> provision {install,deploy,uninstall} --dir … --name …
[--config …]` and never learns the product's schema.

- `peregrin-bundle` → provider **`peregrin`** (materializes a node config +
  a systemd unit).
- `files` → no provider (the store install is the whole story).

This is generalizing into a first-class **driver system** (`P10`): each
product ships a Rust binary **`tamias-driver-<name>`**, tamias discovers it
on `PATH`/`<tamias-home>/drivers/`, and speaks JSON over stdio
(`capabilities`/`install`/`deploy`/`uninstall`). Installing a product drops
its driver; tamias finds it. No compile-time coupling either way.
