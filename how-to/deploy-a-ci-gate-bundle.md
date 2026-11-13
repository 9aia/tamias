# How to deploy a CI gate bundle

A `type: bupha` bundle is a CI fleet shipped as tamias content: the
payload carries a `fleet.toml` (the watched repos + runners) and a
`tamias.yaml` manifest whose `provider:` block declares the systemd
units. This guide stands up the org gate bundle — `ci-gate` from
`9aia/bupha` — end to end. The same steps apply to any `type: bupha`
bundle (e.g. `docs-deploy`).

## Install the driver pack

The bundle's driver, `tamias-driver-bupha`, ships as a payload bin in
the bupha product pack (`libraries/bupha`, `type: files`). Install it
first — the bundle's `deploy`/`hooks`/`health`/`status`/`logs` verbs
all dispatch through it.

The driver binary is a build artifact (`drivers/bin/<triple>/` is
gitignored), so install from a checkout, not a `git:` ref:

```sh
cd 9aia/bupha && ./scripts/pack.sh   # release-builds tamias-driver-bupha
tamias install --allow-drivers ./libraries/bupha
```

(`tamias bundle pack libraries/bupha` produces a `.tgz` for hosts that
shouldn't build — install that ref the same way.) `--allow-drivers`
is the consent gate for packages that ship `drivers/` payloads — see
[Security model](/explanation/security). The driver lands in
`~/.local/share/tamias/drivers/`; the provider units exec it by that
absolute path because systemd's minimal `PATH` can't resolve a bare
`tamias-driver-bupha`.

## Install and deploy the bundle

```sh
tamias install git:9aia/bupha//libraries/ci-gate#main
tamias deploy ci-gate
```

`deploy` does two things:

1. Runs the kind driver's `deploy` verb, which merges the bundle's
   `fleet.toml` with the manifest's `provider.ingress` block and writes
   the operator config to
   `~/.config/bupha/bundles/ci-gate.toml` — one file per bundle, under
   `~/.config/bupha/bundles/`. The fleet itself (`[[repo]]` set,
   `[runner]`, `[status]`, `[worker]`) lives in the payload's
   `fleet.toml`; only the manifest-owned fields (`tunnel.public_url`,
   `trigger.listen`/`kind`) come from `tamias.yaml`.
2. Renders each `provider.services[]` entry as a per-user unit named
   `<provider.name>-<service>` — for ci-gate that's
   `bupha-watch.service` and `bupha-worker.service` (see
   [Provision and service](/how-to/provision-a-service) for the unit
   rendering contract).

## Pick an ingress mode — enable ONE service

ci-gate declares two services, one per ingress mode:

| unit | verb | mode |
| ---- | ---- | ---- |
| `bupha-watch.service` | `tamias-driver-bupha host --config ~/.config/bupha/bundles/ci-gate.toml` | standalone: the bundle's own webhook listener (`provider.ingress`) → queue → pipeline. For hosts with no peregrin. |
| `bupha-worker.service` | `tamias-driver-bupha worker --config ~/.config/bupha/bundles/ci-gate.toml` | substrate: polls `kind="ci-job"` entities on the peregrin node named by `[worker] node` in the generated config. For deployments where the ci-ingress campaign owns webhook ingress. |

Enable exactly one — both at once fight over the listener port and
double-run pushes:

```sh
# standalone mode
tamias service enable bupha-watch

# substrate mode (the org's production shape)
tamias service enable bupha-worker
```

A substrate-mode bundle still declares `provider.ingress` — it is the
standalone fallback and the source of `tunnel.public_url` /
`trigger.listen` in the generated config — but only the worker unit
runs.

## Register the forge hooks

`provider.ingress[].hooks[]` is declarative: `tamias deploy` prints a
hint but never registers live hooks. Registration is an explicit
reviewable step:

```sh
tamias driver exec ci-gate hooks register --dry-run   # show the plan
tamias driver exec ci-gate hooks register             # upsert GitHub webhooks
```

For the substrate mode the webhook URL fronts the peregrin campaign
listener instead — the ci-ingress bundle's own `github-webhook` node
owns `repos[]`, so no `hooks[]` are registered from tamias at all.

## Operate the fleet

```sh
tamias provider list                 # provider: blocks + unit state
tamias driver exec ci-gate health    # JSON probe (listener + unit)
tamias driver exec ci-gate status    # per-unit is-active/is-enabled
tamias driver exec ci-gate logs --lines 50
tamias logs ci-gate                  # journalctl over both units
tamias run ci-gate worker            # foreground exec, no service manager
tamias uninstall ci-gate             # units + bundles/ci-gate.toml + store entry
```

The driver's full verb set is `install`/`deploy`/`uninstall`/`hooks`/
`health`/`status`/`logs`/`host`/`worker`/`config-schema` —
`tamias driver exec` passes the verb through verbatim, and
`tamias driver config-schema ci-gate` prints the JSON Schema of the
operator config `deploy` writes. For the entity-plane worker's
`[worker]` block (`node`, `node_token_env`, `labels`) and the
`runsOn` routing it feeds, see bupha's
[distributed runners](https://github.com/9aia/bupha) docs.

Secrets are env-var **names** only — `secret_env`/`token_env` in
`fleet.toml` resolve env → cuora (`cuora://bupha/<NAME>`) → keyring.
Seed values with `printf '%s' "$VALUE" | cuora put cuora://bupha/<NAME>`;
never write a secret into either file.
