# Changelog

## [0.2.0] - 2026-11-16

### Added

- **install:** agnostic template.extends — spec grammar + multi-parent materialization (P-022) (`e88b0e4`)
- **sealed:** sealed: packs — age-encrypted ciphertext-only payloads (ADR-0004, P-027) (`c323bdc`)
- declared bins lifecycle — bins[] manifests, tools/bin payloads, tamias bin verbs (`ad841f5`)
- **core:** package metadata — keywords/homepage/repository/license/authors end-to-end (P-006) (`ac340da`)
- **install:** template.extends — materialize parent packs at .scarab-parent/ (`5945c02`)

### Fixed

- **uninstall:** skip driver teardown while same-name versions remain (T-029) (`702a469`)
- **install:** deploy declared bins[] on every install path (T-027) (`40525cb`)
