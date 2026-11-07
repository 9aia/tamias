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
