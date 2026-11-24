# Changelog

## [0.3.0] - 2026-11-23

### Added

- **tui:** async versions section on registry package pages (`e1b7704`)
- **tui:** actionable recently-installed list on the Home pane (`def74d6`)
- **tui:** expandable scrollable op-log overlay (L/:log) (`d4defc0`)
- **tui:** dedupe registry versions, service rows open details, versions-focus scrolls into view (`cee2ea8`)
- **tui:** ex-command line editing, history, dynamic completion, :uninstall (`beaee68`)
- **tui:** versions-focus mode on registry package pages (`80d3b8a`)
- **cli,tui:** tamias prune — explicit store GC for superseded versions (`68bf1d7`)
- **tui:** npm-style package page + command completion (`5644aab`)
- **tui:** actionable empty states and error recovery hints (`533435d`)
- **tui:** home landing pane + registry installed/signed markers (`2c35dab`)
- **tui:** q backs out of package pages, Home recents are clickable, registry drops LICENSE when narrow (`205cad4`)
- **tui:** registries table + selection on the publish pane (`1f3c960`)
- **tui:** dock the op log as a right sidebar, drop the bottom strip (`13e89b7`)
- **tui:** y copies install ref, wider log dock, registry browse state (`06fc09c`)

### Changed

- **tui:** declutter installed/registry tables — drop sha noise columns (`fc31a68`)
- **tui:** contrast palette, PROJECT/PUBLISH pane labels, single hint surface (`d930a93`)

### Fixed

- **tui:** navigation/consistency polish — origin restore, g/G, wheel, dead keys (`f6c9fd7`)
- **tui:** harden key handling, confirms, and job-completion side effects (`8d8d97a`)
- **tui:** accent selection marker on data tables, group services rows by package (`795c614`)
- **tui:** scrollable help overlay, reachable descriptions, wrapped-row scroll math (`741f79c`)
- **tui:** give r a single meaning — x removes, ! restarts (`6230b42`)
- **tui:** dead-key feedback for every pane-gated verb, gate F to Installed, keep search context on Home / (`5aadede`)
- **tui:** key hints only in the status bar + brighter palette (`0ee9d25`)

## [0.2.1] - 2026-11-16

## [0.2.0] - 2026-11-16

### Added

- **sealed:** sealed: packs — age-encrypted ciphertext-only payloads (ADR-0004, P-027) (`c323bdc`)
- **core:** package metadata — keywords/homepage/repository/license/authors end-to-end (P-006) (`ac340da`)
- registry retention — prune old published versions per package (P-024) (`222ac8b`)

### Fixed

- **install:** deploy declared bins[] on every install path (T-027) (`40525cb`)
