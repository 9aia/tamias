# tamias CLI

```
Package & content manager — fetch, store, cache, dispense

Usage: tamias [COMMAND]

Commands:
  install          Fetch → store a package from a ref
  add              Install a ref and record it in `tamias.yaml` + `tamias.lock`
  sync             Reconcile the cwd project's dep set against the store — install what's missing or drifted. Never removes store entries: the store is global, not project-scoped; `tamias remove` stays the uninstall verb
  remove           Drop a store entry (cache untouched)
  list             List installed packages
  info             Show one package's manifest + provenance
  verify           Digest-check a package against its record
  update           Re-resolve + re-install a package from its recorded ref
  pin              Retarget an installed package's recorded source ref
  bin              Link a package's `bin/` payload into the bin dir so bare binary names run — provider `services[].exec` and shells resolve them
  cache            Fetch-cache maintenance
  version          Print the tamias version
  manifest         Print the machine-readable capability manifest
  schema           Print the `tamias.yaml` JSON Schema
  gen-cli-docs     Print the CLI reference (markdown) generated from the parser
  completions      Print a shell completion script
  doctor           Diagnose the host metaservice (store, cache, registry, drivers)
  config           Show the effective config — YAML (`tamias.yml`) is the standard
  serve            Run the ndjson JSON-RPC service over stdio
  publish          Publish a package dir to a registry
  search           Search a registry's index
  bundle           Author-side packing
  registry         Registry operations — named sources in the config file
  deploy-registry  Deploy the registry Worker (Cloudflare)
  deploy           Deploy an installed package's service/runtime (via its provider)
  uninstall        Uninstall — provider teardown + drop the store entry
  service          Manage a per-user service unit (systemd)
  drivers          List discovered content drivers (`tamias-driver-<type>` on PATH + `<tamias-home>/drivers/`)
  driver           Provider-verb dispatch — run a driver verb beyond the lifecycle three (`install`/`deploy`/`uninstall`)
  provider         Provider packages — the declarative `provider:` runtime spec
  run              Run an installed provider service's exec in the foreground — the unit's argv + env, without systemd. `tamias run <pkg> [service] [-- extra-args]`
  logs             Follow a package's provider units' journals — `tamias logs <pkg|unit>`. A package name resolves to all its provider units; anything else is treated as a unit base name
  ui               Interactive package-manager TUI (ratatui) — installed entries, registry search, provider lifecycle, manifest deps, authoring
  help             Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help

  -V, --version
          Print version

```

