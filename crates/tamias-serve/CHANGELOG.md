# Changelog

## [0.3.0] - 2026-11-23

### Added

- **serve:** standalone tamias-serve binary — lean server install, no TUI (`88cd2b5`)
- **serve:** driver.* ops — proxy tamias-driver-* binaries over the wire (`50d546e`)

## [0.2.1] - 2026-11-16

## [0.2.0] - 2026-11-16

### Added

- **sealed:** sealed: packs — age-encrypted ciphertext-only payloads (ADR-0004, P-027) (`c323bdc`)
- **core:** package metadata — keywords/homepage/repository/license/authors end-to-end (P-006) (`ac340da`)
- registry retention — prune old published versions per package (P-024) (`222ac8b`)

### Fixed

- **install:** deploy declared bins[] on every install path (T-027) (`40525cb`)
