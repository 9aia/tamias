# tamias CLI

```
Package & content manager — fetch, store, cache, dispense

Usage: tamias [COMMAND]

Commands:
  install          Fetch → store a package from a ref
  add              Install a ref and record it in `tamias.yaml` + `tamias.lock`
  remove           Drop a store entry (cache untouched)
  list             List installed packages
  info             Show one package's manifest + provenance
  verify           Digest-check a package against its record
  update           Re-resolve + re-install a package from its recorded ref
  pin              Retarget an installed package's recorded source ref
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
  registry         Registry operations
  deploy-registry  Deploy the registry Worker (Cloudflare)
  deploy           Deploy an installed package's service/runtime (via its provider)
  uninstall        Uninstall — provider teardown + drop the store entry
  service          Manage a per-user service unit (systemd)
  help             Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help

  -V, --version
          Print version

```

