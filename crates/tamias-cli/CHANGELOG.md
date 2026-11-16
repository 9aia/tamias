# Changelog

## [0.2.0] - 2026-11-16

### Added

- **install:** agnostic template.extends — spec grammar + multi-parent materialization (P-022) (`e88b0e4`)
- **sealed:** sealed: packs — age-encrypted ciphertext-only payloads (ADR-0004, P-027) (`c323bdc`)
- declared bins lifecycle — bins[] manifests, tools/bin payloads, tamias bin verbs (`ad841f5`)
- **core:** package metadata — keywords/homepage/repository/license/authors end-to-end (P-006) (`ac340da`)
- **install:** template.extends — materialize parent packs at .scarab-parent/ (`5945c02`)
- **cli:** lib-ify the tamias engine + ops surface on tamias-driver-tamias (T-007) (`3010f78`)
- **driver:** deploy.d operator config overrides + tamias deploy --set (T-026) (`255f44c`)
- registry retention — prune old published versions per package (P-024) (`222ac8b`)

### Fixed

- **cli:** forward --help/--version through bin exec + driver exec passthrough (`c86b2b0`)
- **install:** deploy declared bins[] on every install path (T-027) (`40525cb`)
