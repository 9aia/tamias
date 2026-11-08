---
url: /tamias/explanation/boundaries.md
---
# Boundaries

Tamias owns **content distribution**: refs, transport, the install store

* fetch cache, and content-type dispatch. Every neighbour owns its own
  concern.

| Sibling | Owns | Relationship |
| --- | --- | --- |
| **peregrin** | orchestration | `install/remove/list/…` become thin wrappers over `tamias-*`; peregrin's distribution impl moves here |
| **myco** | work memory | kinds/views/templates ship as a `myco-config` content type |
| **kuda** | code memory | `kuda-domain` packs |
| **mimus** | critic | `mimus-rules` packs |
| **cuora** | secrets | publish tokens / private-registry creds via `cuora://` refs |
| **sturn** | coordination | claim the store lock during concurrent installs |
| **bupha / ci-gate** | CI gate | the first published bundle |
| **sepia** | sessions | — |
| **cicada** | scheduling | `tamias update --all` + registry reindex |

A hosted registry is in scope (it's a `registry` impl). A daemon or
multi-actor arbitration belongs to peregrin.
