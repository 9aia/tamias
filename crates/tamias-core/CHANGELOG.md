# Changelog

## [0.2.0] - 2026-11-16

### Added

- **sealed:** sealed: packs — age-encrypted ciphertext-only payloads (ADR-0004, P-027) (`c323bdc`)
- declared bins lifecycle — bins[] manifests, tools/bin payloads, tamias bin verbs (`ad841f5`)
- **core:** package metadata — keywords/homepage/repository/license/authors end-to-end (P-006) (`ac340da`)
- **driver:** deploy.d operator config overrides + tamias deploy --set (T-026) (`255f44c`)

### Fixed

- **core:** pack_tgz never emits GNUSparse tar entries (`6383087`)
